//! The network ban.
//!
//! `btdm-core` holds the mining logic and must reach Twitch only through the
//! `TwitchApi` trait. That is what makes "the stream restarted, progress
//! stalled and the token expired at once" an ordinary unit test rather than
//! something you can only observe in production.
//!
//! The rule is easy to break by accident — one `use reqwest::Client` in a
//! helper and the whole test strategy quietly stops working — so it is checked
//! rather than merely documented. This test previously existed only in the
//! design notes, which is exactly as useful as not having it.

use std::process::Command;

/// Crates that would let the engine talk to the network, touch a disk, or
/// depend on a specific async runtime.
const FORBIDDEN: &[&str] = &[
    "reqwest",
    "hyper",
    "tokio",
    "tokio-tungstenite",
    "tungstenite",
    "rusqlite",
    "libsqlite3-sys",
    "axum",
    "h2",
    "rustls",
    "native-tls",
];

#[test]
fn the_engine_cannot_reach_the_network() {
    let out = Command::new(env!("CARGO"))
        .args([
            "tree",
            "--package",
            "btdm-core",
            "--edges",
            "normal",
            "--prefix",
            "none",
            "--no-dedupe",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("cargo tree should run");

    assert!(
        out.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let tree = String::from_utf8_lossy(&out.stdout);
    let names: Vec<&str> = tree
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .collect();

    let found: Vec<&str> = FORBIDDEN
        .iter()
        .copied()
        .filter(|bad| names.contains(bad))
        .collect();

    assert!(
        found.is_empty(),
        "btdm-core must not depend on {found:?}. The engine talks to Twitch \
         only through the TwitchApi trait; adding an I/O crate here makes the \
         failure paths untestable."
    );
}
