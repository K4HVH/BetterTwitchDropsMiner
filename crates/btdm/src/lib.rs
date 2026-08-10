//! The miner binary's internals, exposed as a library.
//!
//! `main.rs` is a thin shell over this. The reason is testing: the wiring in
//! [`run::mine`] — auth, GraphQL, the watch heartbeat, PubSub and the engine,
//! all driven by one loop — is the part that decides what actually happens to
//! a real account, and a binary-only crate cannot be integration-tested at
//! all. Everything here was previously verified by running it and watching.

pub mod auth_flow;
pub mod config;
pub mod control_cli;
pub mod enrol;
pub mod notify;
pub mod run;
pub mod storage;
pub mod supervisor;
pub mod update;
