//! Human review workflow for generated comments: versions, events and status transitions.
//!
//! No function here contacts a third party. `record_publication` records that a human published a
//! comment by hand.

use serde::Serialize;
use serde_json::json;
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::comment::{
    approval_allowed, patch_target_allowed, publication_allowed, rejection_allowed,
};
use crate::domain::enums::{CommentStatus, CommentVariant};
use crate::domain::time::now_iso;
use crate::modules::{generative, operations, to_json};

#[derive(Debug, Clone, FromRow)]
pub struct CommentRow {
    pub id: String,
    pub tenant_id: String,
    pub lead_id: String,
    pub variant: String,
    pub body: String,
    pub language: String,
    pub status: String,
    pub version: i64,
    pub published_at: Option<String>,
    pub publication_note: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub lead_title: Option<String>,
    pub lead_url: Option<String>,
    pub lead_language: Option<String>,
}

const COMMENT_SELECT: &str = "SELECT c.*, l.title AS lead_title, l.source_url AS lead_url, \
    l.language AS lead_language FROM generated_comments c \
    LEFT JOIN leads l ON l.id = c.lead_id";

#[derive(Debug, Clone, Serialize)]
pub struct CommentSummary {
    pub id: String,
    pub lead_id: String,
    pub lead_title: Option<String>,
    pub variant: String,
    pub status: String,
    pub language: String,
    pub body: String,
    pub version: i64,
    pub created_at: String,
    pub updated_at: String,
    pub published_at: Option<String>,
}

