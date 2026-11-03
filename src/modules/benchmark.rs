//! Reproducible provider benchmark: the same labeled dataset, every provider, stored metrics.
//!
//! Labels are human ground truth. Examples are split into `evaluation` and `tuning`; this pipeline
//! only ever evaluates the `evaluation` split, and the split is stored with the labels so a later
//! comparison cannot silently reuse tuning data.

use std::time::Instant;

use serde_json::{json, Value};
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::benchmark::{evaluate, BenchmarkMetrics};
use crate::domain::decision::{lead_questions, DecisionResult};
use crate::domain::enums::JobKind;
use crate::domain::time::now_iso;
use crate::modules::{new_id, operations, to_json};
use crate::providers::{DecisionProvider, DecisionRequest};
use crate::state::AppState;

#[derive(Debug, Clone, FromRow)]
pub struct BenchmarkItemRow {
    pub external_ref: String,
    pub text: String,
    pub language: String,
    pub expected_relevant: i64,
    pub expected_intent: String,
    pub expected_vertical: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BenchmarkError {
    #[error("benchmark run not found")]
    NotFound,
    #[error("no provider available for this benchmark: {0}")]
    NoProvider(String),
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error("invalid benchmark: {0}")]
    Invalid(String),
}

/// Create a queued benchmark run. Providers are named explicitly; nothing is auto-selected.
pub async fn create_run(
    db: &Db,
    tenant_id: &str,
    dataset: &str,
    providers: &[String],
) -> Result<String, sqlx::Error> {
    let id = new_id();
    sqlx::query(
        "INSERT INTO benchmark_runs (id, tenant_id, dataset, status, providers, item_count, \
         created_at) VALUES (?, ?, ?, 'queued', ?, 0, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(dataset)
    .bind(to_json(&providers))
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(id)
}

pub async fn enqueue(
    state: &AppState,
    tenant_id: &str,
    benchmark_run_id: &str,
    dataset: &str,
    providers: &[String],
) -> Result<String, sqlx::Error> {
    let (job_id, _) = operations::enqueue(
        &state.db,
        Some(tenant_id),
        JobKind::BenchmarkExecution,
        &json!({
            "benchmark_run_id": benchmark_run_id,
            "dataset": dataset,
            "providers": providers,
        }),
        Some(&format!("benchmark:{benchmark_run_id}")),
        state.config.worker.max_attempts,
    )
    .await?;
    Ok(job_id)
}

fn provider_by_name<'a>(state: &'a AppState, name: &str) -> Option<&'a dyn DecisionProvider> {
    // "jev" is the production role; in development the deterministic mock stands in for it, and the
    // stored result keeps the real adapter name.
    match name {
        "jev" | "mock" => Some(state.providers.decision.as_ref()),
        "laya" => state
            .providers
            .laya
            .as_deref()
            .filter(|provider| provider.metadata().provider == "laya"),
        _ => None,
    }
}

/// A compact, comparable prediction: relevant when the provider is confident about relevance and
/// reports a commercial intent.
fn predict(result: &DecisionResult) -> bool {
    let relevance = result.noul("relevance").unwrap_or(0.0);
    let intent = result.choice("intent").unwrap_or("irrelevant");
    relevance >= 0.5
        && matches!(
            intent,
            "explicit_solution_request"
                | "active_problem"
                | "latent_problem"
                | "product_comparison"
        )
}

