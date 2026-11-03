//! AI provider status, usage, decision history, benchmarks and explicit health probes.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utoipa::IntoParams;

use super::{paged, AppError, AppResult, Pagination};
use crate::auth::TenantContext;
use crate::domain::time::{now, to_iso};
use crate::modules::{benchmark, decision, operations};
use crate::providers::ProviderHealth;
use crate::state::SharedState;

struct ProviderStats {
    last_request_at: Option<String>,
    last_latency_ms: Option<i64>,
    last_error: Option<String>,
    last_status: Option<String>,
    last_success_at: Option<String>,
    last_error_at: Option<String>,
    /// A recorded failure older than this window is history, not a current outage.
    stale: bool,
    requests: i64,
    input_tokens: i64,
    output_tokens: i64,
}

/// An error recorded longer ago than this is reported as stale rather than as a current failure.
const STALE_AFTER_HOURS: i64 = 24;

async fn stats_for(state: &SharedState, tenant: &str, provider: &str) -> ProviderStats {
    let last: Option<(String, Option<i64>, Option<String>, String)> = sqlx::query_as(
        "SELECT created_at, latency_ms, error, status FROM provider_usage \
         WHERE tenant_id = ? AND provider = ? ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant)
    .bind(provider)
    .fetch_optional(&state.db.reader)
    .await
    .unwrap_or(None);
    let aggregates: (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0) \
         FROM provider_usage WHERE tenant_id = ? AND provider = ?",
    )
    .bind(tenant)
    .bind(provider)
    .fetch_one(&state.db.reader)
    .await
    .unwrap_or((0, 0, 0));

    let last_success_at: Option<String> = sqlx::query_scalar(
        "SELECT MAX(created_at) FROM provider_usage \
         WHERE tenant_id = ? AND provider = ? AND status = 'succeeded'",
    )
    .bind(tenant)
    .bind(provider)
    .fetch_one(&state.db.reader)
    .await
    .unwrap_or(None);
    let last_error_at: Option<String> = sqlx::query_scalar(
        "SELECT MAX(created_at) FROM provider_usage \
         WHERE tenant_id = ? AND provider = ? AND status = 'error'",
    )
    .bind(tenant)
    .bind(provider)
    .fetch_one(&state.db.reader)
    .await
    .unwrap_or(None);

    let stale = last.as_ref().map(|row| row.3.as_str()) == Some("error")
        && last
            .as_ref()
            .and_then(|row| crate::domain::time::parse_iso(&row.0))
            .map(|when| (chrono::Utc::now() - when).num_hours() > STALE_AFTER_HOURS)
            .unwrap_or(false);

    ProviderStats {
        last_request_at: last.as_ref().map(|row| row.0.clone()),
        last_latency_ms: last.as_ref().and_then(|row| row.1),
        last_error: last.as_ref().and_then(|row| row.2.clone()),
        last_status: last.as_ref().map(|row| row.3.clone()),
        last_success_at,
        last_error_at,
        stale,
        requests: aggregates.0,
        input_tokens: aggregates.1,
        output_tokens: aggregates.2,
    }
}

fn status_of(configured: bool, enabled: bool, last_status: Option<&str>) -> &'static str {
    if !configured {
        "unconfigured"
    } else if !enabled {
        "disabled"
    } else if last_status == Some("error") {
        "error"
    } else {
        "ok"
    }
}

