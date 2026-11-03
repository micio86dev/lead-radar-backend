//! Durable jobs, provider usage and costs, audit log and the spend guard.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::enums::JobKind;
use crate::domain::jobs::{self, FailureOutcome};
use crate::domain::time::{now_iso, parse_iso, to_iso};
use crate::error::CostGuardReason;
use crate::modules::{new_id, to_json};

#[derive(Debug, Clone, FromRow)]
pub struct JobRow {
    pub id: String,
    pub tenant_id: Option<String>,
    pub kind: String,
    pub status: String,
    pub idempotency_key: Option<String>,
    pub payload: String,
    pub priority: i64,
    pub attempts: i64,
    pub max_attempts: i64,
    pub run_at: String,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

/// Enqueue a durable job. `idempotency_key` makes repeated scheduling a no-op, which is how the
/// pipeline stays idempotent across retries and re-imports.
pub async fn enqueue(
    db: &Db,
    tenant_id: Option<&str>,
    kind: JobKind,
    payload: &Value,
    idempotency_key: Option<&str>,
    max_attempts: i64,
) -> Result<(String, bool), sqlx::Error> {
    let timestamp = now_iso();
    let id = new_id();
    let inserted = sqlx::query(
        "INSERT INTO jobs (id, tenant_id, kind, status, idempotency_key, payload, priority, \
         attempts, max_attempts, run_at, created_at, updated_at) \
         VALUES (?, ?, ?, 'queued', ?, ?, 0, 0, ?, ?, ?, ?) \
         ON CONFLICT(idempotency_key) DO NOTHING",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(kind.as_str())
    .bind(idempotency_key)
    .bind(to_json(payload))
    .bind(max_attempts.max(1))
    .bind(&timestamp)
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&db.writer)
    .await?;

    if inserted.rows_affected() == 1 {
        return Ok((id, true));
    }
    let existing: Option<String> = match idempotency_key {
        Some(key) => {
            sqlx::query_scalar("SELECT id FROM jobs WHERE idempotency_key = ?")
                .bind(key)
                .fetch_optional(&db.reader)
                .await?
        }
        None => None,
    };
    Ok((existing.unwrap_or(id), false))
}

pub async fn enqueue_at(
    db: &Db,
    tenant_id: Option<&str>,
    kind: JobKind,
    payload: &Value,
    idempotency_key: Option<&str>,
    max_attempts: i64,
    run_at: DateTime<Utc>,
) -> Result<(String, bool), sqlx::Error> {
    let timestamp = now_iso();
    let id = new_id();
    let inserted = sqlx::query(
        "INSERT INTO jobs (id, tenant_id, kind, status, idempotency_key, payload, priority, \
         attempts, max_attempts, run_at, created_at, updated_at) \
         VALUES (?, ?, ?, 'queued', ?, ?, 0, 0, ?, ?, ?, ?) \
         ON CONFLICT(idempotency_key) DO NOTHING",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(kind.as_str())
    .bind(idempotency_key)
    .bind(to_json(payload))
    .bind(max_attempts.max(1))
    .bind(to_iso(run_at))
    .bind(&timestamp)
    .bind(&timestamp)
    .execute(&db.writer)
    .await?;
    if inserted.rows_affected() == 1 {
        return Ok((id, true));
    }
    let existing: Option<String> = match idempotency_key {
        Some(key) => {
            sqlx::query_scalar("SELECT id FROM jobs WHERE idempotency_key = ?")
                .bind(key)
                .fetch_optional(&db.reader)
                .await?
        }
        None => None,
    };
    Ok((existing.unwrap_or(id), false))
}

/// Claim one due job (or a job whose lease expired) atomically. Only the single writer connection
/// runs this statement, so two workers can never claim the same row.
pub async fn claim_next(
    db: &Db,
    lease_owner: &str,
    lease_seconds: i64,
    allow: Option<&[JobKind]>,
) -> Result<Option<JobRow>, sqlx::Error> {
    let now = now_iso();
    let mut sql = String::from(
        "UPDATE jobs SET status = 'running', attempts = attempts + 1, lease_owner = ?, \
         lease_expires_at = ?, updated_at = ? \
         WHERE id = (SELECT id FROM jobs \
         WHERE ((status = 'queued' AND run_at <= ?) OR \
                (status = 'running' AND lease_expires_at IS NOT NULL AND lease_expires_at < ?))",
    );
    if let Some(kinds) = allow {
        let placeholders = std::iter::repeat_n("?", kinds.len())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(" AND kind IN ({placeholders})"));
    }
    sql.push_str(" ORDER BY priority DESC, run_at ASC LIMIT 1) RETURNING *");

    let mut query = sqlx::query_as::<_, JobRow>(&sql)
        .bind(lease_owner)
        .bind(to_iso(
            Utc::now() + chrono::Duration::seconds(lease_seconds.max(1)),
        ))
        .bind(&now)
        .bind(&now)
        .bind(&now);
    if let Some(kinds) = allow {
        for kind in kinds {
            query = query.bind(kind.as_str());
        }
    }
    query.fetch_optional(&db.writer).await
}

pub async fn record_attempt_start(
    db: &Db,
    job: &JobRow,
    lease_owner: &str,
) -> Result<String, sqlx::Error> {
    let id = new_id();
    sqlx::query(
        "INSERT INTO job_attempts (id, job_id, attempt, status, started_at) \
         VALUES (?, ?, ?, 'failed', ?)",
    )
    .bind(&id)
    .bind(&job.id)
    .bind(job.attempts.max(1))
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    let _ = lease_owner;
    Ok(id)
}

pub async fn record_attempt_end(
    db: &Db,
    attempt_id: &str,
    status: &str,
    error: Option<&str>,
    duration_ms: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE job_attempts SET status = ?, error = ?, finished_at = ?, duration_ms = ? \
         WHERE id = ?",
    )
    .bind(status)
    .bind(error)
    .bind(now_iso())
    .bind(duration_ms)
    .bind(attempt_id)
    .execute(&db.writer)
    .await?;
    Ok(())
}

