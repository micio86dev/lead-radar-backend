//! Manual JSON import. This path works independently of Apify and of every AI provider.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use validator::Validate;

use crate::auth::TenantContext;
use crate::error::{AppError, AppResult, FieldError};
use crate::modules::{ingestion, operations};
use crate::state::SharedState;

const MAX_POSTS_PER_IMPORT: u64 = 500;

#[derive(Debug, Deserialize, Validate)]
pub struct ImportBody {
    #[validate(length(max = 100))]
    #[serde(default)]
    pub source_name: Option<String>,
    #[validate(length(min = 1, max = 500), nested)]
    pub posts: Vec<ingestion::ImportPost>,
}

/// Map validator errors onto the API's field-level error format.
fn validate(body: &ImportBody) -> AppResult<()> {
    body.validate().map_err(|errors| {
        let mut fields: Vec<FieldError> = errors
            .field_errors()
            .iter()
            .flat_map(|(field, problems)| {
                problems.iter().map(move |problem| {
                    let message = problem
                        .message
                        .as_ref()
                        .map(|message| message.to_string())
                        .unwrap_or_else(|| problem.code.to_string());
                    FieldError::new(field.to_string(), message)
                })
            })
            .collect();
        fields.sort_by(|left, right| left.field.cmp(&right.field));
        AppError::Validation(fields)
    })?;
    if body.posts.len() as u64 > MAX_POSTS_PER_IMPORT {
        return Err(AppError::Validation(vec![FieldError::new(
            "posts",
            format!("at most {MAX_POSTS_PER_IMPORT} posts per import"),
        )]));
    }
    Ok(())
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