/// Provider status derived from configuration plus recorded usage. No live probe happens here: a
/// dashboard refresh must never spend money.
pub async fn provider_statuses(
    state: &SharedState,
    tenant: &str,
) -> AppResult<Vec<serde_json::Value>> {
    let mut statuses = Vec::new();

    let jev = &state.config.jev;
    let jev_stats = stats_for(state, tenant, "jev").await;
    statuses.push(json!({
        "provider": "jev",
        "kind": "decision",
        "configured": jev.configured(),
        "enabled": state.config.decision_provider == crate::config::DecisionChoice::Jev && jev.configured(),
        "model": jev.model,
        "status": status_of(jev.configured(), state.config.decision_provider == crate::config::DecisionChoice::Jev, jev_stats.last_status.as_deref()),
        "last_request_at": jev_stats.last_request_at,
        "last_latency_ms": jev_stats.last_latency_ms,
        "last_error": jev_stats.last_error,
        "last_success_at": jev_stats.last_success_at,
        "last_error_at": jev_stats.last_error_at,
        "stale": jev_stats.stale,
        "usage": {
            "requests": jev_stats.requests,
            "input_tokens": jev_stats.input_tokens,
            "output_tokens": jev_stats.output_tokens,
        },
        "base_url": jev.base_url,
    }));

    let laya = &state.config.laya;
    let laya_stats = stats_for(state, tenant, "laya").await;
    statuses.push(json!({
        "provider": "laya",
        "kind": "decision",
        "configured": laya.configured(),
        "enabled": laya.usable(),
        "model": laya.model,
        "status": status_of(laya.configured(), laya.usable(), laya_stats.last_status.as_deref()),
        "last_request_at": laya_stats.last_request_at,
        "last_latency_ms": laya_stats.last_latency_ms,
        "last_error": laya_stats.last_error,
        "last_success_at": laya_stats.last_success_at,
        "last_error_at": laya_stats.last_error_at,
        "stale": laya_stats.stale,
        "usage": {
            "requests": laya_stats.requests,
            "input_tokens": laya_stats.input_tokens,
            "output_tokens": laya_stats.output_tokens,
        },
        "base_url": laya.base_url,
    }));

    let qwen = &state.config.qwen;
    let qwen_stats = stats_for(state, tenant, "qwen").await;
    statuses.push(json!({
        "provider": "qwen",
        "kind": "generative",
        "configured": qwen.configured(),
        "enabled": state.config.generative_provider == crate::config::GenerativeChoice::Qwen && qwen.configured(),
        "model": qwen.model,
        "status": status_of(qwen.configured(), state.config.generative_provider == crate::config::GenerativeChoice::Qwen, qwen_stats.last_status.as_deref()),
        "last_request_at": qwen_stats.last_request_at,
        "last_latency_ms": qwen_stats.last_latency_ms,
        "last_error": qwen_stats.last_error,
        "last_success_at": qwen_stats.last_success_at,
        "last_error_at": qwen_stats.last_error_at,
        "stale": qwen_stats.stale,
        "usage": {
            "requests": qwen_stats.requests,
            "input_tokens": qwen_stats.input_tokens,
            "output_tokens": qwen_stats.output_tokens,
        },
        "base_url": qwen.base_url,
    }));

    let apify = &state.config.apify;
    let apify_stats = stats_for(state, tenant, "apify").await;
    statuses.push(json!({
        "provider": "apify",
        "kind": "source",
        "configured": apify.configured(),
        "enabled": state.config.source_provider == crate::config::SourceChoice::Apify && apify.configured(),
        "model": serde_json::Value::Null,
        "status": status_of(apify.configured(), state.config.source_provider == crate::config::SourceChoice::Apify, apify_stats.last_status.as_deref()),
        "last_request_at": apify_stats.last_request_at,
        "last_latency_ms": apify_stats.last_latency_ms,
        "last_error": apify_stats.last_error,
        "last_success_at": apify_stats.last_success_at,
        "last_error_at": apify_stats.last_error_at,
        "stale": apify_stats.stale,
        "usage": {
            "requests": apify_stats.requests,
            "input_tokens": apify_stats.input_tokens,
            "output_tokens": apify_stats.output_tokens,
        },
        "base_url": apify.base_url,
    }));

    Ok(statuses)
}

#[utoipa::path(get, path = "/api/v1/providers", tag = "providers", responses((status = 200, description = "Provider status")))]
pub async fn list(
    State(state): State<SharedState>,
    context: TenantContext,
) -> AppResult<Json<serde_json::Value>> {
    Ok(Json(json!(
        provider_statuses(&state, &context.tenant_id).await?
    )))
}

#[utoipa::path(get, path = "/api/v1/providers/usage", tag = "providers", responses((status = 200, description = "Provider cost summary")))]
pub async fn usage(
    State(state): State<SharedState>,
    context: TenantContext,
) -> AppResult<Json<serde_json::Value>> {
    let since = operations::days_ago(now(), 30);
    let mut summaries =
        operations::provider_cost_summary(&state.db, &context.tenant_id, since).await?;
    for summary in &mut summaries {
        summary.currency = "USD".to_string();
    }
    Ok(Json(json!({
        "items": summaries,
        "period": {
            "from": to_iso(since),
            "to": to_iso(now()),
        },
        "note": "unknown costs are counted separately and never shown as zero",
    })))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct DecisionQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
    #[serde(default)]
    pub provider: Option<String>,
}

