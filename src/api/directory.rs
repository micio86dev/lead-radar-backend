//! Communities and sources: validation is recorded, never assumed.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utoipa::IntoParams;

use super::{paged, AppError, AppResult, Pagination};
use crate::auth::TenantContext;
use crate::domain::enums::{ValidationStatus, Vertical};
use crate::modules::{operations, sources};
use crate::state::SharedState;

#[derive(Debug, Deserialize, IntoParams)]
pub struct CommunityQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
    #[serde(default)]
    pub vertical: Option<String>,
    #[serde(default)]
    pub validation_status: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/communities", tag = "communities", responses((status = 200, description = "Communities with validation state")))]
pub async fn list_communities(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<CommunityQuery>,
) -> AppResult<Json<serde_json::Value>> {
    if let Some(vertical) = &query.vertical {
        if Vertical::parse(vertical).is_none() {
            return Err(AppError::BadRequest(format!(
                "vertical has an unknown value {vertical:?}"
            )));
        }
    }
    if let Some(status) = &query.validation_status {
        if ValidationStatus::parse(status).is_none() {
            return Err(AppError::BadRequest(format!(
                "validation_status has an unknown value {status:?}"
            )));
        }
    }
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (rows, total) = sources::list_communities(
        &state.db,
        &context.tenant_id,
        query.vertical.as_deref(),
        query.validation_status.as_deref(),
        pagination.limit(),
        pagination.offset(),
    )
    .await?;
    Ok(Json(paged(json!(rows), &pagination, total)))
}

#[derive(Debug, Deserialize)]
pub struct CommunityPatchBody {
    pub enabled: Option<bool>,
    pub notes: Option<String>,
}

#[utoipa::path(patch, path = "/api/v1/communities/{id}", tag = "communities", responses((status = 200, description = "Updated community"), (status = 409, description = "Cannot enable an unvalidated community")))]
pub async fn patch_community(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<CommunityPatchBody>,
) -> AppResult<Json<serde_json::Value>> {
    if body.enabled == Some(true) {
        let community = sources::get_community(&state.db, &context.tenant_id, &id)
            .await?
            .ok_or(AppError::NotFound)?;
        if community.validation_status != "validated" {
            return Err(AppError::Conflict(
                "community_not_validated: record validation evidence before enabling".to_string(),
            ));
        }
    }
    let community = sources::patch_community(
        &state.db,
        &context.tenant_id,
        &id,
        &sources::CommunityPatch {
            enabled: body.enabled,
            notes: body.notes.as_deref(),
        },
    )
    .await?
    .ok_or(AppError::NotFound)?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "community_updated",
        "community",
        Some(&id),
        &json!({"enabled": body.enabled}),
    )
    .await?;
    Ok(Json(json!(community)))
}

#[derive(Debug, Deserialize)]
pub struct CommunityValidationBody {
    pub outcome: String,
    pub evidence: String,
    pub notes: Option<String>,
}

#[utoipa::path(post, path = "/api/v1/communities/{id}/validate", tag = "communities", responses((status = 200, description = "Validation recorded"), (status = 422, description = "Evidence is required")))]
pub async fn validate_community(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<CommunityValidationBody>,
) -> AppResult<Json<serde_json::Value>> {
    let outcome = match body.outcome.as_str() {
        "validated" => ValidationStatus::Validated,
        "rejected" => ValidationStatus::Rejected,
        other => {
            return Err(AppError::Validation(vec![crate::error::FieldError::new(
                "outcome",
                format!("must be validated or rejected (got {other:?})"),
            )]))
        }
    };
    if body.evidence.trim().is_empty() {
        return Err(AppError::Validation(vec![crate::error::FieldError::new(
            "evidence",
            "record what was verified and where",
        )]));
    }
    let notes = match &body.notes {
        Some(notes) => format!("{} — evidence: {}", notes, body.evidence.trim()),
        None => format!("evidence: {}", body.evidence.trim()),
    };
    let community =
        sources::validate_community(&state.db, &context.tenant_id, &id, outcome, Some(&notes))
            .await?
            .ok_or(AppError::NotFound)?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "community_validated",
        "community",
        Some(&id),
        &json!({"outcome": outcome.as_str(), "evidence": body.evidence}),
    )
    .await?;
    Ok(Json(json!(community)))
}

#[utoipa::path(get, path = "/api/v1/sources", tag = "sources", responses((status = 200, description = "Configured sources")))]
pub async fn list_sources(
    State(state): State<SharedState>,
    context: TenantContext,
) -> AppResult<Json<serde_json::Value>> {
    let rows = sources::list_sources(&state.db, &context.tenant_id).await?;
    let total = rows.len() as i64;
    let pagination = Pagination {
        page: Some(1),
        per_page: Some(100),
    };
    Ok(Json(paged(json!(rows), &pagination, total)))
}

#[derive(Debug, Deserialize)]
pub struct NewSourceBody {
    pub name: String,
    pub provider: String,
    pub source_type: String,
    #[serde(default)]
    pub config: serde_json::Value,
}

