//! Lead Radar backend library.
//!
//! One crate, two binaries (`api`, `worker`): the API serves HTTP, the worker owns the durable
//! job queue, and both share the domain modules, repositories and provider adapters.

pub mod api;
pub mod auth;
pub mod config;
pub mod constants;
pub mod db;
pub mod domain;
pub mod error;
pub mod modules;
pub mod providers;
pub mod seed;
pub mod state;
pub mod worker;

/// Crate version reported by `GET /health` and the OpenAPI document.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
