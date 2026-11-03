//! Apify Control Center: Actor registry, run lifecycle, spending and the cost guard.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utoipa::IntoParams;

use super::{paged, AppError, AppResult, Pagination};
use crate::auth::TenantContext;
use crate::domain::enums::JobKind;
use crate::modules::{operations, sources};
use crate::state::SharedState;

#[utoipa::path(get, path = "/api/v1/providers/apify/actors", tag = "apify", responses((status = 200, description = "Actor registry")))]
pub async fn list_actors(
    State(state): State<SharedState>,
    context: TenantContext,
) -> AppResult<Json<serde_json::Value>> {
    let actors = sources::list_actors(&state.db, &context.tenant_id).await?;
    let total = actors.len() as i64;
    let pagination = Pagination {
        page: Some(1),
        per_page: Some(100),
    };
    Ok(Json(paged(json!(actors), &pagination, total)))
}

#[derive(Debug, Deserialize)]
pub struct NewActorBody {
    pub actor_id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub source_type: String,
    #[serde(default)]
    pub input_template: serde_json::Value,
    pub max_items_per_run: Option<i64>,
    pub max_run_cost_eur: Option<f64>,
    pub max_total_charge_usd: Option<f64>,
}

#[utoipa::path(post, path = "/api/v1/providers/apify/actors", tag = "apify", responses((status = 201, description = "Actor registered disabled and pending validation"), (status = 422, description = "Invalid input")))]
pub async fn create_actor(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<NewActorBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    super::require_non_empty(&body.actor_id, "actor_id")?;
    super::require_non_empty(&body.name, "name")?;
    let max_items = body
        .max_items_per_run
        .unwrap_or(state.config.apify.max_items_per_run)
        .clamp(1, state.config.apify.max_items_per_run);
    if let Some(cost) = body.max_run_cost_eur {
        if !cost.is_finite() || cost < 0.0 || cost > state.config.apify.max_run_cost_eur {
            return Err(AppError::Validation(vec![crate::error::FieldError::new(
                "max_run_cost_eur",
                format!("must be within 0..={}", state.config.apify.max_run_cost_eur),
            )]));
        }
    }
    let actor = sources::create_actor(
        &state.db,
        &context.tenant_id,
        &sources::NewActor {
            actor_id: body.actor_id.trim(),
            name: body.name.trim(),
            description: &body.description,
            source_type: &body.source_type,
            input_template: &body.input_template,
            max_items_per_run: max_items,
            cost_per_run_estimate: body.max_run_cost_eur,
            max_total_charge_usd: Some(
                body.max_total_charge_usd
                    .unwrap_or(state.config.apify.max_total_charge_usd),
            ),
        },
    )
    .await?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "actor_registered",
        "actor_registry",
        Some(&actor.id),
        &json!({"actor_id": actor.actor_id}),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(json!(actor))))
}

#[derive(Debug, Deserialize)]
pub struct ActorPatchBody {
    pub enabled: Option<bool>,
    pub validation_status: Option<String>,
    pub description: Option<String>,
    pub max_items_per_run: Option<i64>,
    pub max_total_charge_usd: Option<f64>,
    pub input_template: Option<serde_json::Value>,
}

#[utoipa::path(patch, path = "/api/v1/providers/apify/actors/{id}", tag = "apify", responses((status = 200, description = "Updated Actor"), (status = 409, description = "Cannot enable an unvalidated Actor")))]
pub async fn patch_actor(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<ActorPatchBody>,
) -> AppResult<Json<serde_json::Value>> {
    let tenant = &context.tenant_id;
    let actor = sources::get_actor(&state.db, tenant, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let validation = match body.validation_status.as_deref() {
        Some(raw) => Some(
            crate::domain::enums::ValidationStatus::parse(raw).ok_or_else(|| {
                AppError::BadRequest(format!("unknown validation_status {raw:?}"))
            })?,
        ),
        None => None,
    };
    let effective_validation = validation
        .as_ref()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| actor.validation_status.clone());
    if body.enabled == Some(true) && effective_validation != "validated" {
        return Err(AppError::Conflict(
            "actor_not_validated: record validation before enabling this Actor".to_string(),
        ));
    }
    if let Some(items) = body.max_items_per_run {
        if items < 1 || items > state.config.apify.max_items_per_run {
            return Err(AppError::Validation(vec![crate::error::FieldError::new(
                "max_items_per_run",
                format!(
                    "must be within 1..={}",
                    state.config.apify.max_items_per_run
                ),
            )]));
        }
    }
    let updated = sources::update_actor(
        &state.db,
        tenant,
        &id,
        &sources::ActorPatch {
            enabled: body.enabled,
            validation_status: validation,
            description: body.description.as_deref(),
            max_items_per_run: body.max_items_per_run,
            max_total_charge_usd: body.max_total_charge_usd,
            input_template: body.input_template.as_ref(),
        },
    )
    .await?
    .ok_or(AppError::NotFound)?;
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "actor_updated",
        "actor_registry",
        Some(&id),
        &json!({"enabled": body.enabled, "validation_status": body.validation_status}),
    )
    .await?;
    Ok(Json(json!(updated)))
}

#[derive(Debug, Deserialize)]
pub struct StartRunBody {
    pub actor_registry_id: String,
    #[serde(default)]
    pub input_override: Option<serde_json::Value>,
    pub items_limit: Option<i64>,
}

