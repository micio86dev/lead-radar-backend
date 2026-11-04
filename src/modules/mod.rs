//! Domain modules. Each owns its models, SQL and rules; HTTP handlers stay thin.

pub mod benchmark;
pub mod comments;
pub mod decision;
pub mod engagement;
pub mod generative;
pub mod ingestion;
pub mod operations;
pub mod projects;
pub mod qualification;
pub mod replies;
pub mod sources;
pub mod tenancy;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// Stable, time-ordered identifiers (UUIDv7).
pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

pub fn to_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Parse a JSON column. Corrupt data becomes `None` instead of panicking a request.
pub fn from_json<T: DeserializeOwned>(raw: &str) -> Option<T> {
    serde_json::from_str(raw).ok()
}

pub fn from_json_or<T: DeserializeOwned>(raw: &str, default: T) -> T {
    from_json(raw).unwrap_or(default)
}
