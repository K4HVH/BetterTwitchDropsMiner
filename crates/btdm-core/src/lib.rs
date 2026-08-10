//! Domain model and mining engine.
//!
//! This crate contains no I/O by design: it talks to Twitch only through the
//! [`api::TwitchApi`] trait. That is what makes the failure paths testable —
//! "the stream restarted, progress stalled, and the token expired at the same
//! moment" is an ordinary unit test here rather than something you can only
//! observe in production.

pub mod api;
pub mod eligibility;
pub mod engine;
pub mod event;
pub mod model;
pub mod progress;
pub mod strategy;
pub mod watchdog;
