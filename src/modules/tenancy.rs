//! Tenants, users, memberships, API tokens and per-tenant settings.

use serde_json::Value;
use std::collections::BTreeMap;

use crate::db::Db;
use crate::domain::enums::Role;
use crate::domain::time::now_iso;
use crate::modules::{new_id, to_json};

pub struct TenantRecord {
    pub id: String,
    pub name: String,
    pub slug: String,
}

/// Insert a tenant if it does not exist yet; returns the id.
pub async fn ensure_tenant(
    db: &Db,
    tenant_id: &str,
    name: &str,
    slug: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tenants (id, name, slug, created_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(name)
    .bind(slug)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

pub async fn ensure_user(
    db: &Db,
    user_id: &str,
    email: &str,
    display_name: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO users (id, email, display_name, created_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(user_id)
    .bind(email)
    .bind(display_name)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

pub async fn ensure_membership(
    db: &Db,
    tenant_id: &str,
    user_id: &str,
    role: Role,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO tenant_users (tenant_id, user_id, role, created_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(tenant_id, user_id) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(user_id)
    .bind(role.as_str())
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

/// Store a hashed token. Returns true when a new token row was created.
pub async fn ensure_api_token(
    db: &Db,
    tenant_id: &str,
    user_id: &str,
    token_hash: &str,
    label: &str,
) -> Result<bool, sqlx::Error> {
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM api_tokens WHERE token_hash = ?")
            .bind(token_hash)
            .fetch_optional(&db.reader)
            .await?;
    if existing.is_some() {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO api_tokens (id, tenant_id, user_id, token_hash, label, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(user_id)
    .bind(token_hash)
    .bind(label)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(true)
}

pub async fn tenant_name(db: &Db, tenant_id: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT name FROM tenants WHERE id = ?")
        .bind(tenant_id)
        .fetch_optional(&db.reader)
        .await
}

pub async fn get_setting(
    db: &Db,
    tenant_id: &str,
    key: &str,
) -> Result<Option<Value>, sqlx::Error> {
    let raw: Option<String> =
        sqlx::query_scalar("SELECT value FROM app_settings WHERE tenant_id = ? AND key = ?")
            .bind(tenant_id)
            .bind(key)
            .fetch_optional(&db.reader)
            .await?;
    Ok(raw.and_then(|value| serde_json::from_str(&value).ok()))
}

pub async fn set_setting(
    db: &Db,
    tenant_id: &str,
    key: &str,
    value: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO app_settings (tenant_id, key, value, updated_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(tenant_id, key) DO UPDATE SET value = excluded.value, \
         updated_at = excluded.updated_at",
    )
    .bind(tenant_id)
    .bind(key)
    .bind(to_json(value))
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

pub async fn all_settings(
    db: &Db,
    tenant_id: &str,
) -> Result<BTreeMap<String, Value>, sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM app_settings WHERE tenant_id = ? ORDER BY key")
            .bind(tenant_id)
            .fetch_all(&db.reader)
            .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(key, raw)| {
            serde_json::from_str::<Value>(&raw)
                .ok()
                .map(|value| (key, value))
        })
        .collect())
}

/// Provider enable flags recorded by the settings endpoint. `None` means "not configured here", so
/// the environment selection remains in force.
pub async fn provider_toggle(
    db: &Db,
    tenant_id: &str,
    provider: &str,
) -> Result<Option<bool>, sqlx::Error> {
    let settings = get_setting(db, tenant_id, "providers").await?;
    Ok(settings.and_then(|value| {
        value
            .get(format!("{provider}_enabled"))
            .and_then(|flag| flag.as_bool())
    }))
}