pub async fn execute(
    state: &AppState,
    tenant_id: &str,
    benchmark_run_id: &str,
) -> Result<usize, BenchmarkError> {
    let run: Option<(String, String)> = sqlx::query_as(
        "SELECT dataset, providers FROM benchmark_runs WHERE tenant_id = ? AND id = ?",
    )
    .bind(tenant_id)
    .bind(benchmark_run_id)
    .fetch_optional(&state.db.reader)
    .await?;
    let (dataset, providers_raw) = run.ok_or(BenchmarkError::NotFound)?;
    let providers: Vec<String> = serde_json::from_str(&providers_raw).unwrap_or_default();

    sqlx::query("UPDATE benchmark_runs SET status = 'running' WHERE id = ?")
        .bind(benchmark_run_id)
        .execute(&state.db.writer)
        .await?;

    let items = sqlx::query_as::<_, BenchmarkItemRow>(
        "SELECT external_ref, text, language, expected_relevant, expected_intent, expected_vertical \
         FROM benchmark_items WHERE dataset = ? AND split = 'evaluation' ORDER BY external_ref",
    )
    .bind(&dataset)
    .fetch_all(&state.db.reader)
    .await?;
    if items.is_empty() {
        sqlx::query(
            "UPDATE benchmark_runs SET status = 'failed', error = ?, finished_at = ? WHERE id = ?",
        )
        .bind(format!("dataset {dataset} has no evaluation items"))
        .bind(now_iso())
        .bind(benchmark_run_id)
        .execute(&state.db.writer)
        .await?;
        return Err(BenchmarkError::Invalid(format!(
            "dataset {dataset} has no evaluation items"
        )));
    }
    sqlx::query("UPDATE benchmark_runs SET item_count = ? WHERE id = ?")
        .bind(items.len() as i64)
        .bind(benchmark_run_id)
        .execute(&state.db.writer)
        .await?;

    let mut stored = 0usize;
    let mut last_error = None;
    for name in &providers {
        let Some(provider) = provider_by_name(state, name) else {
            last_error = Some(format!("provider {name} is not available for benchmarking"));
            continue;
        };
        let metadata = provider.metadata();
        if !metadata.configured {
            last_error = Some(format!("provider {name} is not configured"));
            continue;
        }

        let labels: Vec<bool> = items
            .iter()
            .map(|item| item.expected_relevant == 1)
            .collect();
        let languages: Vec<String> = items.iter().map(|item| item.language.clone()).collect();
        let mut predictions = Vec::with_capacity(items.len());
        let mut latencies = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let request = DecisionRequest {
                state: item.text.clone(),
                questions: lead_questions(),
                idempotency_key: Some(format!("benchmark:{benchmark_run_id}:{name}:{index}")),
            };
            let started = Instant::now();
            match provider.evaluate(&request).await {
                Ok(result) => {
                    predictions.push(predict(&result));
                    latencies.push(result.latency_ms);
                    operations::record_usage(
                        &state.db,
                        Some(tenant_id),
                        &operations::UsageRecord {
                            provider: name,
                            operation: "benchmark",
                            request_id: result.provider_request_id.as_deref(),
                            input_tokens: result.usage.input_tokens,
                            output_tokens: result.usage.output_tokens,
                            latency_ms: Some(result.latency_ms),
                            status: "succeeded",
                            error: None,
                        },
                    )
                    .await?;
                }
                Err(error) => {
                    last_error = Some(format!("provider {name} failed: {error}"));
                    operations::record_usage(
                        &state.db,
                        Some(tenant_id),
                        &operations::UsageRecord {
                            provider: name,
                            operation: "benchmark",
                            request_id: None,
                            input_tokens: None,
                            output_tokens: None,
                            latency_ms: Some(started.elapsed().as_millis() as i64),
                            status: "error",
                            error: Some(&error.to_string()),
                        },
                    )
                    .await?;
                    break;
                }
            }
        }

        if predictions.len() != items.len() {
            continue;
        }
        let metrics: BenchmarkMetrics =
            evaluate(&labels, &predictions, &languages).map_err(BenchmarkError::Invalid)?;
        let labels_json = labels
            .iter()
            .zip(&items)
            .map(|(label, item)| {
                json!({
                    "ref": item.external_ref,
                    "expected_relevant": label,
                    "expected_intent": item.expected_intent,
                    "expected_vertical": item.expected_vertical,
                    "language": item.language,
                })
            })
            .collect::<Vec<_>>();
        let predictions_json: Vec<Value> = predictions
            .iter()
            .map(|prediction| json!(prediction))
            .collect();
        let latency_avg = if latencies.is_empty() {
            None
        } else {
            Some(latencies.iter().sum::<i64>() as f64 / latencies.len() as f64)
        };

        sqlx::query(
            "INSERT INTO benchmark_results (id, benchmark_run_id, provider, model, model_version, \
             labels, predictions, metrics, latency_ms_avg, cost_per_1000_usd, unknown_costs, \
             created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?) \
             ON CONFLICT(benchmark_run_id, provider) DO UPDATE SET labels = excluded.labels, \
             predictions = excluded.predictions, metrics = excluded.metrics, \
             latency_ms_avg = excluded.latency_ms_avg, unknown_costs = excluded.unknown_costs",
        )
        .bind(new_id())
        .bind(benchmark_run_id)
        .bind(&metadata.provider)
        .bind(
            metadata
                .model
                .clone()
                .unwrap_or_else(|| "unknown".to_string()),
        )
        .bind(Option::<String>::None)
        .bind(to_json(&labels_json))
        .bind(to_json(&predictions_json))
        .bind(to_json(&metrics))
        .bind(latency_avg)
        // The provider's unit price is not part of this system's configuration, so the cost per
        // 1000 posts is reported as unknown rather than invented.
        .bind(items.len() as i64)
        .bind(now_iso())
        .execute(&state.db.writer)
        .await?;
        stored += 1;
    }

    if stored == 0 {
        sqlx::query(
            "UPDATE benchmark_runs SET status = 'failed', error = ?, finished_at = ? WHERE id = ?",
        )
        .bind(last_error.unwrap_or_else(|| "no provider could be benchmarked".to_string()))
        .bind(now_iso())
        .bind(benchmark_run_id)
        .execute(&state.db.writer)
        .await?;
        return Err(BenchmarkError::NoProvider(
            "no provider could be benchmarked".to_string(),
        ));
    }

    sqlx::query(
        "UPDATE benchmark_runs SET status = 'succeeded', finished_at = ?, error = ? WHERE id = ?",
    )
    .bind(now_iso())
    .bind(last_error)
    .bind(benchmark_run_id)
    .execute(&state.db.writer)
    .await?;
    Ok(stored)
}

