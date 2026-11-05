//! Reply pacing and follow-up tracking.
//!
//! Reddit throttles an account on its own terms (karma, age, per-subreddit rules). This module records
//! what actually happened: a conservative floor after a recorded publication, and the exact wait Reddit
//! announced when the browser widget reports it. It never invents a limit, never retries a call and never
//! schedules anything on the user's behalf.

use std::collections::HashMap;

use chrono::Duration;
use serde::Serialize;
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::time::{now, now_iso, to_iso};
use crate::modules::new_id;

pub const STATES: [&str; 3] = ["waiting", "answered", "closed"];

/// Why a lead was set aside. A fixed list, so the reason is reportable instead of free prose.
pub const SKIP_REASONS: [&str; 5] = [
    "not_relevant",
    "no_product_fit",
    "community_rules",
    "already_handled",
    "other",
];

pub fn parse_skip_reason(raw: &str) -> Option<&'static str> {
    let wanted = raw.trim().to_ascii_lowercase();
    SKIP_REASONS
        .into_iter()
        .find(|reason| *reason == wanted.as_str())
}

pub fn parse_state(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "waiting" => Some("waiting"),
        "answered" => Some("answered"),
        "closed" => Some("closed"),
        _ => None,
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct FollowupRow {
    pub id: String,
    pub lead_id: String,
    pub comment_id: Option<String>,
    pub state: String,
    pub new_replies: i64,
    pub last_reporter: Option<String>,
    pub last_excerpt: Option<String>,
    pub last_reply_at: Option<String>,
    pub published_at: Option<String>,
    pub reported_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Pacing {
    pub next_reply_at: Option<String>,
    pub source: Option<String>,
}

/// Read the current pacing row; a missing row means "no wait recorded".
pub async fn pacing(db: &Db, tenant_id: &str) -> anyhow::Result<Pacing> {
    let row: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT next_reply_at, source FROM reply_pacing WHERE tenant_id = ?")
            .bind(tenant_id)
            .fetch_optional(&db.reader)
            .await?;
    Ok(match row {
        Some((next_reply_at, source)) => Pacing {
            next_reply_at,
            source,
        },
        None => Pacing {
            next_reply_at: None,
            source: None,
        },
    })
}

pub async fn set_pacing(
    db: &Db,
    tenant_id: &str,
    next_reply_at: &str,
    source: &str,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO reply_pacing (tenant_id, next_reply_at, source, updated_at) VALUES (?, ?, ?, ?) \
         ON CONFLICT(tenant_id) DO UPDATE SET next_reply_at = excluded.next_reply_at, \
         source = excluded.source, updated_at = excluded.updated_at",
    )
    .bind(tenant_id)
    .bind(next_reply_at)
    .bind(source)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

/// Apply the configured floor after a recorded publication and open (or refresh) the follow-up.
pub async fn record_publication(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
    comment_id: &str,
    min_interval_minutes: i64,
) -> anyhow::Result<()> {
    let minutes = min_interval_minutes.clamp(1, 1440);
    let next = to_iso(now() + Duration::minutes(minutes));
    set_pacing(db, tenant_id, &next, "publication").await?;

    sqlx::query(
        "INSERT INTO followups (id, tenant_id, lead_id, comment_id, state, new_replies, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'waiting', 0, ?, ?) \
         ON CONFLICT(tenant_id, lead_id) DO UPDATE SET comment_id = excluded.comment_id, \
         state = 'waiting', updated_at = excluded.updated_at",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(lead_id)
    .bind(comment_id)
    .bind(now_iso())
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

/// Publication time per lead, used to tell the console which leads were already answered.
pub async fn publications(db: &Db, tenant_id: &str) -> anyhow::Result<HashMap<String, String>> {
    let rows: Vec<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT lead_id, published_at, updated_at FROM generated_comments \
         WHERE tenant_id = ? AND status = 'published_manually'",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(lead_id, published_at, updated_at)| (lead_id, published_at.unwrap_or(updated_at)))
        .collect())
}

pub async fn publication_time(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
) -> anyhow::Result<Option<String>> {
    let row: Option<(Option<String>, String)> = sqlx::query_as(
        "SELECT published_at, updated_at FROM generated_comments \
         WHERE tenant_id = ? AND lead_id = ? AND status = 'published_manually' \
         ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .bind(lead_id)
    .fetch_optional(&db.reader)
    .await?;
    Ok(row.map(|(published_at, updated_at)| published_at.unwrap_or(updated_at)))
}

/// The three queue views as counts, so a tab can print its own number without loading the view.
#[derive(Debug, Clone, Default, Serialize)]
pub struct QueueCounts {
    pub to_reply: i64,
    pub answered: i64,
    pub skipped: i64,
}

/// The reply queue's own definition of "open": active, worth answering, never published, never skipped.
const OPEN_LEAD: &str =
    "l.is_active = 1 AND l.qualification_status IN ('qualified', 'needs_review')";

pub async fn queue_counts(db: &Db, tenant_id: &str) -> anyhow::Result<QueueCounts> {
    let published = "EXISTS (SELECT 1 FROM generated_comments c WHERE c.lead_id = l.id \
                     AND c.status = 'published_manually')";
    let sql = format!(
        "SELECT \
           SUM(CASE WHEN reply_skipped_at IS NULL AND NOT {published} THEN 1 ELSE 0 END) AS to_reply, \
           SUM(CASE WHEN {published} THEN 1 ELSE 0 END) AS answered, \
           SUM(CASE WHEN reply_skipped_at IS NOT NULL AND NOT {published} THEN 1 ELSE 0 END) AS skipped \
         FROM leads l WHERE l.tenant_id = ? AND {OPEN_LEAD}"
    );
    let row: (Option<i64>, Option<i64>, Option<i64>) = sqlx::query_as(&sql)
        .bind(tenant_id)
        .fetch_one(&db.reader)
        .await?;
    Ok(QueueCounts {
        to_reply: row.0.unwrap_or(0),
        answered: row.1.unwrap_or(0),
        skipped: row.2.unwrap_or(0),
    })
}

/// Mark a lead as one the operator will not answer, or clear that mark. `None` clears it.
pub async fn set_skip(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
    reason: Option<&str>,
) -> anyhow::Result<Option<(Option<String>, Option<String>)>> {
    let now = now_iso();
    let updated = sqlx::query(
        "UPDATE leads SET reply_skipped_at = ?, reply_skip_reason = ?, updated_at = ? \
         WHERE id = ? AND tenant_id = ?",
    )
    .bind(reason.map(|_| now.clone()))
    .bind(reason)
    .bind(&now)
    .bind(lead_id)
    .bind(tenant_id)
    .execute(&db.writer)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    let row: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT reply_skipped_at, reply_skip_reason FROM leads WHERE id = ? AND tenant_id = ?",
    )
    .bind(lead_id)
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    Ok(Some(row))
}

pub async fn counts(db: &Db, tenant_id: &str) -> anyhow::Result<i64> {
    let answered: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT lead_id) FROM generated_comments \
         WHERE tenant_id = ? AND status = 'published_manually'",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    Ok(answered)
}

#[derive(Debug, Clone, Default)]
pub struct FollowupUpdate<'a> {
    pub state: Option<&'a str>,
    pub new_replies: Option<i64>,
    pub last_reporter: Option<&'a str>,
    pub last_excerpt: Option<&'a str>,
    pub last_reply_at: Option<&'a str>,
}

pub async fn followup_for_lead(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
) -> anyhow::Result<Option<FollowupRow>> {
    let row = sqlx::query_as::<_, FollowupRow>(
        "SELECT f.*, \
         (SELECT c.published_at FROM generated_comments c WHERE c.id = f.comment_id) AS published_at \
         FROM followups f WHERE f.tenant_id = ? AND f.lead_id = ?",
    )
    .bind(tenant_id)
    .bind(lead_id)
    .fetch_optional(&db.reader)
    .await?;
    Ok(row)
}

/// Upsert a follow-up report. Only the provided fields change, so a widget that reports two new replies
/// cannot erase a state the human set on the dashboard.
pub async fn upsert_followup(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
    update: &FollowupUpdate<'_>,
) -> anyhow::Result<FollowupRow> {
    let existing = followup_for_lead(db, tenant_id, lead_id).await?;
    let now_value = now_iso();
    let (id, created_at) = match &existing {
        Some(row) => (row.id.clone(), row.created_at.clone()),
        None => (new_id(), now_value.clone()),
    };
    let state = update
        .state
        .or(existing.as_ref().map(|row| row.state.as_str()))
        .unwrap_or("waiting");
    let new_replies = update
        .new_replies
        .unwrap_or_else(|| existing.as_ref().map(|row| row.new_replies).unwrap_or(0));
    let last_reporter = update
        .last_reporter
        .map(str::to_string)
        .or_else(|| existing.as_ref().and_then(|row| row.last_reporter.clone()));
    let last_excerpt = update
        .last_excerpt
        .map(|text| text.chars().take(400).collect::<String>())
        .or_else(|| existing.as_ref().and_then(|row| row.last_excerpt.clone()));
    let last_reply_at = update
        .last_reply_at
        .map(str::to_string)
        .or_else(|| existing.as_ref().and_then(|row| row.last_reply_at.clone()));
    let reported_at = if update.new_replies.is_some() || update.last_reply_at.is_some() {
        Some(now_value.clone())
    } else {
        existing.as_ref().and_then(|row| row.reported_at.clone())
    };
    let comment_id = existing.as_ref().and_then(|row| row.comment_id.clone());

    sqlx::query(
        "INSERT INTO followups (id, tenant_id, lead_id, comment_id, state, new_replies, last_reporter, \
         last_excerpt, last_reply_at, reported_at, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(tenant_id, lead_id) DO UPDATE SET state = excluded.state, \
         new_replies = excluded.new_replies, last_reporter = excluded.last_reporter, \
         last_excerpt = excluded.last_excerpt, last_reply_at = excluded.last_reply_at, \
         reported_at = excluded.reported_at, updated_at = excluded.updated_at",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(lead_id)
    .bind(&comment_id)
    .bind(state)
    .bind(new_replies)
    .bind(&last_reporter)
    .bind(&last_excerpt)
    .bind(&last_reply_at)
    .bind(&reported_at)
    .bind(&created_at)
    .bind(&now_value)
    .execute(&db.writer)
    .await?;

    followup_for_lead(db, tenant_id, lead_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("follow-up vanished after upsert"))
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct FollowupView {
    pub lead_id: String,
    pub title: Option<String>,
    pub url: Option<String>,
    pub community: Option<String>,
    pub state: String,
    pub new_replies: i64,
    pub last_reporter: Option<String>,
    pub last_excerpt: Option<String>,
    pub last_reply_at: Option<String>,
    pub published_at: Option<String>,
    pub updated_at: String,
}

pub async fn list(
    db: &Db,
    tenant_id: &str,
    state: Option<&str>,
    limit: i64,
) -> anyhow::Result<Vec<FollowupView>> {
    let rows = sqlx::query_as::<_, FollowupView>(
        "SELECT f.lead_id, l.title, l.source_url AS url, l.community, f.state, f.new_replies, \
         f.last_reporter, f.last_excerpt, f.last_reply_at, \
         (SELECT c.published_at FROM generated_comments c WHERE c.id = f.comment_id) AS published_at, \
         f.updated_at \
         FROM followups f JOIN leads l ON l.id = f.lead_id AND l.tenant_id = f.tenant_id \
         WHERE f.tenant_id = ? AND (? IS NULL OR f.state = ?) \
         ORDER BY f.updated_at DESC LIMIT ?",
    )
    .bind(tenant_id)
    .bind(state)
    .bind(state)
    .bind(limit.clamp(1, 100))
    .fetch_all(&db.reader)
    .await?;
    Ok(rows)
}