pub async fn mark_succeeded(db: &Db, job_id: &str) -> Result<(), sqlx::Error> {
    let now = now_iso();
    sqlx::query(
        "UPDATE jobs SET status = 'succeeded', finished_at = ?, updated_at = ?, \
         lease_owner = NULL, lease_expires_at = NULL, last_error = NULL WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(job_id)
    .execute(&db.writer)
    .await?;
    Ok(())
}

pub async fn mark_failed_or_retry(
    db: &Db,
    job: &JobRow,
    error: &str,
    base_backoff_seconds: i64,
) -> Result<FailureOutcome, sqlx::Error> {
    let outcome = jobs::on_failure(job.attempts, job.max_attempts, base_backoff_seconds);
    let now = now_iso();
    match outcome {
        FailureOutcome::Retry { delay_seconds, .. } => {
            let run_at = to_iso(Utc::now() + chrono::Duration::seconds(delay_seconds));
            sqlx::query(
                "UPDATE jobs SET status = 'queued', run_at = ?, updated_at = ?, last_error = ?, \
                 lease_owner = NULL, lease_expires_at = NULL WHERE id = ?",
            )
            .bind(run_at)
            .bind(&now)
            .bind(error)
            .bind(&job.id)
            .execute(&db.writer)
            .await?;
        }
        FailureOutcome::DeadLetter { .. } => {
            sqlx::query(
                "UPDATE jobs SET status = 'dead_letter', updated_at = ?, last_error = ?, \
                 finished_at = ?, lease_owner = NULL, lease_expires_at = NULL WHERE id = ?",
            )
            .bind(&now)
            .bind(error)
            .bind(&now)
            .bind(&job.id)
            .execute(&db.writer)
            .await?;
        }
    }
    Ok(outcome)
}

/// Terminal failure that must not be retried (invalid input, unconfigured provider, rejected key).
pub async fn mark_dead_letter(db: &Db, job_id: &str, error: &str) -> Result<(), sqlx::Error> {
    let now = now_iso();
    sqlx::query(
        "UPDATE jobs SET status = 'dead_letter', updated_at = ?, finished_at = ?, last_error = ?, \
         lease_owner = NULL, lease_expires_at = NULL WHERE id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(error)
    .bind(job_id)
    .execute(&db.writer)
    .await?;
    Ok(())
}

pub async fn retry_job(
    db: &Db,
    tenant_id: &str,
    job_id: &str,
) -> Result<Option<JobRow>, sqlx::Error> {
    let now = now_iso();
    let updated = sqlx::query(
        "UPDATE jobs SET status = 'queued', run_at = ?, attempts = 0, updated_at = ?, \
         last_error = NULL, finished_at = NULL, lease_owner = NULL, lease_expires_at = NULL \
         WHERE id = ? AND tenant_id = ? AND status IN ('failed', 'dead_letter')",
    )
    .bind(&now)
    .bind(&now)
    .bind(job_id)
    .bind(tenant_id)
    .execute(&db.writer)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(None);
    }
    get_job(db, tenant_id, job_id).await
}

pub async fn get_job(
    db: &Db,
    tenant_id: &str,
    job_id: &str,
) -> Result<Option<JobRow>, sqlx::Error> {
    sqlx::query_as::<_, JobRow>("SELECT * FROM jobs WHERE id = ? AND tenant_id = ?")
        .bind(job_id)
        .bind(tenant_id)
        .fetch_optional(&db.reader)
        .await
}

pub async fn list_jobs(
    db: &Db,
    tenant_id: &str,
    status: Option<&str>,
    kind: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<JobRow>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, JobRow>(
        "SELECT * FROM jobs WHERE tenant_id = ? \
         AND (? IS NULL OR status = ?) AND (? IS NULL OR kind = ?) \
         ORDER BY created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(tenant_id)
    .bind(status)
    .bind(status)
    .bind(kind)
    .bind(kind)
    .bind(limit)
    .bind(offset)
    .fetch_all(&db.reader)
    .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE tenant_id = ? \
         AND (? IS NULL OR status = ?) AND (? IS NULL OR kind = ?)",
    )
    .bind(tenant_id)
    .bind(status)
    .bind(status)
    .bind(kind)
    .bind(kind)
    .fetch_one(&db.reader)
    .await?;
    Ok((rows, total))
}

pub async fn job_failure_counts(db: &Db, tenant_id: &str) -> Result<(i64, i64), sqlx::Error> {
    let failed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE tenant_id = ? AND status = 'failed'")
            .bind(tenant_id)
            .fetch_one(&db.reader)
            .await?;
    let dead: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE tenant_id = ? AND status = 'dead_letter'",
    )
    .bind(tenant_id)
    .fetch_one(&db.reader)
    .await?;
    Ok((failed, dead))
}

