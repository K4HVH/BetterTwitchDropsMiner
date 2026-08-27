//! Runtime state. SQLite rather than JSON because a half-written JSON file is
//! a corrupted config, whereas a half-written transaction is simply rolled back.

// `event_count` and `open_in_memory` are the test seam.
#![allow(dead_code)]

use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::Mutex;

/// The event log is trimmed to this many rows per account on every append
/// batch, so a miner left running for months cannot fill the disk.
pub const EVENT_LOG_LIMIT: u32 = 5_000;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
}

/// One row of the stored event log, ready to hand to the dashboard.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StoredEvent {
    pub at: String,
    pub summary: String,
    pub severity: String,
    /// The original event, as JSON.
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ClaimedDrop {
    pub at: String,
    pub name: String,
    pub drop_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub user_id: String,
    pub login: String,
    /// Stable per install; sent as `X-Device-Id` on auth requests.
    pub device_id: String,
}

pub struct Storage {
    conn: Mutex<Connection>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS tokens (
    account       TEXT PRIMARY KEY,
    access_token  TEXT NOT NULL,
    refresh_token TEXT,
    user_id       TEXT NOT NULL,
    login         TEXT NOT NULL,
    device_id     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    account  TEXT NOT NULL,
    at       TEXT NOT NULL,
    kind     TEXT NOT NULL,
    summary  TEXT NOT NULL DEFAULT '',
    severity TEXT NOT NULL DEFAULT 'info'
);
CREATE INDEX IF NOT EXISTS events_by_account ON events(account, id DESC);
";

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        // ORDERING IS LOAD-BEARING: chmod must happen before `init` enables WAL.
        // SQLite creates the -wal and -shm sidecars with permissions copied from
        // the main database file, so tightening it first means they inherit 0600
        // too. Moving this below `init` would silently leak the token store.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self, StorageError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, StorageError> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        // Databases written before the log became readable have neither
        // column. `ALTER TABLE ... ADD COLUMN` errors if it already exists, so
        // the failure is expected and ignored rather than fatal — an old
        // database must not stop the miner starting.
        for column in [
            "ALTER TABLE events ADD COLUMN summary TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE events ADD COLUMN severity TEXT NOT NULL DEFAULT 'info'",
        ] {
            let _ = conn.execute(column, []);
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn put_tokens(&self, account: &str, t: &Tokens) -> Result<(), StorageError> {
        self.lock().execute(
            "INSERT INTO tokens (account, access_token, refresh_token, user_id, login, device_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account) DO UPDATE SET
                access_token=excluded.access_token,
                refresh_token=excluded.refresh_token,
                user_id=excluded.user_id,
                login=excluded.login,
                device_id=excluded.device_id",
            params![
                account,
                t.access_token,
                t.refresh_token,
                t.user_id,
                t.login,
                t.device_id
            ],
        )?;
        Ok(())
    }

    pub fn get_tokens(&self, account: &str) -> Result<Option<Tokens>, StorageError> {
        let conn = self.lock();
        let row = conn
            .query_row(
                "SELECT access_token, refresh_token, user_id, login, device_id
                 FROM tokens WHERE account = ?1",
                params![account],
                |r| {
                    Ok(Tokens {
                        access_token: r.get(0)?,
                        refresh_token: r.get(1)?,
                        user_id: r.get(2)?,
                        login: r.get(3)?,
                        device_id: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Every account with a stored token, i.e. everything that can be mined.
    pub fn linked_accounts(&self) -> Result<Vec<String>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT account FROM tokens ORDER BY account")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Returns whether a token was actually removed, so the caller cannot
    /// report a purge that never happened.
    pub fn clear_tokens(&self, account: &str) -> Result<bool, StorageError> {
        let removed = self
            .lock()
            .execute("DELETE FROM tokens WHERE account = ?1", params![account])?;
        Ok(removed > 0)
    }

    pub fn append_event(
        &self,
        account: &str,
        event: &btdm_core::event::MinerEvent,
    ) -> Result<(), StorageError> {
        let conn = self.lock();
        // Stored as JSON, not as a Rust debug string. The log used to be
        // written in a form nothing could read back, so a dashboard opened
        // after a restart said "nothing has happened yet" with months of
        // history sitting on disk.
        let kind = serde_json::to_string(&event.kind).unwrap_or_else(|_| "{}".into());
        conn.execute(
            "INSERT INTO events (account, at, kind, summary, severity)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                account,
                event.at.to_rfc3339(),
                kind,
                event.summary(),
                format!("{:?}", event.severity()).to_lowercase(),
            ],
        )?;
        // Keep the log bounded; an unattended miner runs for months.
        conn.execute(
            "DELETE FROM events WHERE account = ?1 AND id <= (
                 SELECT id FROM events WHERE account = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2
             )",
            params![account, EVENT_LOG_LIMIT],
        )?;
        Ok(())
    }

    /// The most recent events for an account, newest first.
    pub fn recent_events(
        &self,
        account: &str,
        limit: u32,
    ) -> Result<Vec<StoredEvent>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT at, summary, severity, kind FROM events
             WHERE account = ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![account, limit], |r| {
                Ok(StoredEvent {
                    at: r.get::<_, String>(0)?,
                    summary: r.get(1)?,
                    severity: r.get(2)?,
                    kind: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every drop this account has claimed, newest first.
    ///
    /// Twitch's inventory only reports campaigns still running, so it is the
    /// one place that cannot answer "what have I actually earned?". The event
    /// log can, and it is already being written.
    pub fn claimed_drops(
        &self,
        account: &str,
        limit: u32,
    ) -> Result<Vec<ClaimedDrop>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT at, kind FROM events
             WHERE account = ?1 AND kind LIKE '%\"drop_claimed\"%'
             ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = stmt
            .query_map(params![account, limit], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows
            .into_iter()
            .filter_map(|(at, kind)| {
                let v: serde_json::Value = serde_json::from_str(&kind).ok()?;
                Some(ClaimedDrop {
                    at,
                    name: v.get("name")?.as_str()?.to_owned(),
                    drop_id: v
                        .get("drop")
                        .and_then(|d| d.as_str())
                        .unwrap_or_default()
                        .to_owned(),
                })
            })
            .collect())
    }

    pub fn event_count(&self, account: &str) -> Result<u32, StorageError> {
        let conn = self.lock();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE account = ?1",
            params![account],
            |r| r.get(0),
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use btdm_core::event::{EventKind, MinerEvent};
    use btdm_core::model::ChannelId;

    fn tokens() -> Tokens {
        Tokens {
            access_token: "at".into(),
            refresh_token: Some("rt".into()),
            user_id: "12345".into(),
            login: "someone".into(),
            device_id: "abc".into(),
        }
    }

    fn event(offset: i64) -> MinerEvent {
        MinerEvent {
            at: Utc::now() + chrono::Duration::seconds(offset),
            kind: EventKind::WatchSent {
                channel: ChannelId::from("c1"),
            },
        }
    }

    #[test]
    fn tokens_round_trip() {
        let s = Storage::open_in_memory().unwrap();
        assert!(s.get_tokens("main").unwrap().is_none());
        s.put_tokens("main", &tokens()).unwrap();
        assert_eq!(s.get_tokens("main").unwrap().unwrap(), tokens());
    }

    #[test]
    fn tokens_are_replaced_not_duplicated() {
        let s = Storage::open_in_memory().unwrap();
        s.put_tokens("main", &tokens()).unwrap();
        let mut updated = tokens();
        updated.access_token = "new".into();
        s.put_tokens("main", &updated).unwrap();
        assert_eq!(s.get_tokens("main").unwrap().unwrap().access_token, "new");
    }

    #[test]
    fn accounts_do_not_share_tokens() {
        let s = Storage::open_in_memory().unwrap();
        s.put_tokens("main", &tokens()).unwrap();
        assert!(s.get_tokens("alt").unwrap().is_none());
    }

    #[test]
    fn linked_accounts_lists_every_stored_token_in_order() {
        let s = Storage::open_in_memory().unwrap();
        assert!(s.linked_accounts().unwrap().is_empty());

        s.put_tokens("zeta", &tokens()).unwrap();
        s.put_tokens("alpha", &tokens()).unwrap();
        assert_eq!(
            s.linked_accounts().unwrap(),
            vec!["alpha".to_string(), "zeta".to_string()]
        );
    }

    #[test]
    fn clearing_tokens_forces_a_fresh_login() {
        let s = Storage::open_in_memory().unwrap();
        s.put_tokens("main", &tokens()).unwrap();
        assert!(s.clear_tokens("main").unwrap(), "a token was removed");
        assert!(s.get_tokens("main").unwrap().is_none());
    }

    /// Reporting a purge that never happened is the same class of lie as a
    /// progress bar that advances without progress.
    #[test]
    fn clearing_an_unknown_account_reports_that_nothing_was_removed() {
        let s = Storage::open_in_memory().unwrap();
        assert!(!s.clear_tokens("never-existed").unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn wal_sidecar_files_are_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let s = Storage::open(&path).unwrap();
        // Force a write so the WAL definitely exists.
        s.put_tokens("main", &tokens()).unwrap();

        for suffix in ["-wal", "-shm"] {
            let side = path.with_file_name(format!("state.db{suffix}"));
            if let Ok(meta) = std::fs::metadata(&side) {
                assert_eq!(
                    meta.permissions().mode() & 0o777,
                    0o600,
                    "{suffix} sidecar leaks the token store"
                );
            }
        }
    }

    #[test]
    fn events_come_back_newest_first_and_respect_the_limit() {
        let s = Storage::open_in_memory().unwrap();
        for i in 0..5 {
            s.append_event("main", &event(i)).unwrap();
        }
        let got = s.recent_events("main", 3).unwrap();
        assert_eq!(got.len(), 3);
        assert!(got[0].at >= got[1].at, "expected newest first");
    }

    #[test]
    fn the_event_log_is_capped() {
        let s = Storage::open_in_memory().unwrap();
        for i in 0..(EVENT_LOG_LIMIT as i64 + 50) {
            s.append_event("main", &event(i)).unwrap();
        }
        assert_eq!(
            s.event_count("main").unwrap(),
            EVENT_LOG_LIMIT,
            "an unattended miner must not grow the log without bound"
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        let _s = Storage::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the DB holds live OAuth tokens");
    }
}

#[cfg(test)]
mod history_tests {
    use super::*;
    use chrono::Utc;
    use btdm_core::event::{EventKind, MinerEvent};
    use btdm_core::model::{ChannelId, DropId};

    fn event(kind: EventKind) -> MinerEvent {
        MinerEvent {
            at: Utc::now(),
            kind,
        }
    }

    /// The log used to be written as a Rust debug string, so nothing could
    /// read it back and the dashboard reported "nothing has happened yet"
    /// with months of history on disk.
    #[test]
    fn stored_events_come_back_readable() {
        let s = Storage::open_in_memory().unwrap();
        s.append_event(
            "main",
            &event(EventKind::CampaignsSynced {
                total: 86,
                eligible: 16,
                unchecked: 0,
            }),
        )
        .unwrap();

        let got = s.recent_events("main", 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].summary, "Synced 86 campaigns, 16 mineable");
        assert_eq!(got[0].severity, "info");
        // And the structured form survives too.
        let parsed: serde_json::Value = serde_json::from_str(&got[0].kind).unwrap();
        assert_eq!(parsed["type"], "campaigns_synced");
        assert_eq!(parsed["total"], 86);
    }

    #[test]
    fn severity_is_recorded_so_the_log_can_be_coloured_after_a_restart() {
        let s = Storage::open_in_memory().unwrap();
        s.append_event(
            "main",
            &event(EventKind::DropClaimed {
                drop: DropId::from("d1"),
                name: "Weapon Charm".into(),
            }),
        )
        .unwrap();
        assert_eq!(s.recent_events("main", 1).unwrap()[0].severity, "good");
    }

    /// "What have I actually earned?" is the question Twitch's own inventory
    /// cannot answer, because it only reports campaigns still running.
    #[test]
    fn claimed_drops_are_recoverable_from_the_log() {
        let s = Storage::open_in_memory().unwrap();
        for name in ["Weapon Charm", "Golden Skin"] {
            s.append_event(
                "main",
                &event(EventKind::DropClaimed {
                    drop: DropId::from("d"),
                    name: name.into(),
                }),
            )
            .unwrap();
        }
        // Noise that must not appear in the reward list.
        s.append_event(
            "main",
            &event(EventKind::WatchSent {
                channel: ChannelId::from("c1"),
            }),
        )
        .unwrap();

        let claimed = s.claimed_drops("main", 50).unwrap();
        assert_eq!(claimed.len(), 2);
        assert_eq!(claimed[0].name, "Golden Skin", "newest first");
        assert_eq!(claimed[1].name, "Weapon Charm");
    }

    #[test]
    fn one_accounts_history_is_not_visible_to_another() {
        let s = Storage::open_in_memory().unwrap();
        s.append_event(
            "main",
            &event(EventKind::DropClaimed {
                drop: DropId::from("d"),
                name: "Mine".into(),
            }),
        )
        .unwrap();
        assert!(s.claimed_drops("other", 10).unwrap().is_empty());
        assert!(s.recent_events("other", 10).unwrap().is_empty());
    }
}
