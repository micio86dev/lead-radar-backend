//! Sources, Actor registry, communities, search profiles, keywords and Apify run bookkeeping.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;

use crate::config::AppConfig;
use crate::db::Db;
use crate::domain::enums::{ValidationStatus, Vertical};
use crate::domain::filtering::KeywordSet;
use crate::domain::time::now_iso;
use crate::error::CostGuardReason;
use crate::modules::operations::{self, CostGuardInput};
use crate::modules::{from_json_or, new_id, to_json};

// ---------------------------------------------------------------------------------------------
// Search profiles and keywords
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchProfile {
    pub id: String,
    pub name: String,
    pub vertical: String,
    pub enabled: bool,
    pub keywords: Vec<String>,
    pub negative_keywords: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(FromRow)]
struct SearchProfileRow {
    id: String,
    name: String,
    vertical: String,
    enabled: i64,
    created_at: String,
    updated_at: String,
}

async fn profile_from_row(db: &Db, tenant_id: &str, row: SearchProfileRow) -> SearchProfile {
    let vertical = row.vertical.clone();
    SearchProfile {
        id: row.id,
        name: row.name,
        vertical: row.vertical,
        enabled: row.enabled != 0,
        keywords: keywords_for(db, tenant_id, &vertical, "positive").await,
        negative_keywords: negative_keywords_for(db, tenant_id, &vertical).await,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

pub async fn keywords_for(db: &Db, tenant_id: &str, vertical: &str, polarity: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT term FROM keywords WHERE tenant_id = ? AND vertical = ? AND polarity = ? \
         ORDER BY term",
    )
    .bind(tenant_id)
    .bind(vertical)
    .bind(polarity)
    .fetch_all(&db.reader)
    .await
    .unwrap_or_default()
}

/// Negative keywords apply per vertical plus the global `all` set.
pub async fn negative_keywords_for(db: &Db, tenant_id: &str, vertical: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT term FROM keywords WHERE tenant_id = ? AND polarity = 'negative' \
         AND (vertical = ? OR vertical = 'all') ORDER BY term",
    )
    .bind(tenant_id)
    .bind(vertical)
    .fetch_all(&db.reader)
    .await
    .unwrap_or_default()
}

/// Every configured keyword, used by the pre-decision filter.
pub async fn keyword_set(db: &Db, tenant_id: &str) -> KeywordSet {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT vertical, term, polarity FROM keywords WHERE tenant_id = ? ORDER BY vertical, term",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await
    .unwrap_or_default();

    let mut set = KeywordSet::default();
    for (vertical, term, polarity) in rows {
        if polarity == "negative" {
            set.add_negative(&term);
        } else if let Some(vertical) = Vertical::parse(&vertical) {
            set.add_positive(vertical, &term);
        }
    }
    set
}

pub async fn list_search_profiles(
    db: &Db,
    tenant_id: &str,
) -> Result<Vec<SearchProfile>, sqlx::Error> {
    let rows = sqlx::query_as::<_, SearchProfileRow>(
        "SELECT id, name, vertical, enabled, created_at, updated_at FROM search_profiles \
         WHERE tenant_id = ? ORDER BY vertical, name",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await?;
    let mut profiles = Vec::with_capacity(rows.len());
    for row in rows {
        profiles.push(profile_from_row(db, tenant_id, row).await);
    }
    Ok(profiles)
}

pub async fn get_search_profile(
    db: &Db,
    tenant_id: &str,
    id: &str,
) -> Result<Option<SearchProfile>, sqlx::Error> {
    let row = sqlx::query_as::<_, SearchProfileRow>(
        "SELECT id, name, vertical, enabled, created_at, updated_at FROM search_profiles \
         WHERE tenant_id = ? AND id = ?",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&db.reader)
    .await?;
    match row {
        Some(row) => Ok(Some(profile_from_row(db, tenant_id, row).await)),
        None => Ok(None),
    }
}

pub struct NewSearchProfile<'a> {
    pub name: &'a str,
    pub vertical: Vertical,
    pub enabled: bool,
    pub keywords: &'a [String],
    pub negative_keywords: &'a [String],
}

