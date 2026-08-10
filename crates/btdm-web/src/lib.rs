//! The embedded dashboard.
//!
//! The page is a pure consumer of the miner's state: it renders snapshots and
//! streams events. It has no notion of "progress" the miner has not confirmed,
//! which is what stops it from ever showing invented movement.

mod hub;
mod server;

pub use hub::Hub;
pub use server::{ServeError, serve};
