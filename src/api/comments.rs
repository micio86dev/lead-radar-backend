//! Comment Studio and Review Queue endpoints. Every transition is explicit and audited.

use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utoipa::IntoParams;

use super::{paged, AppError, AppResult, Pagination};
use crate::auth::TenantContext;
use crate::domain::comment::{patch_target_allowed, publication_allowed};
use crate::domain::enums::{CommentStatus, CommentVariant};
use crate::domain::time::parse_iso;
use crate::modules::comments::{self, CommentError};
use crate::state::SharedState;

fn map_error(error: CommentError) -> AppError {
    match error {
        CommentError::NotFound => AppError::NotFound,
        CommentError::InvalidTransition(message) => AppError::Conflict(message),
        CommentError::InvalidInput(message) => {
            AppError::Validation(vec![crate::error::FieldError::new("body", message)])
        }
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct CommentQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub lead_id: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/comments", tag = "comments", responses((status = 200, description = "Paginated comments")))]
pub async fn list(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<CommentQuery>,
) -> AppResult<Json<serde_json::Value>> {
    if let Some(status) = &query.status {
        if CommentStatus::parse(status).is_none() {
            return Err(AppError::BadRequest(format!(
                "status has an unknown value {status:?}"
            )));
        }
    }
    if let Some(variant) = &query.variant {
        if CommentVariant::parse(variant).is_none() {
            return Err(AppError::BadRequest(format!(
                "variant has an unknown value {variant:?}"
            )));
        }
    }
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (rows, total) = comments::list_comments(
        &state.db,
        &context.tenant_id,
        query.status.as_deref(),
        query.variant.as_deref(),
        query.lead_id.as_deref(),
        pagination.limit(),
        pagination.offset(),
    )
    .await?;
    let items: Vec<serde_json::Value> = rows.into_iter().map(comments::summary).collect();
    Ok(Json(paged(json!(items), &pagination, total)))
}

async fn detail_json(
    state: &SharedState,
    tenant: &str,
    comment_id: &str,
) -> AppResult<serde_json::Value> {
    let comment = comments::get_comment(&state.db, tenant, comment_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let versions = comments::versions(&state.db, comment_id).await?;
    let events = comments::events(&state.db, comment_id).await?;
    Ok(json!({
        "id": comment.id,
        "lead_id": comment.lead_id,
        "lead_title": comment.lead_title,
        "lead": {
            "id": comment.lead_id,
            "title": comment.lead_title,
            "url": comment.lead_url,
            "language": comment.lead_language,
        },
        "variant": comment.variant,
        "status": comment.status,
        "language": comment.language,
        "body": comment.body,
        "version": comment.version,
        "published_at": comment.published_at,
        "publication_note": comment.publication_note,
        "created_at": comment.created_at,
        "updated_at": comment.updated_at,
        "versions": versions,
        "review_events": events,
    }))
}

#[utoipa::path(get, path = "/api/v1/comments/{id}", tag = "comments", responses((status = 200, description = "Comment detail"), (status = 404, description = "Unknown comment")))]
pub async fn detail(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(detail_json(&state, &context.tenant_id, &id).await?))
}

#[derive(Debug, Deserialize)]
pub struct CommentPatch {
    pub body: Option<String>,
    pub status: Option<String>,
}

#[utoipa::path(patch, path = "/api/v1/comments/{id}", tag = "comments", responses((status = 200, description = "Updated comment"), (status = 409, description = "Illegal transition")))]
pub async fn patch(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(patch): Json<CommentPatch>,
) -> AppResult<Json<serde_json::Value>> {
    context.require_reviewer()?;
    let status = match patch.status.as_deref() {
        Some(raw) => {
            let parsed = CommentStatus::parse(raw).ok_or_else(|| {
                AppError::BadRequest(format!("status has an unknown value {raw:?}"))
            })?;
            if !patch_target_allowed(CommentStatus::Draft, parsed) && parsed != CommentStatus::Draft
            {
                return Err(AppError::BadRequest(format!(
                    "status {raw:?} cannot be set through PATCH; use approve, reject or record-publication"
                )));
            }
            Some(parsed)
        }
        None => None,
    };
    let comment = comments::patch_comment(
        &state.db,
        &context.tenant_id,
        &id,
        patch.body.as_deref(),
        status,
        &context.actor,
    )
    .await
    .map_err(map_error)?;
    let _ = comment;
    Ok(Json(detail_json(&state, &context.tenant_id, &id).await?))
}

#[derive(Debug, Default, Deserialize)]
pub struct NoteBody {
    pub note: Option<String>,
}

#[utoipa::path(post, path = "/api/v1/comments/{id}/approve", tag = "comments", responses((status = 200, description = "Comment approved"), (status = 403, description = "Viewer role cannot review")))]
pub async fn approve(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<NoteBody>,
) -> AppResult<Json<serde_json::Value>> {
    context.require_reviewer()?;
    comments::approve_comment(
        &state.db,
        &context.tenant_id,
        &id,
        &context.actor,
        body.note.as_deref(),
    )
    .await
    .map_err(map_error)?;
    Ok(Json(detail_json(&state, &context.tenant_id, &id).await?))
}

#[utoipa::path(post, path = "/api/v1/comments/{id}/reject", tag = "comments", responses((status = 200, description = "Comment rejected")))]
pub async fn reject(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<NoteBody>,
) -> AppResult<Json<serde_json::Value>> {
    context.require_reviewer()?;
    comments::reject_comment(
        &state.db,
        &context.tenant_id,
        &id,
        &context.actor,
        body.note.as_deref(),
    )
    .await
    .map_err(map_error)?;
    Ok(Json(detail_json(&state, &context.tenant_id, &id).await?))
}

#[derive(Debug, Default, Deserialize)]
pub struct PublicationBody {
    pub published_at: Option<String>,
    pub note: Option<String>,
}

#[utoipa::path(post, path = "/api/v1/comments/{id}/record-publication", tag = "comments", responses((status = 200, description = "Publication recorded"), (status = 409, description = "Comment must be approved first")))]
pub async fn record_publication(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<PublicationBody>,
) -> AppResult<Json<serde_json::Value>> {
    context.require_reviewer()?;
    if let Some(published_at) = &body.published_at {
        if parse_iso(published_at).is_none() {
            return Err(AppError::BadRequest(
                "published_at must be an RFC 3339 timestamp".to_string(),
            ));
        }
    }
    let comment = comments::record_publication(
        &state.db,
        &context.tenant_id,
        &id,
        &context.actor,
        body.published_at.as_deref(),
        body.note.as_deref(),
    )
    .await
    .map_err(map_error)?;
    debug_assert!(publication_allowed(CommentStatus::Approved));
    let _ = comment;
    Ok(Json(detail_json(&state, &context.tenant_id, &id).await?))
}
