//! Manual JSON import. This path works independently of Apify and of every AI provider.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use crate::auth::TenantContext;
use crate::domain::time::parse_iso;
use crate::error::{AppError, AppResult, FieldError};
use crate::modules::{ingestion, operations};
use crate::state::SharedState;

const MAX_POSTS_PER_IMPORT: usize = 500;
const MAX_CONTENT_CHARS: usize = 20_000;

#[derive(Debug, Deserialize)]
pub struct ImportBody {
    #[serde(default)]
    pub source_name: Option<String>,
    pub posts: Vec<ingestion::ImportPost>,
}

fn validate(body: &ImportBody) -> AppResult<()> {
    let mut errors = Vec::new();
    if body.posts.is_empty() {
        errors.push(FieldError::new("posts", "at least one post is required"));
    }
    if body.posts.len() > MAX_POSTS_PER_IMPORT {
        errors.push(FieldError::new(
            "posts",
            format!("at most {MAX_POSTS_PER_IMPORT} posts per import"),
        ));
    }
    for (index, post) in body.posts.iter().enumerate() {
        if post.content.trim().is_empty() {
            errors.push(FieldError::new(
                format!("posts[{index}].content"),
                "must not be empty",
            ));
        }
        if post.content.chars().count() > MAX_CONTENT_CHARS {
            errors.push(FieldError::new(
                format!("posts[{index}].content"),
                format!("must be at most {MAX_CONTENT_CHARS} characters"),
            ));
        }
        if let Some(published_at) = &post.published_at {
            if parse_iso(published_at).is_none() {
                errors.push(FieldError::new(
                    format!("posts[{index}].published_at"),
                    "must be an RFC 3339 timestamp",
                ));
            }
        }
        if let Some(engagement) = &post.engagement {
            if engagement.comments < 0 || engagement.upvotes < 0 || engagement.shares < 0 {
                errors.push(FieldError::new(
                    format!("posts[{index}].engagement"),
                    "counts must not be negative",
                ));
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(AppError::Validation(errors))
    }
}

#[utoipa::path(post, path = "/api/v1/import/json", tag = "import", responses((status = 202, description = "Import accepted"), (status = 422, description = "Invalid payload")))]
pub async fn import_json(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<ImportBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    validate(&body)?;
    let tenant = &context.tenant_id;
    let source_id = ingestion::ensure_manual_source(&state.db, tenant).await?;
    let outcome = ingestion::import_posts(
        &state.db,
        tenant,
        Some(&source_id),
        &body.posts,
        &context.actor,
        state.config.worker.max_attempts,
        "manual_json",
    )
    .await?;
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "manual_import",
        "ingestion_run",
        Some(&outcome.ingestion_run_id),
        &json!({
            "source_name": body.source_name,
            "imported": outcome.imported,
            "duplicates": outcome.duplicates,
            "rejected": outcome.rejected,
        }),
    )
    .await?;
    operations::record_usage(
        &state.db,
        Some(tenant),
        &operations::UsageRecord {
            provider: "manual_import",
            operation: "import_json",
            request_id: Some(&outcome.ingestion_run_id),
            input_tokens: None,
            output_tokens: None,
            latency_ms: None,
            status: "succeeded",
            error: None,
        },
    )
    .await?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "ingestion_run_id": outcome.ingestion_run_id,
            "imported": outcome.imported,
            "duplicates": outcome.duplicates,
            "rejected": outcome.rejected,
            "rejected_reasons": outcome.rejected_reasons,
            "job_ids": outcome.normalization_job_id.into_iter().collect::<Vec<_>>(),
        })),
    ))
}
