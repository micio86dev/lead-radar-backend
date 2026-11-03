//! Search profile CRUD (verticals, keywords, negative keywords).

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utoipa::IntoParams;

use super::{paged, AppError, AppResult, Pagination};
use crate::auth::TenantContext;
use crate::domain::enums::Vertical;
use crate::modules::{operations, sources};
use crate::state::SharedState;

#[derive(Debug, Deserialize, IntoParams)]
pub struct ProfileQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
}

#[utoipa::path(get, path = "/api/v1/search-profiles", tag = "search-profiles", responses((status = 200, description = "Search profiles")))]
pub async fn list(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<ProfileQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let profiles = sources::list_search_profiles(&state.db, &context.tenant_id).await?;
    let total = profiles.len() as i64;
    let page_items: Vec<serde_json::Value> = profiles
        .into_iter()
        .skip(pagination.offset() as usize)
        .take(pagination.limit() as usize)
        .map(|profile| json!(profile))
        .collect();
    Ok(Json(paged(json!(page_items), &pagination, total)))
}

#[derive(Debug, Deserialize)]
pub struct NewProfileBody {
    pub name: String,
    pub vertical: String,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub negative_keywords: Vec<String>,
}

fn validate_terms(terms: &[String], field: &str) -> AppResult<()> {
    if terms.len() > 200 {
        return Err(AppError::Validation(vec![crate::error::FieldError::new(
            field,
            "at most 200 terms are allowed",
        )]));
    }
    for term in terms {
        if term.trim().is_empty() || term.chars().count() > 120 {
            return Err(AppError::Validation(vec![crate::error::FieldError::new(
                field,
                "each term must be 1..=120 characters",
            )]));
        }
    }
    Ok(())
}

#[utoipa::path(post, path = "/api/v1/search-profiles", tag = "search-profiles", responses((status = 201, description = "Profile created"), (status = 422, description = "Invalid input")))]
pub async fn create(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<NewProfileBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if body.name.trim().is_empty() {
        return Err(AppError::Validation(vec![crate::error::FieldError::new(
            "name",
            "must not be empty",
        )]));
    }
    let vertical = Vertical::parse(&body.vertical).ok_or_else(|| {
        AppError::Validation(vec![crate::error::FieldError::new(
            "vertical",
            "unknown vertical",
        )])
    })?;
    validate_terms(&body.keywords, "keywords")?;
    validate_terms(&body.negative_keywords, "negative_keywords")?;

    let profile = sources::create_search_profile(
        &state.db,
        &context.tenant_id,
        &sources::NewSearchProfile {
            name: body.name.trim(),
            vertical,
            enabled: body.enabled.unwrap_or(true),
            keywords: &body.keywords,
            negative_keywords: &body.negative_keywords,
        },
    )
    .await?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "search_profile_created",
        "search_profile",
        Some(&profile.id),
        &json!({"vertical": vertical.as_str()}),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!(profile))))
}

#[utoipa::path(get, path = "/api/v1/search-profiles/{id}", tag = "search-profiles", responses((status = 200, description = "Search profile"), (status = 404, description = "Unknown profile")))]
pub async fn detail(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let profile = sources::get_search_profile(&state.db, &context.tenant_id, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!(profile)))
}

#[derive(Debug, Deserialize)]
pub struct ProfilePatchBody {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub keywords: Option<Vec<String>>,
    pub negative_keywords: Option<Vec<String>>,
}

#[utoipa::path(patch, path = "/api/v1/search-profiles/{id}", tag = "search-profiles", responses((status = 200, description = "Updated profile"), (status = 404, description = "Unknown profile")))]
pub async fn patch(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<ProfilePatchBody>,
) -> AppResult<Json<serde_json::Value>> {
    if let Some(keywords) = &body.keywords {
        validate_terms(keywords, "keywords")?;
    }
    if let Some(keywords) = &body.negative_keywords {
        validate_terms(keywords, "negative_keywords")?;
    }
    if let Some(name) = &body.name {
        if name.trim().is_empty() {
            return Err(AppError::Validation(vec![crate::error::FieldError::new(
                "name",
                "must not be empty",
            )]));
        }
    }
    let profile = sources::update_search_profile(
        &state.db,
        &context.tenant_id,
        &id,
        &sources::ProfilePatch {
            name: body.name.as_deref(),
            enabled: body.enabled,
            keywords: body.keywords.as_deref(),
            negative_keywords: body.negative_keywords.as_deref(),
        },
    )
    .await?
    .ok_or(AppError::NotFound)?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "search_profile_updated",
        "search_profile",
        Some(&id),
        &json!({"enabled": body.enabled}),
    )
    .await?;
    Ok(Json(json!(profile)))
}

#[utoipa::path(delete, path = "/api/v1/search-profiles/{id}", tag = "search-profiles", responses((status = 204, description = "Profile deleted"), (status = 404, description = "Unknown profile")))]
pub async fn remove(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let deleted = sources::delete_search_profile(&state.db, &context.tenant_id, &id).await?;
    if !deleted {
        return Err(AppError::NotFound);
    }
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "search_profile_deleted",
        "search_profile",
        Some(&id),
        &json!({}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