pub async fn create_search_profile(
    db: &Db,
    tenant_id: &str,
    input: &NewSearchProfile<'_>,
) -> Result<SearchProfile, sqlx::Error> {
    let id = new_id();
    let now = now_iso();
    let mut tx = db.writer.begin().await?;
    sqlx::query(
        "INSERT INTO search_profiles (id, tenant_id, name, vertical, enabled, created_at, \
         updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(input.name)
    .bind(input.vertical.as_str())
    .bind(if input.enabled { 1 } else { 0 })
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    for term in input.keywords {
        insert_keyword(
            &mut tx,
            tenant_id,
            input.vertical.as_str(),
            term,
            "positive",
        )
        .await?;
    }
    for term in input.negative_keywords {
        insert_keyword(
            &mut tx,
            tenant_id,
            input.vertical.as_str(),
            term,
            "negative",
        )
        .await?;
    }
    tx.commit().await?;
    get_search_profile(db, tenant_id, &id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

async fn insert_keyword(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    tenant_id: &str,
    vertical: &str,
    term: &str,
    polarity: &str,
) -> Result<(), sqlx::Error> {
    let term = term.trim();
    if term.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO keywords (id, tenant_id, vertical, term, polarity, created_at) \
         VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(tenant_id, vertical, term, polarity) DO NOTHING",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(vertical)
    .bind(term)
    .bind(polarity)
    .bind(now_iso())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub struct ProfilePatch<'a> {
    pub name: Option<&'a str>,
    pub enabled: Option<bool>,
    pub keywords: Option<&'a [String]>,
    pub negative_keywords: Option<&'a [String]>,
}

pub async fn update_search_profile(
    db: &Db,
    tenant_id: &str,
    id: &str,
    patch: &ProfilePatch<'_>,
) -> Result<Option<SearchProfile>, sqlx::Error> {
    let existing = get_search_profile(db, tenant_id, id).await?;
    let Some(existing) = existing else {
        return Ok(None);
    };
    let now = now_iso();
    let mut tx = db.writer.begin().await?;
    if let Some(name) = patch.name {
        sqlx::query(
            "UPDATE search_profiles SET name = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(name)
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(enabled) = patch.enabled {
        sqlx::query(
            "UPDATE search_profiles SET enabled = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(if enabled { 1 } else { 0 })
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(keywords) = patch.keywords {
        sqlx::query(
            "DELETE FROM keywords WHERE tenant_id = ? AND vertical = ? AND polarity = 'positive'",
        )
        .bind(tenant_id)
        .bind(&existing.vertical)
        .execute(&mut *tx)
        .await?;
        for term in keywords {
            insert_keyword(&mut tx, tenant_id, &existing.vertical, term, "positive").await?;
        }
    }
    if let Some(keywords) = patch.negative_keywords {
        sqlx::query(
            "DELETE FROM keywords WHERE tenant_id = ? AND vertical = ? AND polarity = 'negative'",
        )
        .bind(tenant_id)
        .bind(&existing.vertical)
        .execute(&mut *tx)
        .await?;
        for term in keywords {
            insert_keyword(&mut tx, tenant_id, &existing.vertical, term, "negative").await?;
        }
    }
    sqlx::query("UPDATE search_profiles SET updated_at = ? WHERE id = ? AND tenant_id = ?")
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    get_search_profile(db, tenant_id, id).await
}

pub async fn delete_search_profile(
    db: &Db,
    tenant_id: &str,
    id: &str,
) -> Result<bool, sqlx::Error> {
    let profile = get_search_profile(db, tenant_id, id).await?;
    let Some(profile) = profile else {
        return Ok(false);
    };
    let mut tx = db.writer.begin().await?;
    sqlx::query("DELETE FROM keywords WHERE tenant_id = ? AND vertical = ?")
        .bind(tenant_id)
        .bind(&profile.vertical)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM search_profiles WHERE id = ? AND tenant_id = ?")
        .bind(id)
        .bind(tenant_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

// ---------------------------------------------------------------------------------------------
// Communities
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Community {
    pub id: String,
    pub name: String,
    pub vertical: String,
    pub validation_status: String,
    pub enabled: bool,
    pub notes: Option<String>,
    pub last_checked_at: Option<String>,
    pub created_at: String,
}

#[derive(FromRow)]
struct CommunityRow {
    id: String,
    name: String,
    vertical: String,
    validation_status: String,
    enabled: i64,
    notes: Option<String>,
    last_checked_at: Option<String>,
    created_at: String,
}

impl From<CommunityRow> for Community {
    fn from(row: CommunityRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            vertical: row.vertical,
            validation_status: row.validation_status,
            enabled: row.enabled != 0,
            notes: row.notes,
            last_checked_at: row.last_checked_at,
            created_at: row.created_at,
        }
    }
}

pub async fn list_communities(
    db: &Db,
    tenant_id: &str,
    vertical: Option<&str>,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<Community>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, CommunityRow>(
        "SELECT id, name, vertical, validation_status, enabled, notes, last_checked_at, created_at \
         FROM communities WHERE tenant_id = ? AND (? IS NULL OR vertical = ?) \
         AND (? IS NULL OR validation_status = ?) ORDER BY vertical, name LIMIT ? OFFSET ?",
    )
    .bind(tenant_id)
    .bind(vertical)
    .bind(vertical)
    .bind(status)
    .bind(status)
    .bind(limit)
    .bind(offset)
    .fetch_all(&db.reader)
    .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM communities WHERE tenant_id = ? \
         AND (? IS NULL OR vertical = ?) AND (? IS NULL OR validation_status = ?)",
    )
    .bind(tenant_id)
    .bind(vertical)
    .bind(vertical)
    .bind(status)
    .bind(status)
    .fetch_one(&db.reader)
    .await?;
    Ok((rows.into_iter().map(Community::from).collect(), total))
}

pub async fn get_community(
    db: &Db,
    tenant_id: &str,
    id: &str,
) -> Result<Option<Community>, sqlx::Error> {
    let row = sqlx::query_as::<_, CommunityRow>(
        "SELECT id, name, vertical, validation_status, enabled, notes, last_checked_at, created_at \
         FROM communities WHERE tenant_id = ? AND id = ?",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&db.reader)
    .await?;
    Ok(row.map(Community::from))
}

pub struct CommunityPatch<'a> {
    pub enabled: Option<bool>,
    pub notes: Option<&'a str>,
}

pub async fn patch_community(
    db: &Db,
    tenant_id: &str,
    id: &str,
    patch: &CommunityPatch<'_>,
) -> Result<Option<Community>, sqlx::Error> {
    if let Some(enabled) = patch.enabled {
        sqlx::query(
            "UPDATE communities SET enabled = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(if enabled { 1 } else { 0 })
        .bind(now_iso())
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(notes) = patch.notes {
        sqlx::query(
            "UPDATE communities SET notes = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(notes)
        .bind(now_iso())
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    get_community(db, tenant_id, id).await
}

/// Record a validation outcome. Enabling stays a separate, explicit step.
pub async fn validate_community(
    db: &Db,
    tenant_id: &str,
    id: &str,
    outcome: ValidationStatus,
    notes: Option<&str>,
) -> Result<Option<Community>, sqlx::Error> {
    let now = now_iso();
    let result = sqlx::query(
        "UPDATE communities SET validation_status = ?, last_checked_at = ?, \
         notes = COALESCE(?, notes), updated_at = ? WHERE id = ? AND tenant_id = ?",
    )
    .bind(outcome.as_str())
    .bind(&now)
    .bind(notes)
    .bind(&now)
    .bind(id)
    .bind(tenant_id)
    .execute(&db.writer)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    get_community(db, tenant_id, id).await
}

// ---------------------------------------------------------------------------------------------
// Actor registry
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub provider: String,
    pub actor_id: String,
    pub name: String,
    pub description: String,
    pub source_type: String,
    pub enabled: bool,
    pub validation_status: String,
    pub input_template: Value,
    pub max_items_per_run: i64,
    pub cost_per_run_estimate: Option<f64>,
    /// Provider-side per-run cap in USD, passed to Apify as `maxTotalChargeUsd`.
    pub max_total_charge_usd: Option<f64>,
    pub last_success_at: Option<String>,
    pub created_at: String,
}

#[derive(FromRow)]
struct ActorRow {
    id: String,
    provider: String,
    actor_id: String,
    name: String,
    description: String,
    source_type: String,
    enabled: i64,
    validation_status: String,
    input_template: String,
    max_items_per_run: i64,
    cost_per_run_estimate: Option<f64>,
    max_total_charge_usd: Option<f64>,
    last_success_at: Option<String>,
    created_at: String,
}

impl From<ActorRow> for Actor {
    fn from(row: ActorRow) -> Self {
        Self {
            id: row.id,
            provider: row.provider,
            actor_id: row.actor_id,
            name: row.name,
            description: row.description,
            source_type: row.source_type,
            enabled: row.enabled != 0,
            validation_status: row.validation_status,
            input_template: from_json_or(&row.input_template, Value::Object(Default::default())),
            max_items_per_run: row.max_items_per_run,
            cost_per_run_estimate: row.cost_per_run_estimate,
            max_total_charge_usd: row.max_total_charge_usd,
            last_success_at: row.last_success_at,
            created_at: row.created_at,
        }
    }
}

pub async fn list_actors(db: &Db, tenant_id: &str) -> Result<Vec<Actor>, sqlx::Error> {
    let rows = sqlx::query_as::<_, ActorRow>(
        "SELECT * FROM actor_registry WHERE tenant_id = ? ORDER BY name",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await?;
    Ok(rows.into_iter().map(Actor::from).collect())
}

pub async fn get_actor(db: &Db, tenant_id: &str, id: &str) -> Result<Option<Actor>, sqlx::Error> {
    let row = sqlx::query_as::<_, ActorRow>(
        "SELECT * FROM actor_registry WHERE tenant_id = ? AND id = ?",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&db.reader)
    .await?;
    Ok(row.map(Actor::from))
}

pub struct NewActor<'a> {
    pub actor_id: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub source_type: &'a str,
    pub input_template: &'a Value,
    pub max_items_per_run: i64,
    pub cost_per_run_estimate: Option<f64>,
    pub max_total_charge_usd: Option<f64>,
}

/// The Actor id is stored exactly as supplied: validation is a human step, never a guess.
pub async fn create_actor(
    db: &Db,
    tenant_id: &str,
    input: &NewActor<'_>,
) -> Result<Actor, sqlx::Error> {
    let id = new_id();
    let now = now_iso();
    sqlx::query(
        "INSERT INTO actor_registry (id, tenant_id, provider, actor_id, name, description, \
         source_type, enabled, validation_status, input_template, max_items_per_run, \
         cost_per_run_estimate, max_total_charge_usd, created_at, updated_at) \
         VALUES (?, ?, 'apify', ?, ?, ?, ?, 0, 'pending', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(input.actor_id.trim())
    .bind(input.name)
    .bind(input.description)
    .bind(input.source_type)
    .bind(to_json(input.input_template))
    .bind(input.max_items_per_run.max(1))
    .bind(input.cost_per_run_estimate)
    .bind(input.max_total_charge_usd)
    .bind(&now)
    .bind(&now)
    .execute(&db.writer)
    .await?;
    get_actor(db, tenant_id, &id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

pub async fn set_actor_validation(
    db: &Db,
    tenant_id: &str,
    id: &str,
    status: ValidationStatus,
    enabled: bool,
) -> Result<Option<Actor>, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE actor_registry SET validation_status = ?, enabled = ?, updated_at = ? \
         WHERE id = ? AND tenant_id = ?",
    )
    .bind(status.as_str())
    .bind(if enabled { 1 } else { 0 })
    .bind(now_iso())
    .bind(id)
    .bind(tenant_id)
    .execute(&db.writer)
    .await?;
    if result.rows_affected() == 0 {
        return Ok(None);
    }
    get_actor(db, tenant_id, id).await
}

pub struct ActorPatch<'a> {
    pub enabled: Option<bool>,
    pub validation_status: Option<ValidationStatus>,
    pub description: Option<&'a str>,
    pub max_items_per_run: Option<i64>,
    pub max_total_charge_usd: Option<f64>,
    pub input_template: Option<&'a Value>,
}

pub async fn update_actor(
    db: &Db,
    tenant_id: &str,
    id: &str,
    patch: &ActorPatch<'_>,
) -> Result<Option<Actor>, sqlx::Error> {
    let now = now_iso();
    if let Some(validation) = patch.validation_status {
        sqlx::query(
            "UPDATE actor_registry SET validation_status = ?, updated_at = ?              WHERE id = ? AND tenant_id = ?",
        )
        .bind(validation.as_str())
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(enabled) = patch.enabled {
        sqlx::query(
            "UPDATE actor_registry SET enabled = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(if enabled { 1 } else { 0 })
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(description) = patch.description {
        sqlx::query(
            "UPDATE actor_registry SET description = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(description)
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(items) = patch.max_items_per_run {
        sqlx::query(
            "UPDATE actor_registry SET max_items_per_run = ?, updated_at = ?              WHERE id = ? AND tenant_id = ?",
        )
        .bind(items.max(1))
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(cap) = patch.max_total_charge_usd {
        sqlx::query(
            "UPDATE actor_registry SET max_total_charge_usd = ?, updated_at = ? \
             WHERE id = ? AND tenant_id = ?",
        )
        .bind(cap)
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(template) = patch.input_template {
        sqlx::query(
            "UPDATE actor_registry SET input_template = ?, updated_at = ?              WHERE id = ? AND tenant_id = ?",
        )
        .bind(to_json(template))
        .bind(&now)
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    get_actor(db, tenant_id, id).await
}

pub async fn mark_actor_success(db: &Db, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE actor_registry SET last_success_at = ?, updated_at = ? WHERE id = ?")
        .bind(now_iso())
        .bind(now_iso())
        .bind(id)
        .execute(&db.writer)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub source_type: String,
    pub enabled: bool,
    pub validation_status: String,
    pub notes: Option<String>,
    pub config: Value,
    pub last_run_at: Option<String>,
    pub created_at: String,
}

#[derive(FromRow)]
struct SourceRow {
    id: String,
    name: String,
    provider: String,
    source_type: String,
    enabled: i64,
    validation_status: String,
    notes: Option<String>,
    config: String,
    last_run_at: Option<String>,
    created_at: String,
}

impl From<SourceRow> for Source {
    fn from(row: SourceRow) -> Self {
        Self {
            id: row.id,
            name: row.name,
            provider: row.provider,
            source_type: row.source_type,
            enabled: row.enabled != 0,
            validation_status: row.validation_status,
            notes: row.notes,
            config: from_json_or(&row.config, Value::Object(Default::default())),
            last_run_at: row.last_run_at,
            created_at: row.created_at,
        }
    }
}

pub async fn list_sources(db: &Db, tenant_id: &str) -> Result<Vec<Source>, sqlx::Error> {
    let rows = sqlx::query_as::<_, SourceRow>(
        "SELECT id, name, provider, source_type, enabled, validation_status, notes, config, \
         last_run_at, created_at FROM sources WHERE tenant_id = ? ORDER BY name",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await?;
    Ok(rows.into_iter().map(Source::from).collect())
}

pub async fn get_source(db: &Db, tenant_id: &str, id: &str) -> Result<Option<Source>, sqlx::Error> {
    let row = sqlx::query_as::<_, SourceRow>(
        "SELECT id, name, provider, source_type, enabled, validation_status, notes, config, \
         last_run_at, created_at FROM sources WHERE tenant_id = ? AND id = ?",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&db.reader)
    .await?;
    Ok(row.map(Source::from))
}

pub async fn find_source_by_name(
    db: &Db,
    tenant_id: &str,
    name: &str,
) -> Result<Option<Source>, sqlx::Error> {
    let row = sqlx::query_as::<_, SourceRow>(
        "SELECT id, name, provider, source_type, enabled, validation_status, notes, config, \
         last_run_at, created_at FROM sources WHERE tenant_id = ? AND name = ?",
    )
    .bind(tenant_id)
    .bind(name)
    .fetch_optional(&db.reader)
    .await?;
    Ok(row.map(Source::from))
}

pub struct NewSource<'a> {
    pub name: &'a str,
    pub provider: &'a str,
    pub source_type: &'a str,
    pub config: &'a Value,
}

pub async fn create_source(
    db: &Db,
    tenant_id: &str,
    input: &NewSource<'_>,
) -> Result<Source, sqlx::Error> {
    let id = new_id();
    let now = now_iso();
    sqlx::query(
        "INSERT INTO sources (id, tenant_id, name, provider, source_type, enabled, \
         validation_status, config, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, 0, 'pending', ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(input.name.trim())
    .bind(input.provider)
    .bind(input.source_type)
    .bind(to_json(input.config))
    .bind(&now)
    .bind(&now)
    .execute(&db.writer)
    .await?;
    get_source(db, tenant_id, &id)
        .await?
        .ok_or(sqlx::Error::RowNotFound)
}

pub struct SourcePatch<'a> {
    pub enabled: Option<bool>,
    pub config: Option<&'a Value>,
}

pub async fn patch_source(
    db: &Db,
    tenant_id: &str,
    id: &str,
    patch: &SourcePatch<'_>,
) -> Result<Option<Source>, sqlx::Error> {
    if let Some(enabled) = patch.enabled {
        sqlx::query(
            "UPDATE sources SET enabled = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(if enabled { 1 } else { 0 })
        .bind(now_iso())
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(config) = patch.config {
        sqlx::query("UPDATE sources SET config = ?, updated_at = ? WHERE id = ? AND tenant_id = ?")
            .bind(to_json(config))
            .bind(now_iso())
            .bind(id)
            .bind(tenant_id)
            .execute(&db.writer)
            .await?;
    }
    get_source(db, tenant_id, id).await
}

/// Record a validation outcome for a source. Enabling stays a separate, explicit step.
pub async fn validate_source(
    db: &Db,
    tenant_id: &str,
    id: &str,
    outcome: ValidationStatus,
    notes: Option<&str>,
) -> Result<Option<Source>, sqlx::Error> {
    let now = now_iso();
    let updated = sqlx::query(
        "UPDATE sources SET validation_status = ?, notes = COALESCE(?, notes), updated_at = ? \
         WHERE id = ? AND tenant_id = ?",
    )
    .bind(outcome.as_str())
    .bind(notes)
    .bind(&now)
    .bind(id)
    .bind(tenant_id)
    .execute(&db.writer)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    get_source(db, tenant_id, id).await
}

pub async fn mark_source_run(db: &Db, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sources SET last_run_at = ?, updated_at = ? WHERE id = ?")
        .bind(now_iso())
        .bind(now_iso())
        .bind(id)
        .execute(&db.writer)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Apify runs
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApifyRun {
    pub id: String,
    pub actor_registry_id: String,
    pub actor_name: String,
    pub status: String,
    pub items_count: i64,
    pub dataset_id: Option<String>,
    pub cost_estimate_usd: Option<f64>,
    pub cost_actual_usd: Option<f64>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub job_id: Option<String>,
    pub created_at: String,
}

#[derive(FromRow)]
struct ApifyRunRow {
    id: String,
    actor_registry_id: String,
    actor_name: Option<String>,
    status: String,
    items_count: i64,
    dataset_id: Option<String>,
    cost_estimate_eur: Option<f64>,
    cost_actual_eur: Option<f64>,
    error: Option<String>,
    started_at: Option<String>,
    finished_at: Option<String>,
    job_id: Option<String>,
    created_at: String,
}

impl From<ApifyRunRow> for ApifyRun {
    fn from(row: ApifyRunRow) -> Self {
        Self {
            id: row.id,
            actor_registry_id: row.actor_registry_id,
            actor_name: row
                .actor_name
                .unwrap_or_else(|| "unknown actor".to_string()),
            status: row.status,
            items_count: row.items_count,
            dataset_id: row.dataset_id,
            // Apify reports spend in USD; the columns keep the provider currency.
            cost_estimate_usd: row.cost_estimate_eur,
            cost_actual_usd: row.cost_actual_eur,
            error: row.error,
            started_at: row.started_at,
            finished_at: row.finished_at,
            job_id: row.job_id,
            created_at: row.created_at,
        }
    }
}

const APIFY_RUN_SELECT: &str = "SELECT r.id, r.actor_registry_id, a.name AS actor_name, r.status, \
    r.items_count, r.dataset_id, r.cost_estimate_eur, r.cost_actual_eur, r.error, r.started_at, \
    r.finished_at, r.job_id, r.created_at FROM apify_runs r \
    LEFT JOIN actor_registry a ON a.id = r.actor_registry_id";

pub async fn insert_apify_run(
    db: &Db,
    tenant_id: &str,
    actor_registry_id: &str,
    job_id: &str,
    input: &Value,
) -> Result<String, sqlx::Error> {
    let id = new_id();
    let now = now_iso();
    sqlx::query(
        "INSERT INTO apify_runs (id, tenant_id, actor_registry_id, job_id, status, input, \
         created_at, updated_at) VALUES (?, ?, ?, ?, 'queued', ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(actor_registry_id)
    .bind(job_id)
    .bind(to_json(input))
    .bind(&now)
    .bind(&now)
    .execute(&db.writer)
    .await?;
    Ok(id)
}

pub async fn get_apify_run(
    db: &Db,
    tenant_id: &str,
    id: &str,
) -> Result<Option<ApifyRun>, sqlx::Error> {
    let sql = format!("{APIFY_RUN_SELECT} WHERE r.tenant_id = ? AND r.id = ?");
    let row = sqlx::query_as::<_, ApifyRunRow>(&sql)
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(&db.reader)
        .await?;
    Ok(row.map(ApifyRun::from))
}

pub async fn list_apify_runs(
    db: &Db,
    tenant_id: &str,
    status: Option<&str>,
    actor_registry_id: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<ApifyRun>, i64), sqlx::Error> {
    let sql = format!(
        "{APIFY_RUN_SELECT} WHERE r.tenant_id = ? AND (? IS NULL OR r.status = ?) \
         AND (? IS NULL OR r.actor_registry_id = ?) ORDER BY r.created_at DESC LIMIT ? OFFSET ?"
    );
    let rows = sqlx::query_as::<_, ApifyRunRow>(&sql)
        .bind(tenant_id)
        .bind(status)
        .bind(status)
        .bind(actor_registry_id)
        .bind(actor_registry_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&db.reader)
        .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM apify_runs WHERE tenant_id = ? AND (? IS NULL OR status = ?) \
         AND (? IS NULL OR actor_registry_id = ?)",
    )
    .bind(tenant_id)
    .bind(status)
    .bind(status)
    .bind(actor_registry_id)
    .bind(actor_registry_id)
    .fetch_one(&db.reader)
    .await?;
    Ok((rows.into_iter().map(ApifyRun::from).collect(), total))
}

#[allow(clippy::too_many_arguments)]
pub async fn update_apify_run(
    db: &Db,
    id: &str,
    status: &str,
    provider_run_id: Option<&str>,
    dataset_id: Option<&str>,
    items_count: Option<i64>,
    cost_actual_usd: Option<f64>,
    error: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = now_iso();
    sqlx::query(
        "UPDATE apify_runs SET status = ?, provider_run_id = COALESCE(?, provider_run_id), \
         dataset_id = COALESCE(?, dataset_id), items_count = COALESCE(?, items_count), \
         cost_actual_eur = COALESCE(?, cost_actual_eur), error = COALESCE(?, error), \
         started_at = COALESCE(started_at, ?), \
         finished_at = CASE WHEN ? IN ('succeeded','failed','aborted','timed_out') THEN ? \
                            ELSE finished_at END, \
         updated_at = ? WHERE id = ?",
    )
    .bind(status)
    .bind(provider_run_id)
    .bind(dataset_id)
    .bind(items_count)
    .bind(cost_actual_usd)
    .bind(error)
    .bind(&now)
    .bind(status)
    .bind(&now)
    .bind(&now)
    .bind(id)
    .execute(&db.writer)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ApifyUsage {
    pub runs_total: i64,
    pub runs_running: i64,
    pub results_total: i64,
    pub errors_total: i64,
    pub actual_spend_usd: Option<f64>,
    pub estimate_spend_usd: Option<f64>,
}

pub async fn apify_usage(db: &Db, tenant_id: &str) -> Result<ApifyUsage, sqlx::Error> {
    let runs_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM apify_runs WHERE tenant_id = ?")
        .bind(tenant_id)
        .fetch_one(&db.reader)
        .await?;
    let runs_running: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM apify_runs WHERE tenant_id = ? AND status IN ('queued','running')",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    let results_total: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(items_count), 0) FROM apify_runs WHERE tenant_id = ?",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    let errors_total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM apify_runs WHERE tenant_id = ? AND status IN ('failed','aborted','timed_out')",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    let actual_spend_usd: Option<f64> = sqlx::query_scalar(
        "SELECT SUM(cost_actual_eur) FROM apify_runs WHERE tenant_id = ? AND cost_actual_eur IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    let estimate_spend_usd: Option<f64> = sqlx::query_scalar(
        "SELECT SUM(cost_estimate_eur) FROM apify_runs WHERE tenant_id = ? AND cost_estimate_eur IS NOT NULL",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    Ok(ApifyUsage {
        runs_total,
        runs_running,
        results_total,
        errors_total,
        actual_spend_usd,
        estimate_spend_usd,
    })
}

pub async fn apify_spend_since(db: &Db, tenant_id: &str, since: &str) -> Result<f64, sqlx::Error> {
    let spend: Option<f64> = sqlx::query_scalar(
        "SELECT SUM(cost_actual_eur) FROM apify_runs WHERE tenant_id = ? \
         AND cost_actual_eur IS NOT NULL AND created_at >= ?",
    )
    .bind(tenant_id)
    .bind(since)
    .fetch_one(&db.reader)
    .await?;
    Ok(spend.unwrap_or(0.0))
}

pub async fn count_running_apify_runs(db: &Db, tenant_id: &str) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT COUNT(*) FROM apify_runs WHERE tenant_id = ? AND status IN ('queued','running')",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await
}

/// Read the stored input template of one Apify run.
pub async fn apify_run_input(
    db: &Db,
    tenant_id: &str,
    run_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT input FROM apify_runs WHERE tenant_id = ? AND id = ?")
        .bind(tenant_id)
        .bind(run_id)
        .fetch_optional(&db.reader)
        .await
}

/// Spend guard for a billable Apify run. The console ceilings are in EUR; Apify reports USD, so the
/// guard never converts currencies: it enforces hard ceilings and refuses when a run cannot be
/// bounded. When spend is known in the provider currency it is reported separately as `actual_usd`.
pub async fn guard_apify_run(
    db: &Db,
    tenant_id: &str,
    config: &AppConfig,
    actor: &Actor,
    items_limit: i64,
) -> Result<(), CostGuardReason> {
    let running = count_running_apify_runs(db, tenant_id).await.unwrap_or(0);
    let budgets = effective_budgets(db, tenant_id, config).await;
    let since = crate::domain::time::to_iso(operations::since_midnight_utc(chrono::Utc::now()));
    let daily_spend = apify_spend_since(db, tenant_id, &since)
        .await
        .unwrap_or(0.0);
    let total_spend = apify_spend_since(db, tenant_id, "0000-01-01T00:00:00Z")
        .await
        .unwrap_or(0.0);

    operations::cost_guard(&CostGuardInput {
        // In mock mode the selected source adapter is always available; the ceilings still apply.
        provider_configured: match config.source_provider {
            crate::config::SourceChoice::Apify => config.apify.configured(),
            crate::config::SourceChoice::Mock => true,
        },
        kill_switch: budgets.kill_switch,
        actor_enabled: actor.enabled,
        actor_validated: actor.validation_status == "validated",
        items_limit,
        max_items_per_run: actor.max_items_per_run.min(budgets.max_items_per_run),
        running_runs: running,
        max_concurrent_runs: budgets.max_concurrent_runs,
        total_spend,
        daily_spend,
        run_cost_estimate: actor.cost_per_run_estimate,
        max_run_cost: budgets.max_run_cost_eur,
        daily_budget: budgets.daily_budget_eur,
        total_budget: budgets.total_budget_eur,
    })
}

/// Operator-set budget ceilings, validated against the environment defaults. An invalid override is
/// ignored so a bad setting can never widen spending.
#[derive(Debug, Clone, Copy)]
pub struct EffectiveBudgets {
    pub total_budget_eur: f64,
    pub daily_budget_eur: f64,
    pub max_run_cost_eur: f64,
    pub max_items_per_run: i64,
    pub max_concurrent_runs: i64,
    pub kill_switch: bool,
}

pub async fn effective_budgets(db: &Db, tenant_id: &str, config: &AppConfig) -> EffectiveBudgets {
    let mut budgets = EffectiveBudgets {
        total_budget_eur: config.apify.total_budget_eur,
        daily_budget_eur: config.apify.daily_budget_eur,
        max_run_cost_eur: config.apify.max_run_cost_eur,
        max_items_per_run: config.apify.max_items_per_run,
        max_concurrent_runs: config.apify.max_concurrent_runs,
        kill_switch: config.apify.kill_switch,
    };
    let Some(value) = crate::modules::tenancy::get_setting(db, tenant_id, "budgets")
        .await
        .ok()
        .flatten()
    else {
        return budgets;
    };
    if let Some(total) = value.get("apify_total_budget_eur").and_then(|v| v.as_f64()) {
        if total.is_finite() && total >= 0.0 {
            budgets.total_budget_eur = total;
        }
    }
    if let Some(daily) = value.get("apify_daily_budget_eur").and_then(|v| v.as_f64()) {
        if daily.is_finite() && daily >= 0.0 && daily <= budgets.total_budget_eur {
            budgets.daily_budget_eur = daily;
        }
    }
    if let Some(run_cost) = value.get("apify_max_run_cost_eur").and_then(|v| v.as_f64()) {
        if run_cost.is_finite() && run_cost >= 0.0 && run_cost <= budgets.daily_budget_eur {
            budgets.max_run_cost_eur = run_cost;
        }
    }
    if let Some(items) = value
        .get("apify_max_items_per_run")
        .and_then(|v| v.as_i64())
    {
        if items >= 1 && items <= config.apify.max_items_per_run {
            budgets.max_items_per_run = items;
        }
    }
    if let Some(concurrent) = value
        .get("apify_max_concurrent_runs")
        .and_then(|v| v.as_i64())
    {
        if concurrent >= 1 && concurrent <= config.apify.max_concurrent_runs {
            budgets.max_concurrent_runs = concurrent;
        }
    }
    if let Some(kill_switch) = value.get("kill_switch").and_then(|v| v.as_bool()) {
        budgets.kill_switch = budgets.kill_switch || kill_switch;
    }
    budgets
}
