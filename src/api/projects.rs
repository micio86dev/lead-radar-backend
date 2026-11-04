//! Projects: the product a tenant works on, and the community ranking computed from it.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::auth::TenantContext;
use crate::error::{AppError, AppResult};
use crate::modules::projects::{self, NewProject, Project, ProjectError, ProjectPatch};
use crate::state::SharedState;

fn project_json(project: &Project) -> serde_json::Value {
    json!({
        "id": project.id,
        "name": project.name,
        "summary": project.summary,
        "website_urls": project.urls(),
        "is_default": project.is_default,
        "created_at": project.created_at,
        "updated_at": project.updated_at,
    })
}

fn map_error(error: ProjectError) -> AppError {
    match error {
        ProjectError::NotFound => AppError::NotFound,
        ProjectError::Invalid(message) => {
            AppError::Validation(vec![crate::error::FieldError::new("project", message)])
        }
        ProjectError::NoCandidates => AppError::Conflict(
            "no validated and enabled community to match against: validate and enable at least one \
             community first"
                .to_string(),
        ),
        ProjectError::Provider(error) => AppError::Provider(error.to_string()),
        ProjectError::Unusable(message) => AppError::Conflict(format!(
            "the analysis returned nothing usable: {message}"
        )),
        ProjectError::Database(error) => AppError::Database(error),
    }
}

#[utoipa::path(get, path = "/api/v1/projects", tag = "projects", responses((status = 200, description = "Projects of the tenant")))]
pub async fn list(
    State(state): State<SharedState>,
    context: TenantContext,
) -> AppResult<Json<serde_json::Value>> {
    let items = projects::list_projects(&state.db, &context.tenant_id).await?;
    let body = items.iter().map(project_json).collect::<Vec<_>>();
    Ok(Json(json!({ "items": body, "total": body.len() })))
}

#[derive(Debug, Deserialize)]
pub struct NewProjectBody {
    pub name: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub website_urls: Vec<String>,
}

#[utoipa::path(post, path = "/api/v1/projects", tag = "projects", responses((status = 201, description = "Project created"), (status = 422, description = "Invalid input")))]
pub async fn create(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<NewProjectBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let project = projects::create_project(
        &state.db,
        &context.tenant_id,
        &NewProject {
            name: &body.name,
            summary: &body.summary,
            website_urls: &body.website_urls,
        },
    )
    .await
    .map_err(map_error)?;
    Ok((StatusCode::CREATED, Json(project_json(&project))))
}

#[derive(Debug, Deserialize)]
pub struct ProjectPatchBody {
    pub name: Option<String>,
    pub summary: Option<String>,
    pub website_urls: Option<Vec<String>>,
    pub is_default: Option<bool>,
}

#[utoipa::path(get, path = "/api/v1/projects/{id}", tag = "projects", responses((status = 200, description = "Project"), (status = 404, description = "Unknown project")))]
pub async fn detail(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let project = projects::get_project(&state.db, &context.tenant_id, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({
        "project": project_json(&project),
        "communities": projects::project_matches(&state.db, &context.tenant_id, &id).await?,
    })))
}

#[utoipa::path(patch, path = "/api/v1/projects/{id}", tag = "projects", responses((status = 200, description = "Updated project"), (status = 404, description = "Unknown project")))]
pub async fn patch(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<ProjectPatchBody>,
) -> AppResult<Json<serde_json::Value>> {
    let project = projects::patch_project(
        &state.db,
        &context.tenant_id,
        &id,
        &ProjectPatch {
            name: body.name.as_deref(),
            summary: body.summary.as_deref(),
            website_urls: body.website_urls.as_deref(),
            is_default: body.is_default,
        },
    )
    .await
    .map_err(map_error)?;
    Ok(Json(project_json(&project)))
}

#[utoipa::path(delete, path = "/api/v1/projects/{id}", tag = "projects", responses((status = 204, description = "Deleted"), (status = 404, description = "Unknown project")))]
pub async fn remove(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let deleted = projects::delete_project(&state.db, &context.tenant_id, &id)
        .await
        .map_err(map_error)?;
    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound)
    }
}

#[utoipa::path(post, path = "/api/v1/projects/{id}/match-communities", tag = "projects", responses((status = 200, description = "Community ranking stored"), (status = 409, description = "No candidate community"), (status = 502, description = "Provider failure")))]
pub async fn match_communities(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let outcome = projects::analyze_communities(&state, &context.tenant_id, &id)
        .await
        .map_err(map_error)?;
    let matches = projects::project_matches(&state.db, &context.tenant_id, &id).await?;
    Ok(Json(json!({
        "project_id": outcome.project_id,
        "candidates": outcome.candidates,
        "matched": outcome.matched,
        "provider": outcome.provider,
        "model": outcome.model,
        "pages": outcome.pages.iter().map(|page| json!({
            "url": page.url,
            "read": page.text.is_some(),
            "error": page.error,
        })).collect::<Vec<_>>(),
        "communities": matches,
    })))
}
