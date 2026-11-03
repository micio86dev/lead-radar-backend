//! Manual engagement recording. This module records what a human did elsewhere; it never acts.

use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::time::now_iso;
use crate::modules::new_id;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngagementKind {
    Reply,
    Dm,
    Meeting,
    Interest,
    Lost,
    Other,
}

impl EngagementKind {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "reply" => Some(Self::Reply),
            "dm" => Some(Self::Dm),
            "meeting" => Some(Self::Meeting),
            "interest" => Some(Self::Interest),
            "lost" => Some(Self::Lost),
            "other" => Some(Self::Other),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Reply => "reply",
            Self::Dm => "dm",
            Self::Meeting => "meeting",
            Self::Interest => "interest",
            Self::Lost => "lost",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize)]
pub struct EngagementRow {
    pub id: String,
    pub lead_id: String,
    pub comment_id: Option<String>,
    pub kind: String,
    pub note: Option<String>,
    pub occurred_at: String,
    pub created_at: String,
}

pub async fn list_engagements(
    db: &Db,
    tenant_id: &str,
    lead_id: Option<&str>,
    kind: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<EngagementRow>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, EngagementRow>(
        "SELECT id, lead_id, comment_id, kind, note, occurred_at, created_at FROM engagements \
         WHERE tenant_id = ? AND (? IS NULL OR lead_id = ?) AND (? IS NULL OR kind = ?) \
         ORDER BY occurred_at DESC LIMIT ? OFFSET ?",
    )
    .bind(tenant_id)
    .bind(lead_id)
    .bind(lead_id)
    .bind(kind)
    .bind(kind)
    .bind(limit)
    .bind(offset)
    .fetch_all(&db.reader)
    .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM engagements WHERE tenant_id = ? \
         AND (? IS NULL OR lead_id = ?) AND (? IS NULL OR kind = ?)",
    )
    .bind(tenant_id)
    .bind(lead_id)
    .bind(lead_id)
    .bind(kind)
    .bind(kind)
    .fetch_one(&db.reader)
    .await?;
    Ok((rows, total))
}

pub async fn engagements_for_lead(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
) -> Result<Vec<EngagementRow>, sqlx::Error> {
    sqlx::query_as::<_, EngagementRow>(
        "SELECT id, lead_id, comment_id, kind, note, occurred_at, created_at FROM engagements \
         WHERE tenant_id = ? AND lead_id = ? ORDER BY occurred_at DESC",
    )
    .bind(tenant_id)
    .bind(lead_id)
    .fetch_all(&db.reader)
    .await
}

pub struct NewEngagement<'a> {
    pub lead_id: &'a str,
    pub comment_id: Option<&'a str>,
    pub kind: EngagementKind,
    pub note: Option<&'a str>,
    pub occurred_at: Option<&'a str>,
}

pub async fn create_engagement(
    db: &Db,
    tenant_id: &str,
    input: &NewEngagement<'_>,
) -> Result<Option<EngagementRow>, sqlx::Error> {
    let lead_exists: Option<String> =
        sqlx::query_scalar("SELECT id FROM leads WHERE tenant_id = ? AND id = ?")
            .bind(tenant_id)
            .bind(input.lead_id)
            .fetch_optional(&db.reader)
            .await?;
    if lead_exists.is_none() {
        return Ok(None);
    }
    if let Some(comment_id) = input.comment_id {
        let comment_exists: Option<String> = sqlx::query_scalar(
            "SELECT id FROM generated_comments WHERE tenant_id = ? AND id = ? AND lead_id = ?",
        )
        .bind(tenant_id)
        .bind(comment_id)
        .bind(input.lead_id)
        .fetch_optional(&db.reader)
        .await?;
        if comment_exists.is_none() {
            return Ok(None);
        }
    }
    let id = new_id();
    let occurred_at = input
        .occurred_at
        .map(str::to_string)
        .unwrap_or_else(now_iso);
    sqlx::query(
        "INSERT INTO engagements (id, tenant_id, lead_id, comment_id, kind, note, occurred_at, \
         created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(input.lead_id)
    .bind(input.comment_id)
    .bind(input.kind.as_str())
    .bind(input.note)
    .bind(&occurred_at)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    sqlx::query_as::<_, EngagementRow>(
        "SELECT id, lead_id, comment_id, kind, note, occurred_at, created_at FROM engagements \
         WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&db.reader)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engagement_kinds_round_trip() {
        for kind in [
            EngagementKind::Reply,
            EngagementKind::Dm,
            EngagementKind::Meeting,
            EngagementKind::Interest,
            EngagementKind::Lost,
            EngagementKind::Other,
        ] {
            assert_eq!(EngagementKind::parse(kind.as_str()), Some(kind));
        }
        assert_eq!(EngagementKind::parse("call"), None);
    }
}