#[utoipa::path(get, path = "/api/v1/providers/decisions", tag = "providers", responses((status = 200, description = "Recorded decisions")))]
pub async fn decisions(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<DecisionQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (rows, total) = decision::list_decisions(
        &state.db,
        &context.tenant_id,
        query.provider.as_deref(),
        pagination.limit(),
        pagination.offset(),
    )
    .await?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "lead_id": row.lead_id,
                "provider": row.provider,
                "model": row.model,
                "model_version": row.model_version,
                "confidence": row.confidence,
                "intent": row.intent,
                "vertical": row.vertical,
                "latency_ms": row.latency_ms,
                "created_at": row.created_at,
            })
        })
        .collect();
    Ok(Json(paged(json!(items), &pagination, total)))
}

#[utoipa::path(post, path = "/api/v1/providers/{provider}/test", tag = "providers", responses((status = 200, description = "Live health probe result"), (status = 400, description = "Unknown provider")))]
pub async fn test(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(provider): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let health: ProviderHealth = match provider.as_str() {
        "jev" => state.providers.decision.health_check().await,
        "laya" => match state.providers.laya.as_deref() {
            Some(laya) => laya.health_check().await,
            None => ProviderHealth {
                provider: "laya".to_string(),
                status: "disabled".to_string(),
                latency_ms: None,
                message: Some("LAYA_ENABLED is false or credentials are missing".to_string()),
            },
        },
        "qwen" => state.providers.generative.health_check().await,
        "apify" => state.providers.source.health_check().await,
        other => {
            return Err(AppError::BadRequest(format!(
                "unknown provider {other:?}; expected jev, laya, qwen or apify"
            )))
        }
    };

    operations::record_usage(
        &state.db,
        Some(&context.tenant_id),
        &operations::UsageRecord {
            provider: &health.provider,
            operation: "health_check",
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
        "provider": health.provider,
        "status": health.status,
        "latency_ms": health.latency_ms,
        "message": health.message,
    })))
}

#[derive(Debug, Deserialize)]
pub struct BenchmarkDatasetBody {
    pub dataset: String,
    pub items: Vec<BenchmarkItemBody>,
}

#[derive(Debug, Deserialize)]
pub struct BenchmarkItemBody {
    pub external_ref: String,
    pub text: String,
    pub language: String,
    pub expected_relevant: bool,
    pub expected_intent: String,
    pub expected_vertical: String,
    #[serde(default)]
    pub split: Option<String>,
}

#[utoipa::path(post, path = "/api/v1/providers/benchmark/dataset", tag = "providers", responses((status = 200, description = "Labeled examples stored"), (status = 422, description = "Invalid dataset")))]
pub async fn ingest_benchmark_dataset(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<BenchmarkDatasetBody>,
) -> AppResult<Json<serde_json::Value>> {
    let dataset = body.dataset.trim();
    let mut errors = Vec::new();
    if dataset.is_empty() || dataset.chars().count() > 64 {
        errors.push(crate::error::FieldError::new(
            "dataset",
            "must be 1..=64 characters",
        ));
    }
    if body.items.is_empty() || body.items.len() > 500 {
        errors.push(crate::error::FieldError::new(
            "items",
            "between 1 and 500 labeled examples are required",
        ));
    }
    for (index, item) in body.items.iter().enumerate() {
        if item.external_ref.trim().is_empty() || item.external_ref.chars().count() > 128 {
            errors.push(crate::error::FieldError::new(
                format!("items[{index}].external_ref"),
                "must be 1..=128 characters",
            ));
        }
        if item.text.trim().is_empty() || item.text.chars().count() > 20_000 {
            errors.push(crate::error::FieldError::new(
                format!("items[{index}].text"),
                "must be 1..=20000 characters",
            ));
        }
        if item.language.trim().is_empty() || item.language.chars().count() > 16 {
            errors.push(crate::error::FieldError::new(
                format!("items[{index}].language"),
                "must be a 1..=16 character language tag",
            ));
        }
        if crate::domain::enums::Intent::parse(&item.expected_intent).is_none() {
            errors.push(crate::error::FieldError::new(
                format!("items[{index}].expected_intent"),
                "unknown intent",
            ));
        }
        if crate::domain::enums::Vertical::parse(&item.expected_vertical).is_none() {
            errors.push(crate::error::FieldError::new(
                format!("items[{index}].expected_vertical"),
                "unknown vertical",
            ));
        }
        if let Some(split) = &item.split {
            if split != "tuning" && split != "evaluation" {
                errors.push(crate::error::FieldError::new(
                    format!("items[{index}].split"),
                    "must be tuning or evaluation",
                ));
            }
        }
    }
    if !errors.is_empty() {
        return Err(AppError::Validation(errors));
    }

    let mut stored = 0usize;
    for item in &body.items {
        sqlx::query(
            "INSERT INTO benchmark_items (id, tenant_id, dataset, external_ref, text, language, \
             expected_relevant, expected_intent, expected_vertical, split, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(dataset, external_ref) DO UPDATE SET text = excluded.text, \
             language = excluded.language, expected_relevant = excluded.expected_relevant, \
             expected_intent = excluded.expected_intent, \
             expected_vertical = excluded.expected_vertical, split = excluded.split",
        )
        .bind(crate::modules::new_id())
        .bind(&context.tenant_id)
        .bind(dataset)
        .bind(item.external_ref.trim())
        .bind(&item.text)
        .bind(item.language.trim())
        .bind(if item.expected_relevant { 1 } else { 0 })
        .bind(&item.expected_intent)
        .bind(&item.expected_vertical)
        .bind(item.split.as_deref().unwrap_or("evaluation"))
        .bind(crate::domain::time::now_iso())
        .execute(&state.db.writer)
        .await?;
        stored += 1;
    }

    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "benchmark_dataset_ingested",
        "benchmark_dataset",
        Some(dataset),
        &json!({"items": stored}),
    )
    .await?;

    Ok(Json(json!({"dataset": dataset, "stored": stored})))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct BenchmarkQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
}