impl From<CommentRow> for CommentSummary {
    fn from(row: CommentRow) -> Self {
        Self {
            id: row.id,
            lead_id: row.lead_id,
            lead_title: row.lead_title,
            variant: row.variant,
            status: row.status,
            language: row.language,
            body: row.body,
            version: row.version,
            created_at: row.created_at,
            updated_at: row.updated_at,
            published_at: row.published_at,
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct CommentVersionRow {
    pub version: i64,
    pub body: String,
    pub actor: String,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct ReviewEventRow {
    pub event: String,
    pub actor: String,
    pub note: Option<String>,
    pub created_at: String,
}

pub async fn list_comments(
    db: &Db,
    tenant_id: &str,
    status: Option<&str>,
    variant: Option<&str>,
    lead_id: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<CommentRow>, i64), sqlx::Error> {
    let sql = format!(
        "{COMMENT_SELECT} WHERE c.tenant_id = ? AND (? IS NULL OR c.status = ?) \
         AND (? IS NULL OR c.variant = ?) AND (? IS NULL OR c.lead_id = ?) \
         ORDER BY c.updated_at DESC LIMIT ? OFFSET ?"
    );
    let rows = sqlx::query_as::<_, CommentRow>(&sql)
        .bind(tenant_id)
        .bind(status)
        .bind(status)
        .bind(variant)
        .bind(variant)
        .bind(lead_id)
        .bind(lead_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&db.reader)
        .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM generated_comments WHERE tenant_id = ? \
         AND (? IS NULL OR status = ?) AND (? IS NULL OR variant = ?) \
         AND (? IS NULL OR lead_id = ?)",
    )
    .bind(tenant_id)
    .bind(status)
    .bind(status)
    .bind(variant)
    .bind(variant)
    .bind(lead_id)
    .bind(lead_id)
    .fetch_one(&db.reader)
    .await?;
    Ok((rows, total))
}

pub async fn get_comment(
    db: &Db,
    tenant_id: &str,
    comment_id: &str,
) -> Result<Option<CommentRow>, sqlx::Error> {
    let sql = format!("{COMMENT_SELECT} WHERE c.tenant_id = ? AND c.id = ?");
    sqlx::query_as::<_, CommentRow>(&sql)
        .bind(tenant_id)
        .bind(comment_id)
        .fetch_optional(&db.reader)
        .await
}

pub async fn versions(db: &Db, comment_id: &str) -> Result<Vec<CommentVersionRow>, sqlx::Error> {
    sqlx::query_as::<_, CommentVersionRow>(
        "SELECT version, body, actor, created_at FROM comment_versions WHERE comment_id = ? \
         ORDER BY version DESC",
    )
    .bind(comment_id)
    .fetch_all(&db.reader)
    .await
}

pub async fn events(db: &Db, comment_id: &str) -> Result<Vec<ReviewEventRow>, sqlx::Error> {
    sqlx::query_as::<_, ReviewEventRow>(
        "SELECT event, actor, note, created_at FROM review_events WHERE comment_id = ? \
         ORDER BY created_at DESC",
    )
    .bind(comment_id)
    .fetch_all(&db.reader)
    .await
}

#[derive(Debug, PartialEq, Eq)]
pub enum CommentError {
    NotFound,
    InvalidTransition(String),
    InvalidInput(String),
}

fn parse_status(raw: &str) -> Result<CommentStatus, CommentError> {
    CommentStatus::parse(raw)
        .ok_or_else(|| CommentError::InvalidInput(format!("unknown comment status {raw:?}")))
}

/// Edit the body and/or move between drafting statuses. A body change creates a new version.
pub async fn patch_comment(
    db: &Db,
    tenant_id: &str,
    comment_id: &str,
    body: Option<&str>,
    status: Option<CommentStatus>,
    actor: &str,
) -> Result<CommentRow, CommentError> {
    let comment = get_comment(db, tenant_id, comment_id)
        .await
        .map_err(|_| CommentError::NotFound)?
        .ok_or(CommentError::NotFound)?;
    let current = parse_status(&comment.status)?;

    if let Some(target) = status {
        if !patch_target_allowed(current, target) {
            return Err(CommentError::InvalidTransition(format!(
                "cannot move a comment from {} to {}; use the approve, reject or \
                 record-publication endpoint",
                current.as_str(),
                target.as_str()
            )));
        }
    }
    if body.is_none() && status.is_none() {
        return Err(CommentError::InvalidInput("nothing to update".to_string()));
    }

    let now = now_iso();
    if let Some(new_body) = body {
        let trimmed = new_body.trim();
        if trimmed.is_empty() {
            return Err(CommentError::InvalidInput(
                "body must not be empty".to_string(),
            ));
        }
        let next_version = comment.version + 1;
        sqlx::query(
            "UPDATE generated_comments SET body = ?, version = ?, updated_at = ? WHERE id = ?",
        )
        .bind(trimmed)
        .bind(next_version)
        .bind(&now)
        .bind(comment_id)
        .execute(&db.writer)
        .await
        .map_err(|_| CommentError::InvalidTransition("database error".to_string()))?;
        generative::insert_version(db, comment_id, next_version, trimmed, actor)
            .await
            .map_err(|_| CommentError::InvalidTransition("database error".to_string()))?;
        generative::insert_event(db, tenant_id, comment_id, "edited", actor, None)
            .await
            .ok();
    }
    if let Some(target) = status {
        sqlx::query("UPDATE generated_comments SET status = ?, updated_at = ? WHERE id = ?")
            .bind(target.as_str())
            .bind(&now)
            .bind(comment_id)
            .execute(&db.writer)
            .await
            .map_err(|_| CommentError::InvalidTransition("database error".to_string()))?;
        generative::insert_event(
            db,
            tenant_id,
            comment_id,
            &format!("status_{}", target.as_str()),
            actor,
            None,
        )
        .await
        .ok();
    }

    get_comment(db, tenant_id, comment_id)
        .await
        .map_err(|_| CommentError::NotFound)?
        .ok_or(CommentError::NotFound)
}

pub async fn approve_comment(
    db: &Db,
    tenant_id: &str,
    comment_id: &str,
    actor: &str,
    note: Option<&str>,
) -> Result<CommentRow, CommentError> {
    let comment = get_comment(db, tenant_id, comment_id)
        .await
        .map_err(|_| CommentError::NotFound)?
        .ok_or(CommentError::NotFound)?;
    let current = parse_status(&comment.status)?;
    if !approval_allowed(current) {
        return Err(CommentError::InvalidTransition(format!(
            "cannot approve a comment in status {}",
            current.as_str()
        )));
    }
    sqlx::query("UPDATE generated_comments SET status = 'approved', updated_at = ? WHERE id = ?")
        .bind(now_iso())
        .bind(comment_id)
        .execute(&db.writer)
        .await
        .map_err(|_| CommentError::InvalidTransition("database error".to_string()))?;
    generative::insert_event(db, tenant_id, comment_id, "approved", actor, note)
        .await
        .ok();
    operations::audit(
        db,
        tenant_id,
        actor,
        "comment_approved",
        "generated_comment",
        Some(comment_id),
        &json!({"note": note}),
    )
    .await
    .ok();
    get_comment(db, tenant_id, comment_id)
        .await
        .map_err(|_| CommentError::NotFound)?
        .ok_or(CommentError::NotFound)
}

pub async fn reject_comment(
    db: &Db,
    tenant_id: &str,
    comment_id: &str,
    actor: &str,
    note: Option<&str>,
) -> Result<CommentRow, CommentError> {
    let comment = get_comment(db, tenant_id, comment_id)
        .await
        .map_err(|_| CommentError::NotFound)?
        .ok_or(CommentError::NotFound)?;
    let current = parse_status(&comment.status)?;
    if !rejection_allowed(current) {
        return Err(CommentError::InvalidTransition(format!(
            "cannot reject a comment in status {}",
            current.as_str()
        )));
    }
    sqlx::query("UPDATE generated_comments SET status = 'rejected', updated_at = ? WHERE id = ?")
        .bind(now_iso())
        .bind(comment_id)
        .execute(&db.writer)
        .await
        .map_err(|_| CommentError::InvalidTransition("database error".to_string()))?;
    generative::insert_event(db, tenant_id, comment_id, "rejected", actor, note)
        .await
        .ok();
    operations::audit(
        db,
        tenant_id,
        actor,
        "comment_rejected",
        "generated_comment",
        Some(comment_id),
        &json!({"note": note}),
    )
    .await
    .ok();
    get_comment(db, tenant_id, comment_id)
        .await
        .map_err(|_| CommentError::NotFound)?
        .ok_or(CommentError::NotFound)
}

/// Record that a human published the approved comment manually. Nothing is sent anywhere.
pub async fn record_publication(
    db: &Db,
    tenant_id: &str,
    comment_id: &str,
    actor: &str,
    published_at: Option<&str>,
    note: Option<&str>,
) -> Result<CommentRow, CommentError> {
    let comment = get_comment(db, tenant_id, comment_id)
        .await
        .map_err(|_| CommentError::NotFound)?
        .ok_or(CommentError::NotFound)?;
    let current = parse_status(&comment.status)?;
    if !publication_allowed(current) {
        return Err(CommentError::InvalidTransition(format!(
            "comment must be approved before recording publication (status {})",
            current.as_str()
        )));
    }
    // A pitch is the one kind of reply that can get an account banned. It is recorded as published
    // only when the operator has established that the community allows promotion: `unknown` is not
    // permission, and neither is a stale assumption.
    if matches!(comment.variant.as_str(), "soft_pitch" | "direct_pitch") {
        let community = community_of_lead(db, tenant_id, &comment.lead_id)
            .await
            .map_err(|_| CommentError::InvalidTransition("database error".to_string()))?;
        let policy = match community.as_deref() {
            Some(name) => crate::modules::sources::community_policy_by_name(db, tenant_id, name)
                .await
                .map_err(|_| CommentError::InvalidTransition("database error".to_string()))?,
            None => None,
        };
        let allowed = policy
            .as_ref()
            .map(|community| community.self_promotion.as_str() == "allowed")
            .unwrap_or(false);
        if !allowed {
            let status = policy
                .map(|community| community.self_promotion)
                .unwrap_or_else(|| "unknown (community not registered)".to_string());
            return Err(CommentError::InvalidTransition(format!(
                "self-promotion_status={status}: record that this community allows promotion before \
                 publishing a pitch, or publish the helpful variant instead"
            )));
        }
    }

    let published_at = published_at.map(str::to_string).unwrap_or_else(now_iso);
    sqlx::query(
        "UPDATE generated_comments SET status = 'published_manually', published_at = ?, \
         publication_note = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&published_at)
    .bind(note)
    .bind(now_iso())
    .bind(comment_id)
    .execute(&db.writer)
    .await
    .map_err(|_| CommentError::InvalidTransition("database error".to_string()))?;
    generative::insert_event(db, tenant_id, comment_id, "published_manually", actor, note)
        .await
        .ok();
    operations::audit(
        db,
        tenant_id,
        actor,
        "comment_publication_recorded",
        "generated_comment",
        Some(comment_id),
        &json!({"published_at": published_at, "note": note}),
    )
    .await
    .ok();
    get_comment(db, tenant_id, comment_id)
        .await
        .map_err(|_| CommentError::NotFound)?
        .ok_or(CommentError::NotFound)
}

pub async fn review_queue_counts(db: &Db, tenant_id: &str) -> Result<(i64, i64, i64), sqlx::Error> {
    let pending: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM generated_comments WHERE tenant_id = ? AND status = 'pending_review'",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    let needs_edit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM generated_comments WHERE tenant_id = ? AND status = 'needs_edit'",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    let approved: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM generated_comments WHERE tenant_id = ? AND status = 'approved'",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    Ok((pending, needs_edit, approved))
}

pub async fn comments_for_lead(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
) -> Result<Vec<CommentRow>, sqlx::Error> {
    let sql = format!(
        "{COMMENT_SELECT} WHERE c.tenant_id = ? AND c.lead_id = ? \
         ORDER BY CASE c.variant WHEN 'helpful' THEN 0 WHEN 'soft_pitch' THEN 1 ELSE 2 END"
    );
    sqlx::query_as::<_, CommentRow>(&sql)
        .bind(tenant_id)
        .bind(lead_id)
        .fetch_all(&db.reader)
        .await
}

/// The comment workflow never publishes. Kept explicit so a future change has to confront it.
pub const AUTOMATIC_PUBLICATION_SUPPORTED: bool = false;

pub fn variant_of(row: &CommentRow) -> Option<CommentVariant> {
    CommentVariant::parse(&row.variant)
}

/// Serialize a stored comment for the API.
pub fn summary(row: CommentRow) -> serde_json::Value {
    json!({
        "id": row.id,
        "lead_id": row.lead_id,
        "lead_title": row.lead_title,
        "variant": row.variant,
        "status": row.status,
        "language": row.language,
        "body": row.body,
        "version": row.version,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
        "published_at": row.published_at,
    })
}

pub fn to_json_string(value: &serde_json::Value) -> String {
    to_json(value)
}

/// The community a lead was collected from, used to resolve its engagement policy.
async fn community_of_lead(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT community FROM leads WHERE tenant_id = ? AND id = ?")
        .bind(tenant_id)
        .bind(lead_id)
        .fetch_optional(&db.reader)
        .await
        .map(|row| row.flatten())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_selection_never_publishes() {
        const { assert!(!AUTOMATIC_PUBLICATION_SUPPORTED) };
    }

    #[test]
    fn variant_parsing_is_strict() {
        let row = CommentRow {
            id: "c".into(),
            tenant_id: "t".into(),
            lead_id: "l".into(),
            variant: "direct_pitch".into(),
            body: "body".into(),
            language: "en".into(),
            status: "draft".into(),
            version: 1,
            published_at: None,
            publication_note: None,
            created_at: "now".into(),
            updated_at: "now".into(),
            lead_title: None,
            lead_url: None,
            lead_language: None,
        };
        assert_eq!(variant_of(&row), Some(CommentVariant::DirectPitch));
        let mut unknown = row;
        unknown.variant = "spam".into();
        assert_eq!(variant_of(&unknown), None);
    }
}
