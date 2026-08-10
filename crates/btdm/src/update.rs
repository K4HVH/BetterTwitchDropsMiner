//! Self-update.
//!
//! Deliberately never automatic. An unattended miner that replaces its own
//! binary mid-run is the single easiest way to turn a working box into a
//! broken one, so this only ever runs when the user asks, and even then it
//! only reports: `btdm update` checks, `btdm update --apply` installs.
//!
//! The downloaded binary is verified against the published SHA-256 before it
//! replaces anything, and the swap is a rename so a failure mid-write cannot
//! leave a truncated executable.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::Path;

const RELEASES_API: &str =
    "https://api.github.com/repos/K4HVH/BetterTwitchDropsMiner/releases/latest";

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("contacting GitHub: {0}")]
    Network(String),
    #[error("no release asset for this platform ({0})")]
    NoAsset(String),
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    Checksum { expected: String, actual: String },
    #[error("the release published no checksum for {0}; refusing to install it")]
    NoChecksum(String),
    #[error("this repository has published no releases yet, so there is nothing to update to")]
    NoReleases,
    #[error("filesystem: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

/// The release asset name this build would install.
///
/// Must match the names produced by the release workflow.
pub fn asset_name() -> String {
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "x86_64"
    };
    format!("btdm-{arch}-unknown-linux-musl")
}

/// Compares two release versions, tolerating a leading `v`.
///
/// Anything that does not parse as `major.minor.patch` is treated as "not
/// newer", so a malformed tag can never trigger an install.
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parse(v: &str) -> Option<(u64, u64, u64)> {
        let v = v.trim().trim_start_matches('v');
        let mut parts = v.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        // Tolerate a pre-release suffix on the patch component.
        let patch_raw = parts.next().unwrap_or("0");
        let patch = patch_raw
            .split(['-', '+'])
            .next()?
            .parse()
            .ok()?;
        Some((major, minor, patch))
    }

    match (parse(latest), parse(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn verify_checksum(bytes: &[u8], expected: &str) -> Result<(), UpdateError> {
    let actual = hex(&Sha256::digest(bytes));
    // The published file is `<sha>  <filename>`, so compare only the digest.
    let expected = expected.split_whitespace().next().unwrap_or("").to_lowercase();
    if expected.is_empty() {
        return Err(UpdateError::NoChecksum(actual));
    }
    if actual == expected {
        Ok(())
    } else {
        Err(UpdateError::Checksum { expected, actual })
    }
}

/// Replaces the running executable.
///
/// Writes beside the target and renames, so the swap is atomic and a partial
/// download can never become the installed binary. On Unix the running process
/// keeps its open image, so the new binary takes effect on the next start.
pub fn install(target: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    let tmp = target.with_extension("new");
    std::fs::write(&tmp, bytes)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(target)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o755);
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
    }

    std::fs::rename(&tmp, target)?;
    Ok(())
}

async fn latest_release(http: &reqwest::Client) -> Result<Release, UpdateError> {
    let resp = http
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;

    // A repository with no published releases answers 404, which is a normal
    // state rather than a failure worth a raw status code.
    if resp.status().as_u16() == 404 {
        return Err(UpdateError::NoReleases);
    }
    if !resp.status().is_success() {
        return Err(UpdateError::Network(format!("HTTP {}", resp.status())));
    }
    resp.json()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))
}

/// Reports what is available, and installs it when `apply` is set.
pub async fn run(apply: bool) -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let http = btdm_twitch::client::build_http_client()?;

    let release = latest_release(&http).await?;
    println!("current: v{current}\nlatest:  {}", release.tag_name);

    if !is_newer(&release.tag_name, current) {
        println!("Already up to date.");
        return Ok(());
    }
    if !apply {
        println!("An update is available. Run `btdm update --apply` to install it.");
        return Ok(());
    }

    let wanted = asset_name();
    let binary = release
        .assets
        .iter()
        .find(|a| a.name == wanted)
        .ok_or_else(|| UpdateError::NoAsset(wanted.clone()))?;
    // An unverified binary is never installed, so a missing checksum is fatal.
    let checksum_asset = release
        .assets
        .iter()
        .find(|a| a.name == format!("{wanted}.sha256"))
        .ok_or_else(|| UpdateError::NoChecksum(wanted.clone()))?;

    println!("Downloading {}…", binary.name);
    let bytes = http
        .get(&binary.browser_download_url)
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?
        .bytes()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;

    let expected = http
        .get(&checksum_asset.browser_download_url)
        .send()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?
        .text()
        .await
        .map_err(|e| UpdateError::Network(e.to_string()))?;

    verify_checksum(&bytes, &expected)?;
    println!("Checksum verified.");

    let exe = std::env::current_exe()?;
    install(&exe, &bytes)?;
    println!(
        "Installed {} to {}.\nRestart the miner to use it.",
        release.tag_name,
        exe.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_higher_version_is_newer() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
    }

    #[test]
    fn the_same_or_older_version_is_not_newer() {
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("v0.0.9", "0.1.0"));
    }

    /// A tag that cannot be parsed must never trigger an install.
    #[test]
    fn an_unparseable_tag_is_never_newer() {
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
        assert!(!is_newer("v9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    #[test]
    fn a_prerelease_suffix_is_tolerated() {
        assert!(is_newer("v0.2.0-rc1", "0.1.0"));
        assert!(!is_newer("v0.1.0-rc1", "0.1.0"));
    }

    #[test]
    fn hex_encoding_is_lowercase_and_zero_padded() {
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn a_matching_checksum_passes() {
        let bytes = b"hello";
        let sha = hex(&Sha256::digest(bytes));
        assert!(verify_checksum(bytes, &sha).is_ok());
        // The published format is `<sha>  <filename>`.
        assert!(verify_checksum(bytes, &format!("{sha}  btdm-x86_64")).is_ok());
        // Case must not matter.
        assert!(verify_checksum(bytes, &sha.to_uppercase()).is_ok());
    }

    #[test]
    fn a_mismatched_checksum_is_rejected() {
        let err = verify_checksum(b"hello", &"0".repeat(64)).unwrap_err();
        assert!(matches!(err, UpdateError::Checksum { .. }));
    }

    /// Installing an unverified binary is the failure mode that turns a
    /// convenience feature into a supply-chain hole.
    #[test]
    fn an_empty_checksum_is_rejected() {
        assert!(matches!(
            verify_checksum(b"hello", "").unwrap_err(),
            UpdateError::NoChecksum(_)
        ));
    }

    #[test]
    fn the_asset_name_matches_the_release_workflow() {
        let name = asset_name();
        assert!(name.starts_with("btdm-"));
        assert!(name.ends_with("-unknown-linux-musl"));
    }

    #[test]
    fn install_replaces_atomically_and_keeps_the_mode() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("btdm");
        std::fs::write(&target, b"old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        install(&target, b"new").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"new");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o755, "the binary must stay executable");
        }

        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "btdm")
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }
}