#[utoipa::path(get, path = "/api/v1/providers/benchmark", tag = "providers", responses((status = 200, description = "Benchmark runs")))]
pub async fn list_benchmarks(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<BenchmarkQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let (rows, total) = benchmark::list_runs(
        &state.db,
        &context.tenant_id,
        pagination.limit(),
        pagination.offset(),
    )
    .await?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.id,
                "dataset": row.dataset,
                "status": row.status,
                "providers": serde_json::from_str::<serde_json::Value>(&row.providers).unwrap_or_default(),
                "item_count": row.item_count,
                "error": row.error,
                "created_at": row.created_at,
                "finished_at": row.finished_at,
            })
        })
        .collect();
    Ok(Json(paged(json!(items), &pagination, total)))
}

#[derive(Debug, Deserialize)]
pub struct NewBenchmarkBody {
    #[serde(default = "default_dataset")]
    pub dataset: String,
    #[serde(default = "default_providers")]
    pub providers: Vec<String>,
}

fn default_dataset() -> String {
    "seed".to_string()
}

fn default_providers() -> Vec<String> {
    vec!["jev".to_string(), "laya".to_string()]
}

#[utoipa::path(post, path = "/api/v1/providers/benchmark", tag = "providers", responses((status = 202, description = "Benchmark queued"), (status = 422, description = "Invalid providers")))]
pub async fn start_benchmark(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<NewBenchmarkBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if body.providers.is_empty() {
        return Err(AppError::Validation(vec![crate::error::FieldError::new(
            "providers",
            "name at least one provider",
        )]));
    }
    for provider in &body.providers {
        super::require_one_of(provider, "providers", &["jev", "laya"])?;
    }
    let run_id = benchmark::create_run(
        &state.db,
        &context.tenant_id,
        body.dataset.trim(),
        &body.providers,
    )
    .await?;
    let job_id = benchmark::enqueue(
        &state,
        &context.tenant_id,
        &run_id,
        body.dataset.trim(),
        &body.providers,
    )
    .await?;
    operations::audit(
        &state.db,
        &context.tenant_id,
        &context.actor,
        "benchmark_queued",
        "benchmark_run",
        Some(&run_id),
        &json!({"dataset": body.dataset, "providers": body.providers}),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"job_id": job_id, "benchmark_run_id": run_id})),
    ))
}

#[utoipa::path(get, path = "/api/v1/providers/benchmark/{id}", tag = "providers", responses((status = 200, description = "Benchmark detail"), (status = 404, description = "Unknown benchmark")))]
pub async fn get_benchmark(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let (run, results) = benchmark::get_run(&state.db, &context.tenant_id, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    Ok(Json(json!({
        "id": run.id,
        "dataset": run.dataset,
        "status": run.status,
        "providers": serde_json::from_str::<serde_json::Value>(&run.providers).unwrap_or_default(),
        "item_count": run.item_count,
        "error": run.error,
        "created_at": run.created_at,
        "finished_at": run.finished_at,
        "results": results.iter().map(benchmark::result_json).collect::<Vec<_>>(),
    })))
}