#[utoipa::path(post, path = "/api/v1/sources", tag = "sources", responses((status = 201, description = "Source created"), (status = 422, description = "Invalid input")))]
pub async fn create_source(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<NewSourceBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    super::require_non_empty(&body.name, "name")?;
    super::require_one_of(
        &body.provider,
        "provider",
        &["apify", "manual_import", "official_api", "licensed_feed"],
    )?;
    super::require_one_of(
        &body.source_type,
        "source_type",
        &["community_feed", "manual_json", "webhook"],
    )?;
    let source = sources::create_source(
        &state.db,
        &context.tenant_id,
        &sources::NewSource {
            name: body.name.trim(),
            provider: &body.provider,
            source_type: &body.source_type,
            config: &body.config,
        },
    )
    .await
    .map_err(|error| {
        if let sqlx::Error::Database(ref db_error) = error {
            if db_error.is_unique_violation() {
                return AppError::Conflict("a source with that name already exists".to_string());
            }
        }
        AppError::Database(error)
    })?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "source_created",
        "source",
        Some(&source.id),
        &json!({"provider": body.provider}),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!(source))))
}

#[derive(Debug, Deserialize)]
pub struct SourcePatchBody {
    pub enabled: Option<bool>,
    pub config: Option<serde_json::Value>,
}

#[utoipa::path(patch, path = "/api/v1/sources/{id}", tag = "sources", responses((status = 200, description = "Updated source"), (status = 404, description = "Unknown source")))]
pub async fn patch_source(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<SourcePatchBody>,
) -> AppResult<Json<serde_json::Value>> {
    if body.enabled == Some(true) {
        let source = sources::get_source(&state.db, &context.tenant_id, &id)
            .await?
            .ok_or(AppError::NotFound)?;
        if source.validation_status != "validated" && source.provider != "manual_import" {
            return Err(AppError::Conflict(
                "source_not_validated: record validation before enabling this source".to_string(),
            ));
        }
    }
    let source = sources::patch_source(
        &state.db,
        &context.tenant_id,
        &id,
        &sources::SourcePatch {
            enabled: body.enabled,
            config: body.config.as_ref(),
        },
    )
    .await?
    .ok_or(AppError::NotFound)?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "source_updated",
        "source",
        Some(&id),
        &json!({"enabled": body.enabled}),
    )
    .await?;
    Ok(Json(json!(source)))
}

#[derive(Debug, Deserialize)]
pub struct SourceValidationBody {
    pub outcome: String,
    pub evidence: String,
    pub notes: Option<String>,
}

#[utoipa::path(post, path = "/api/v1/sources/{id}/validate", tag = "sources", responses((status = 200, description = "Validation recorded"), (status = 422, description = "Evidence is required")))]
pub async fn validate_source(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<SourceValidationBody>,
) -> AppResult<Json<serde_json::Value>> {
    let outcome = match body.outcome.as_str() {
        "validated" => ValidationStatus::Validated,
        "rejected" => ValidationStatus::Rejected,
        other => {
            return Err(AppError::Validation(vec![crate::error::FieldError::new(
                "outcome",
                format!("must be validated or rejected (got {other:?})"),
            )]))
        }
    };
    if body.evidence.trim().is_empty() {
        return Err(AppError::Validation(vec![crate::error::FieldError::new(
            "evidence",
            "record what was verified and where",
        )]));
    }
    let notes = match &body.notes {
        Some(notes) => format!("{} — evidence: {}", notes, body.evidence.trim()),
        None => format!("evidence: {}", body.evidence.trim()),
    };
    let source =
        sources::validate_source(&state.db, &context.tenant_id, &id, outcome, Some(&notes))
            .await?
            .ok_or(AppError::NotFound)?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "source_validated",
        "source",
        Some(&id),
        &json!({"outcome": outcome.as_str(), "evidence": body.evidence}),
    )
    .await?;
    Ok(Json(json!(source)))
}

#[utoipa::path(post, path = "/api/v1/sources/{id}/test", tag = "sources", responses((status = 200, description = "Provider reachability result")))]
pub async fn test_source(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let source = sources::get_source(&state.db, &context.tenant_id, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let health = if source.provider == "manual_import" {
        crate::providers::ProviderHealth::ok("manual_import", 0)
    } else {
        state.providers.source.health_check().await
    };
    operations::record_usage(
        &state.db,
        Some(&context.tenant_id),
        &operations::UsageRecord {
            provider: &source.provider,
            operation: "source_test",
            request_id: None,
            input_tokens: None,
            output_tokens: None,
            latency_ms: health.latency_ms,
            status: if health.status == "ok" {
                "succeeded"
            } else {
                "error"
            },
            error: health.message.as_deref(),
        },
    )
    .await?;
    Ok(Json(json!({
        "status": health.status,
        "message": health.message,
        "latency_ms": health.latency_ms,
    })))
}