#[derive(Debug, Clone, FromRow, serde::Serialize)]
pub struct BenchmarkRunRow {
    pub id: String,
    pub dataset: String,
    pub status: String,
    pub providers: String,
    pub item_count: i64,
    pub error: Option<String>,
    pub created_at: String,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct BenchmarkResultRow {
    pub provider: String,
    pub model: String,
    pub model_version: Option<String>,
    pub labels: String,
    pub predictions: String,
    pub metrics: String,
    pub latency_ms_avg: Option<f64>,
    pub cost_per_1000_usd: Option<f64>,
    pub unknown_costs: i64,
    pub created_at: String,
}

pub async fn list_runs(
    db: &Db,
    tenant_id: &str,
    limit: i64,
    offset: i64,
) -> Result<(Vec<BenchmarkRunRow>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, BenchmarkRunRow>(
        "SELECT id, dataset, status, providers, item_count, error, created_at, finished_at \
         FROM benchmark_runs WHERE tenant_id = ? ORDER BY created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(tenant_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&db.reader)
    .await?;
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM benchmark_runs WHERE tenant_id = ?")
        .bind(tenant_id)
        .fetch_one(&db.reader)
        .await?;
    Ok((rows, total))
}

pub async fn get_run(
    db: &Db,
    tenant_id: &str,
    id: &str,
) -> Result<Option<(BenchmarkRunRow, Vec<BenchmarkResultRow>)>, sqlx::Error> {
    let run = sqlx::query_as::<_, BenchmarkRunRow>(
        "SELECT id, dataset, status, providers, item_count, error, created_at, finished_at \
         FROM benchmark_runs WHERE tenant_id = ? AND id = ?",
    )
    .bind(tenant_id)
    .bind(id)
    .fetch_optional(&db.reader)
    .await?;
    let Some(run) = run else { return Ok(None) };
    let results = sqlx::query_as::<_, BenchmarkResultRow>(
        "SELECT provider, model, model_version, labels, predictions, metrics, latency_ms_avg, \
         cost_per_1000_usd, unknown_costs, created_at FROM benchmark_results \
         WHERE benchmark_run_id = ? ORDER BY provider",
    )
    .bind(id)
    .fetch_all(&db.reader)
    .await?;
    Ok(Some((run, results)))
}

/// Serialize one result for the API, keeping the metrics object intact.
pub fn result_json(row: &BenchmarkResultRow) -> Value {
    json!({
        "provider": row.provider,
        "model": row.model,
        "model_version": row.model_version,
        "metrics": serde_json::from_str::<Value>(&row.metrics).unwrap_or(Value::Null),
        "latency_ms_avg": row.latency_ms_avg,
        "cost_per_1000_usd": row.cost_per_1000_usd,
        "unknown_costs": row.unknown_costs,
        "created_at": row.created_at,
    })
}