#[utoipa::path(post, path = "/api/v1/providers/apify/runs", tag = "apify", responses((status = 202, description = "Run queued"), (status = 409, description = "Refused by the cost guard")))]
pub async fn start_run(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<StartRunBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let tenant = &context.tenant_id;
    let actor = sources::get_actor(&state.db, tenant, &body.actor_registry_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let items_limit = body
        .items_limit
        .unwrap_or(actor.max_items_per_run)
        .min(state.config.apify.max_items_per_run);

    sources::guard_apify_run(&state.db, tenant, &state.config, &actor, items_limit)
        .await
        .map_err(AppError::CostGuardRejected)?;

    let input = match &body.input_override {
        Some(override_value) => override_value.clone(),
        None => actor.input_template.clone(),
    };
    let (job_id, _) = operations::enqueue(
        &state.db,
        Some(tenant),
        JobKind::ActorRun,
        &json!({"pending": true}),
        None,
        state.config.worker.max_attempts,
    )
    .await?;
    let run_id = sources::insert_apify_run(
        &state.db,
        tenant,
        &actor.id,
        &job_id,
        &json!({"input": input, "items_limit": items_limit}),
    )
    .await?;
    sqlx::query("UPDATE jobs SET payload = ? WHERE id = ?")
        .bind(json!({"apify_run_id": run_id}).to_string())
        .bind(&job_id)
        .execute(&state.db.writer)
        .await?;
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "apify_run_queued",
        "apify_run",
        Some(&run_id),
        &json!({"actor_id": actor.actor_id, "items_limit": items_limit}),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"job_id": job_id, "run_id": run_id, "status": "queued"})),
    ))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct RunQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub actor_registry_id: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/providers/apify/runs", tag = "apify", responses((status = 200, description = "Apify runs")))]
pub async fn list_runs(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<RunQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (rows, total) = sources::list_apify_runs(
        &state.db,
        &context.tenant_id,
        query.status.as_deref(),
        query.actor_registry_id.as_deref(),
        pagination.limit(),
        pagination.offset(),
    )
    .await?;
    Ok(Json(paged(json!(rows), &pagination, total)))
}

#[utoipa::path(get, path = "/api/v1/providers/apify/runs/{id}", tag = "apify", responses((status = 200, description = "Apify run detail"), (status = 404, description = "Unknown run")))]
pub async fn get_run(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let run = sources::get_apify_run(&state.db, &context.tenant_id, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({
        "id": run.id,
        "actor_registry_id": run.actor_registry_id,
        "actor_name": run.actor_name,
        "status": run.status,
        "items_count": run.items_count,
        "dataset_id": run.dataset_id,
        "cost_estimate_usd": run.cost_estimate_usd,
        "cost_actual_usd": run.cost_actual_usd,
        "error": run.error,
        "started_at": run.started_at,
        "finished_at": run.finished_at,
        "job_id": run.job_id,
        "created_at": run.created_at,
    })))
}

#[utoipa::path(post, path = "/api/v1/providers/apify/runs/{id}/cancel", tag = "apify", responses((status = 202, description = "Cancellation requested"), (status = 404, description = "Unknown run")))]
pub async fn cancel_run(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let tenant = &context.tenant_id;
    let run = sources::get_apify_run(&state.db, tenant, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    if matches!(
        run.status.as_str(),
        "succeeded" | "failed" | "aborted" | "timed_out"
    ) {
        return Err(AppError::Conflict(
            "run already finished; nothing to cancel".to_string(),
        ));
    }
    if let Some(provider_run_id) = sqlx::query_scalar::<_, String>(
        "SELECT provider_run_id FROM apify_runs WHERE tenant_id = ? AND id = ? \
         AND provider_run_id IS NOT NULL",
    )
    .bind(tenant)
    .bind(&id)
    .fetch_optional(&state.db.reader)
    .await?
    {
        state
            .providers
            .source
            .cancel_run(&provider_run_id)
            .await
            .map_err(|error| AppError::Provider(error.to_string()))?;
    }
    sources::update_apify_run(&state.db, &id, "aborted", None, None, None, None, None).await?;
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "apify_run_cancelled",
        "apify_run",
        Some(&id),
        &json!({"previous_status": run.status}),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"status": "aborted"}))))
}

#[utoipa::path(get, path = "/api/v1/providers/apify/usage", tag = "apify", responses((status = 200, description = "Budget and usage")))]
pub async fn usage(
    State(state): State<SharedState>,
    context: TenantContext,
) -> AppResult<Json<serde_json::Value>> {
    let usage = sources::apify_usage(&state.db, &context.tenant_id).await?;
    let cost_source = if usage.actual_spend_usd.is_some() {
        "actual"
    } else if usage.estimate_spend_usd.is_some() {
        "estimate"
    } else {
        "unknown"
    };
    Ok(Json(json!({
        "budget_eur": state.config.apify.total_budget_eur,
        "daily_budget_eur": state.config.apify.daily_budget_eur,
        "max_run_cost_eur": state.config.apify.max_run_cost_eur,
        "max_concurrent_runs": state.config.apify.max_concurrent_runs,
        "max_items_per_run": state.config.apify.max_items_per_run,
        "known_actual_usd": usage.actual_spend_usd,
        "estimated_usd": usage.estimate_spend_usd,
        "remaining_known_eur": if usage.actual_spend_usd.is_some() {
            serde_json::Value::Null
        } else {
            json!(state.config.apify.total_budget_eur)
        },
        "runs_total": usage.runs_total,
        "runs_running": usage.runs_running,
        "results_total": usage.results_total,
        "errors_total": usage.errors_total,
        "kill_switch": state.config.apify.kill_switch,
        "cost_source": cost_source,
        "currency_note": "configured ceilings are EUR; Apify reports spend in USD and both are shown separately",
    })))
}
