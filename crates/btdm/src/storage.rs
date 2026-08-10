//! Runtime state. SQLite rather than JSON because a half-written JSON file is
//! a corrupted config, whereas a half-written transaction is simply rolled back.

use chrono::{DateTime, Utc};
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
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    account TEXT NOT NULL,
    at      TEXT NOT NULL,
    kind    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS events_by_account ON events(account, id DESC);
";

impl Storage {
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
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

    pub fn clear_tokens(&self, account: &str) -> Result<(), StorageError> {
        self.lock()
            .execute("DELETE FROM tokens WHERE account = ?1", params![account])?;
        Ok(())
    }

    pub fn append_event(
        &self,
        account: &str,
        event: &btdm_core::event::MinerEvent,
    ) -> Result<(), StorageError> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO events (account, at, kind) VALUES (?1, ?2, ?3)",
            params![account, event.at.to_rfc3339(), format!("{:?}", event.kind)],
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

    pub fn recent_events(
        &self,
        account: &str,
        limit: u32,
    ) -> Result<Vec<(DateTime<Utc>, String)>, StorageError> {
        let conn = self.lock();
        let mut stmt = conn
            .prepare("SELECT at, kind FROM events WHERE account = ?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = stmt
            .query_map(params![account, limit], |r| {
                let at: String = r.get(0)?;
                let kind: String = r.get(1)?;
                Ok((at, kind))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .filter_map(|(at, kind)| {
                DateTime::parse_from_rfc3339(&at)
                    .ok()
                    .map(|t| (t.with_timezone(&Utc), kind))
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
    fn clearing_tokens_forces_a_fresh_login() {
        let s = Storage::open_in_memory().unwrap();
        s.put_tokens("main", &tokens()).unwrap();
        s.clear_tokens("main").unwrap();
        assert!(s.get_tokens("main").unwrap().is_none());
    }

    #[test]
    fn events_come_back_newest_first_and_respect_the_limit() {
        let s = Storage::open_in_memory().unwrap();
        for i in 0..5 {
            s.append_event("main", &event(i)).unwrap();
        }
        let got = s.recent_events("main", 3).unwrap();
        assert_eq!(got.len(), 3);
        assert!(got[0].0 >= got[1].0, "expected newest first");
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
