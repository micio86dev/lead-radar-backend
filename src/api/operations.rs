//! Manual engagement recording and job inspection.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utoipa::IntoParams;

use super::{paged, AppError, AppResult, Pagination};
use crate::auth::TenantContext;
use crate::domain::enums::{JobKind, JobStatus};
use crate::domain::time::parse_iso;
use crate::modules::engagement::{self, EngagementKind};
use crate::modules::operations;
use crate::state::SharedState;

#[derive(Debug, Deserialize, IntoParams)]
pub struct EngagementQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
    #[serde(default)]
    pub lead_id: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/engagements", tag = "engagements", responses((status = 200, description = "Recorded engagements")))]
pub async fn list_engagements(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<EngagementQuery>,
) -> AppResult<Json<serde_json::Value>> {
    if let Some(kind) = &query.kind {
        if EngagementKind::parse(kind).is_none() {
            return Err(AppError::BadRequest(format!(
                "kind has an unknown value {kind:?}"
            )));
        }
    }
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (rows, total) = engagement::list_engagements(
        &state.db,
        &context.tenant_id,
        query.lead_id.as_deref(),
        query.kind.as_deref(),
        pagination.limit(),
        pagination.offset(),
    )
    .await?;
    Ok(Json(paged(json!(rows), &pagination, total)))
}

#[derive(Debug, Deserialize)]
pub struct NewEngagementBody {
    pub lead_id: String,
    pub comment_id: Option<String>,
    pub kind: String,
    pub note: Option<String>,
    pub occurred_at: Option<String>,
}

#[utoipa::path(post, path = "/api/v1/engagements", tag = "engagements", responses((status = 201, description = "Engagement recorded"), (status = 422, description = "Invalid input")))]
pub async fn create_engagement(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<NewEngagementBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let kind = EngagementKind::parse(&body.kind).ok_or_else(|| {
        AppError::Validation(vec![crate::error::FieldError::new(
            "kind",
            "must be one of reply, dm, meeting, interest, lost, other",
        )])
    })?;
    if let Some(occurred_at) = &body.occurred_at {
        if parse_iso(occurred_at).is_none() {
            return Err(AppError::Validation(vec![crate::error::FieldError::new(
                "occurred_at",
                "must be an RFC 3339 timestamp",
            )]));
        }
    }
    let created = engagement::create_engagement(
        &state.db,
        &context.tenant_id,
        &engagement::NewEngagement {
            lead_id: &body.lead_id,
            comment_id: body.comment_id.as_deref(),
            kind,
            note: body.note.as_deref(),
            occurred_at: body.occurred_at.as_deref(),
        },
    )
    .await?
    .ok_or(AppError::NotFound)?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "engagement_recorded",
        "lead",
        Some(&body.lead_id),
        &json!({"kind": kind.as_str(), "comment_id": body.comment_id}),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!(created))))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct JobQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/jobs", tag = "jobs", responses((status = 200, description = "Paginated jobs")))]
pub async fn list_jobs(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<JobQuery>,
) -> AppResult<Json<serde_json::Value>> {
    if let Some(status) = &query.status {
        if JobStatus::parse(status).is_none() {
            return Err(AppError::BadRequest(format!(
                "status has an unknown value {status:?}"
            )));
        }
    }
    if let Some(kind) = &query.kind {
        if JobKind::parse(kind).is_none() {
            return Err(AppError::BadRequest(format!(
                "kind has an unknown value {kind:?}"
            )));
        }
    }
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (rows, total) = operations::list_jobs(
        &state.db,
        &context.tenant_id,
        query.status.as_deref(),
        query.kind.as_deref(),
        pagination.limit(),
        pagination.offset(),
    )
    .await?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|job| {
            json!({
                "id": job.id,
                "kind": job.kind,
                "status": job.status,
                "attempts": job.attempts,
                "max_attempts": job.max_attempts,
                "run_at": job.run_at,
                "last_error": job.last_error,
                "created_at": job.created_at,
                "updated_at": job.updated_at,
            })
        })
        .collect();
    Ok(Json(paged(json!(items), &pagination, total)))
}

#[utoipa::path(post, path = "/api/v1/jobs/{id}/retry", tag = "jobs", responses((status = 202, description = "Job requeued"), (status = 404, description = "Job cannot be retried")))]
pub async fn retry_job(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let job = operations::retry_job(&state.db, &context.tenant_id, &id)
        .await?
        .ok_or_else(|| {
            AppError::Conflict("only failed or dead-letter jobs can be retried".to_string())
        })?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "job_retried",
        "job",
        Some(&id),
        &json!({"kind": job.kind}),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"job_id": job.id, "status": job.status})),
    ))
}