// ---------------------------------------------------------------------------------------------
// Provider usage and costs
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct UsageRecord<'a> {
    pub provider: &'a str,
    pub operation: &'a str,
    pub request_id: Option<&'a str>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub latency_ms: Option<i64>,
    pub status: &'a str,
    pub error: Option<&'a str>,
}

pub async fn record_usage(
    db: &Db,
    tenant_id: Option<&str>,
    usage: &UsageRecord<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_usage (id, tenant_id, provider, operation, request_id, \
         input_tokens, output_tokens, latency_ms, status, error, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(usage.provider)
    .bind(usage.operation)
    .bind(usage.request_id)
    .bind(usage.input_tokens)
    .bind(usage.output_tokens)
    .bind(usage.latency_ms)
    .bind(usage.status)
    .bind(usage.error)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostKind {
    Actual,
    Estimate,
}

impl CostKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Actual => "actual",
            Self::Estimate => "estimate",
        }
    }
}

/// Record a provider cost in the provider's own currency. Unknown cost is recorded as `unknown`
/// with a zero amount and never presented as a known charge.
#[allow(clippy::too_many_arguments)]
pub async fn record_cost(
    db: &Db,
    tenant_id: Option<&str>,
    provider: &str,
    kind: CostKind,
    amount: f64,
    currency: &str,
    unit: &str,
    reference: Option<&str>,
    unknown: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO provider_costs (id, tenant_id, provider, kind, amount, currency, unit, \
         reference, unknown, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(provider)
    .bind(kind.as_str())
    .bind(if unknown { 0.0 } else { amount })
    .bind(currency)
    .bind(unit)
    .bind(reference)
    .bind(if unknown { 1 } else { 0 })
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderCostSummary {
    pub provider: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub actual_cost_usd: Option<f64>,
    pub estimated_cost_usd: Option<f64>,
    pub unknown_cost_requests: i64,
    pub currency: String,
}

pub async fn provider_cost_summary(
    db: &Db,
    tenant_id: &str,
    since: DateTime<Utc>,
) -> Result<Vec<ProviderCostSummary>, sqlx::Error> {
    let since = to_iso(since);
    let providers = ["jev", "laya", "qwen", "apify"];
    let mut summaries = Vec::new();
    for provider in providers {
        let (requests, input_tokens, output_tokens): (i64, i64, i64) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(input_tokens), 0), COALESCE(SUM(output_tokens), 0) \
             FROM provider_usage WHERE tenant_id = ? AND provider = ? AND created_at >= ?",
        )
        .bind(tenant_id)
        .bind(provider)
        .bind(&since)
        .fetch_one(&db.reader)
        .await?;

        let actual: Option<f64> = sqlx::query_scalar(
            "SELECT SUM(amount) FROM provider_costs WHERE tenant_id = ? AND provider = ? \
             AND kind = 'actual' AND unknown = 0 AND created_at >= ?",
        )
        .bind(tenant_id)
        .bind(provider)
        .bind(&since)
        .fetch_one(&db.reader)
        .await?;
        let estimated: Option<f64> = sqlx::query_scalar(
            "SELECT SUM(amount) FROM provider_costs WHERE tenant_id = ? AND provider = ? \
             AND kind = 'estimate' AND unknown = 0 AND created_at >= ?",
        )
        .bind(tenant_id)
        .bind(provider)
        .bind(&since)
        .fetch_one(&db.reader)
        .await?;
        let unknown_costs: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM provider_costs WHERE tenant_id = ? AND provider = ? \
             AND unknown = 1 AND created_at >= ?",
        )
        .bind(tenant_id)
        .bind(provider)
        .bind(&since)
        .fetch_one(&db.reader)
        .await?;

        summaries.push(ProviderCostSummary {
            provider: provider.to_string(),
            requests,
            input_tokens,
            output_tokens,
            actual_cost_usd: actual,
            estimated_cost_usd: estimated,
            unknown_cost_requests: unknown_costs,
            currency: "USD".to_string(),
        });
    }
    Ok(summaries)
}

// ---------------------------------------------------------------------------------------------
// Audit log
// ---------------------------------------------------------------------------------------------

pub async fn audit(
    db: &Db,
    tenant_id: &str,
    actor: &str,
    action: &str,
    entity: &str,
    entity_id: Option<&str>,
    details: &Value,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO audit_logs (id, tenant_id, actor, action, entity, entity_id, details, \
         created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(actor)
    .bind(action)
    .bind(entity)
    .bind(entity_id)
    .bind(to_json(details))
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Spend guard (pure decision, called before a billable run is queued)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CostGuardInput {
    pub provider_configured: bool,
    pub kill_switch: bool,
    pub actor_enabled: bool,
    pub actor_validated: bool,
    pub items_limit: i64,
    pub max_items_per_run: i64,
    pub running_runs: i64,
    pub max_concurrent_runs: i64,
    pub total_spend: f64,
    pub daily_spend: f64,
    pub run_cost_estimate: Option<f64>,
    pub max_run_cost: f64,
    pub daily_budget: f64,
    pub total_budget: f64,
}

pub fn cost_guard(input: &CostGuardInput) -> Result<(), CostGuardReason> {
    if !input.provider_configured {
        return Err(CostGuardReason::ProviderUnconfigured);
    }
    if input.kill_switch {
        return Err(CostGuardReason::KillSwitch);
    }
    if !input.actor_enabled {
        return Err(CostGuardReason::ActorNotEnabled);
    }
    if !input.actor_validated {
        return Err(CostGuardReason::ActorNotValidated);
    }
    if input.items_limit < 1 || input.items_limit > input.max_items_per_run {
        return Err(CostGuardReason::ItemsLimitInvalid);
    }
    if let Some(estimate) = input.run_cost_estimate {
        if estimate > input.max_run_cost {
            return Err(CostGuardReason::RunCostAboveLimit);
        }
    }
    if input.total_spend >= input.total_budget {
        return Err(CostGuardReason::BudgetExhausted);
    }
    if input.daily_spend >= input.daily_budget {
        return Err(CostGuardReason::DailyCeilingReached);
    }
    if input.running_runs >= input.max_concurrent_runs {
        return Err(CostGuardReason::ConcurrencyLimit);
    }
    Ok(())
}

/// Spacing between budget checks, so the guard reads a consistent window.
pub fn since_midnight_utc(now: DateTime<Utc>) -> DateTime<Utc> {
    now.date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|naive| naive.and_utc())
        .unwrap_or(now)
}

pub fn days_ago(now: DateTime<Utc>, days: i64) -> DateTime<Utc> {
    now - chrono::Duration::days(days.max(1))
}

/// Timestamps are stored as RFC 3339; expose the parser for handlers that need to filter.
pub fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    parse_iso(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> CostGuardInput {
        CostGuardInput {
            provider_configured: true,
            kill_switch: false,
            actor_enabled: true,
            actor_validated: true,
            items_limit: 25,
            max_items_per_run: 50,
            running_runs: 0,
            max_concurrent_runs: 2,
            total_spend: 0.5,
            daily_spend: 0.1,
            run_cost_estimate: Some(0.2),
            max_run_cost: 0.5,
            daily_budget: 2.0,
            total_budget: 25.0,
        }
    }

    #[test]
    fn conservative_defaults_allow_a_small_run() {
        assert_eq!(cost_guard(&guard()), Ok(()));
    }

    #[test]
    fn every_guard_reason_is_reachable_and_distinct() {
        assert_eq!(
            cost_guard(&CostGuardInput {
                provider_configured: false,
                ..guard()
            }),
            Err(CostGuardReason::ProviderUnconfigured)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                kill_switch: true,
                ..guard()
            }),
            Err(CostGuardReason::KillSwitch)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                actor_enabled: false,
                ..guard()
            }),
            Err(CostGuardReason::ActorNotEnabled)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                actor_validated: false,
                ..guard()
            }),
            Err(CostGuardReason::ActorNotValidated)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                items_limit: 0,
                ..guard()
            }),
            Err(CostGuardReason::ItemsLimitInvalid)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                items_limit: 51,
                ..guard()
            }),
            Err(CostGuardReason::ItemsLimitInvalid)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                run_cost_estimate: Some(0.9),
                ..guard()
            }),
            Err(CostGuardReason::RunCostAboveLimit)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                total_spend: 25.0,
                ..guard()
            }),
            Err(CostGuardReason::BudgetExhausted)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                daily_spend: 2.0,
                ..guard()
            }),
            Err(CostGuardReason::DailyCeilingReached)
        );
        assert_eq!(
            cost_guard(&CostGuardInput {
                running_runs: 2,
                ..guard()
            }),
            Err(CostGuardReason::ConcurrencyLimit)
        );
    }
}
