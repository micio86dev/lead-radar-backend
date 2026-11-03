//! Durable worker loop.
//!
//! One writer connection, bounded concurrency, separate slots for provider calls and for I/O, and a
//! lease per job so a crashed process's jobs are recoverable. A job never holds a database
//! transaction while it talks to a provider.

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

use crate::domain::enums::JobKind;
use crate::domain::filtering::evaluate;
use crate::domain::jobs::FailureOutcome;
use crate::domain::time::{now_iso, to_iso};
use crate::modules::operations::JobRow;
use crate::modules::{benchmark, decision, generative, ingestion, operations, sources};
use crate::providers::{RunStatus, SourceRunRequest};
use crate::state::{AppState, SharedState};

const AI_KINDS: &[JobKind] = &[
    JobKind::DecisionEvaluation,
    JobKind::GenerativeAnalysis,
    JobKind::CommentGeneration,
    JobKind::BenchmarkExecution,
];

const IO_KINDS: &[JobKind] = &[
    JobKind::Discovery,
    JobKind::ActorRun,
    JobKind::DatasetIngestion,
    JobKind::Normalization,
    JobKind::Deduplication,
];

#[derive(Debug)]
pub enum JobOutcome {
    Done,
    Requeue { delay_seconds: i64, payload: Value },
}

#[derive(Debug)]
pub struct JobError {
    pub message: String,
    pub retryable: bool,
}

impl JobError {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
        }
    }

    fn transient(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
        }
    }
}

impl From<sqlx::Error> for JobError {
    fn from(error: sqlx::Error) -> Self {
        Self::transient(format!("database error: {error}"))
    }
}

fn parse_payload<T: serde::de::DeserializeOwned>(job: &JobRow) -> Result<T, JobError> {
    serde_json::from_str(&job.payload)
        .map_err(|error| JobError::permanent(format!("invalid job payload: {error}")))
}

fn tenant_of(job: &JobRow) -> Result<&str, JobError> {
    job.tenant_id
        .as_deref()
        .ok_or_else(|| JobError::permanent("job has no tenant"))
}

/// Start the worker. Returns on graceful shutdown.
pub async fn run(state: SharedState) -> anyhow::Result<()> {
    let worker_id = format!("worker-{}", uuid::Uuid::now_v7());
    let io_slots = Arc::new(Semaphore::new(
        state.config.worker.concurrency.max(1) as usize
    ));
    let ai_slots = Arc::new(Semaphore::new(
        state.config.worker.ai_concurrency.max(1) as usize
    ));
    let poll = Duration::from_secs(state.config.worker.poll_interval_seconds.max(1) as u64);

    tracing::info!(worker = %worker_id, "worker started");
    let mut tasks: JoinSet<()> = JoinSet::new();

    loop {
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                if signal.is_ok() {
                    tracing::info!(worker = %worker_id, "graceful shutdown requested");
                }
                break;
            }
            _ = tokio::time::sleep(poll) => {}
        }

        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                tracing::error!(error = %error, "job task panicked");
            }
        }

        // Claim while slots are available.
        loop {
            let io_permit = io_slots.clone().try_acquire_owned().ok();
            let ai_permit = ai_slots.clone().try_acquire_owned().ok();
            let allow: Option<Vec<JobKind>> = match (&io_permit, &ai_permit) {
                (Some(_), Some(_)) => None,
                (Some(_), None) => Some(IO_KINDS.to_vec()),
                (None, Some(_)) => Some(AI_KINDS.to_vec()),
                (None, None) => break,
            };
            let claimed = operations::claim_next(
                &state.db,
                &worker_id,
                state.config.worker.lease_seconds,
                allow.as_deref(),
            )
            .await;
            let job = match claimed {
                Ok(Some(job)) => job,
                Ok(None) => break,
                Err(error) => {
                    tracing::error!(error = %error, "claiming a job failed");
                    break;
                }
            };
            let kind = JobKind::parse(&job.kind);
            let is_ai = kind.map(|kind| AI_KINDS.contains(&kind)).unwrap_or(false);
            let permit: Option<OwnedSemaphorePermit> = if is_ai { ai_permit } else { io_permit };
            let state = state.clone();
            let worker_id = worker_id.clone();
            tasks.spawn(async move {
                let _permit = permit;
                execute_job(&state, &job, &worker_id).await;
            });
        }
    }

    let deadline = Instant::now() + Duration::from_secs(30);
    while !tasks.is_empty() && Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(1), tasks.join_next()).await {
            Ok(Some(Err(error))) => tracing::error!(error = %error, "job task panicked"),
            Ok(Some(Ok(()))) | Err(_) => {}
            Ok(None) => break,
        }
    }
    tracing::info!(worker = %worker_id, "worker stopped");
    state.db.close().await;
    Ok(())
}

async fn execute_job(state: &AppState, job: &JobRow, worker_id: &str) {
    let attempt_id = match operations::record_attempt_start(&state.db, job, worker_id).await {
        Ok(id) => Some(id),
        Err(error) => {
            tracing::error!(error = %error, job = %job.id, "recording the attempt start failed");
            None
        }
    };
    let started = Instant::now();

    match handle_job(state, job).await {
        Ok(outcome) => {
            let mut requeue_error = None;
            if let JobOutcome::Requeue {
                delay_seconds,
                payload,
            } = outcome
            {
                let run_at = chrono::Utc::now() + chrono::Duration::seconds(delay_seconds.max(1));
                if let Err(error) = operations::enqueue_at(
                    &state.db,
                    job.tenant_id.as_deref(),
                    JobKind::parse(&job.kind).unwrap_or(JobKind::Discovery),
                    &payload,
                    None,
                    job.max_attempts,
                    run_at,
                )
                .await
                {
                    requeue_error = Some(format!("requeue failed: {error}"));
                }
            }
            if let Some(error) = requeue_error {
                fail_job(
                    state,
                    job,
                    attempt_id.as_deref(),
                    JobError::transient(error),
                    started,
                )
                .await;
                return;
            }
            if let Err(error) = operations::mark_succeeded(&state.db, &job.id).await {
                tracing::error!(error = %error, job = %job.id, "marking the job succeeded failed");
            }
            finish_attempt(state, attempt_id.as_deref(), "succeeded", None, started).await;
        }
        Err(error) => {
            let retryable = error.retryable;
            fail_job(state, job, attempt_id.as_deref(), error, started).await;
            tracing::warn!(
                job = %job.id,
                kind = %job.kind,
                retryable,
                "job failed"
            );
        }
    }
}

async fn fail_job(
    state: &AppState,
    job: &JobRow,
    attempt_id: Option<&str>,
    error: JobError,
    started: Instant,
) {
    let outcome = if error.retryable {
        operations::mark_failed_or_retry(
            &state.db,
            job,
            &error.message,
            state.config.worker.retry_backoff_seconds,
        )
        .await
    } else {
        operations::mark_dead_letter(&state.db, &job.id, &error.message)
            .await
            .map(|_| FailureOutcome::DeadLetter {
                attempt: job.attempts,
            })
    };
    let status = match outcome {
        Ok(FailureOutcome::Retry { .. }) => "failed",
        _ => "dead_letter",
    };
    finish_attempt(state, attempt_id, status, Some(&error.message), started).await;
}

async fn finish_attempt(
    state: &AppState,
    attempt_id: Option<&str>,
    status: &str,
    error: Option<&str>,
    started: Instant,
) {
    if let Some(attempt_id) = attempt_id {
        let duration = started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        if let Err(write_error) =
            operations::record_attempt_end(&state.db, attempt_id, status, error, duration).await
        {
            tracing::error!(error = %write_error, "recording the attempt outcome failed");
        }
    }
}

async fn handle_job(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let kind = JobKind::parse(&job.kind)
        .ok_or_else(|| JobError::permanent(format!("unknown job kind {}", job.kind)))?;
    match kind {
        JobKind::Normalization => normalize_run(state, job).await,
        JobKind::Deduplication => verify_deduplication(state, job).await,
        JobKind::DecisionEvaluation => evaluate_decision(state, job).await,
        JobKind::GenerativeAnalysis => analyze(state, job).await,
        JobKind::CommentGeneration => generate_comments(state, job).await,
        JobKind::Discovery => discover(state, job).await,
        JobKind::ActorRun => poll_actor_run(state, job).await,
        JobKind::DatasetIngestion => ingest_dataset(state, job).await,
        JobKind::BenchmarkExecution => run_benchmark(state, job).await,
    }
}

async fn normalize_run(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    #[derive(serde::Deserialize)]
    struct Payload {
        ingestion_run_id: String,
    }
    let payload: Payload = parse_payload(job)?;
    let post_ids = ingestion::run_post_ids(&state.db, tenant_id, &payload.ingestion_run_id).await?;
    let keywords = sources::keyword_set(&state.db, tenant_id).await;
    let mut queued = 0usize;
    let mut filtered = 0usize;

    for post_id in post_ids {
        let Some(post) = ingestion::get_post(&state.db, tenant_id, &post_id).await? else {
            continue;
        };
        let outcome = evaluate(&post.content, &keywords);
        if outcome.relevant {
            let hash = ingestion::content_hash(post.title.as_deref(), &post.content);
            operations::enqueue(
                &state.db,
                Some(tenant_id),
                JobKind::DecisionEvaluation,
                &json!({"post_id": post_id, "trigger": "auto"}),
                Some(&format!("decision:auto:{post_id}:{hash}")),
                state.config.worker.max_attempts,
            )
            .await?;
            queued += 1;
        } else {
            filtered += 1;
            operations::audit(
                &state.db,
                tenant_id,
                "system:worker",
                "post_filtered_out",
                "discovered_post",
                Some(&post_id),
                &json!({
                    "matched_negative": outcome.matched_negative,
                    "matched_positive": outcome.matched_positive,
                }),
            )
            .await?;
        }
    }
    tracing::info!(
        run = %payload.ingestion_run_id,
        queued,
        filtered,
        "normalization finished"
    );
    Ok(JobOutcome::Done)
}

async fn verify_deduplication(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    let duplicates: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM discovered_posts WHERE tenant_id = ? AND deleted_at IS NULL \
         GROUP BY content_hash HAVING COUNT(*) > 1",
    )
    .bind(tenant_id)
    .fetch_optional(&state.db.reader)
    .await?
    .unwrap_or(0);
    operations::audit(
        &state.db,
        tenant_id,
        "system:worker",
        "deduplication_verified",
        "tenant",
        Some(tenant_id),
        &json!({"duplicate_hashes": duplicates}),
    )
    .await?;
    Ok(JobOutcome::Done)
}

async fn evaluate_decision(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    #[derive(serde::Deserialize)]
    struct Payload {
        post_id: String,
        #[serde(default)]
        trigger: Option<String>,
    }
    let payload: Payload = parse_payload(job)?;
    let trigger = match payload.trigger.as_deref() {
        Some("manual") => decision::Trigger::Manual,
        _ => decision::Trigger::Auto,
    };
    match decision::evaluate_post(state, tenant_id, &payload.post_id, trigger).await {
        Ok(outcome) => {
            tracing::info!(
                post = %payload.post_id,
                score = outcome.score,
                status = %outcome.qualification_status,
                "decision evaluated"
            );
            Ok(JobOutcome::Done)
        }
        Err(error) => {
            let retryable = error.is_retryable();
            let message = error.to_string();
            if retryable {
                Err(JobError::transient(message))
            } else {
                Err(JobError::permanent(message))
            }
        }
    }
}

async fn analyze(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    #[derive(serde::Deserialize)]
    struct Payload {
        lead_id: String,
    }
    let payload: Payload = parse_payload(job)?;
    match generative::analyze_lead(state, tenant_id, &payload.lead_id).await {
        Ok(_) => Ok(JobOutcome::Done),
        Err(error) => {
            let message = error.to_string();
            if error.is_retryable() {
                Err(JobError::transient(message))
            } else {
                Err(JobError::permanent(message))
            }
        }
    }
}

async fn generate_comments(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    #[derive(serde::Deserialize)]
    struct Payload {
        lead_id: String,
        #[serde(default)]
        actor: Option<String>,
    }
    let payload: Payload = parse_payload(job)?;
    let actor = payload.actor.unwrap_or_else(|| "system:worker".to_string());
    match generative::generate_comments(state, tenant_id, &payload.lead_id, &actor).await {
        Ok(outcome) => {
            tracing::info!(
                lead = %payload.lead_id,
                stored = outcome.stored,
                skipped = outcome.skipped,
                "comments drafted"
            );
            Ok(JobOutcome::Done)
        }
        Err(error) => {
            let message = error.to_string();
            if error.is_retryable() {
                Err(JobError::transient(message))
            } else {
                Err(JobError::permanent(message))
            }
        }
    }
}

async fn discover(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    let actors = sources::list_actors(&state.db, tenant_id).await?;
    let mut queued = 0usize;
    for actor in actors {
        if !actor.enabled || actor.validation_status != "validated" {
            continue;
        }
        let items_limit = actor
            .max_items_per_run
            .min(state.config.apify.max_items_per_run);
        if let Err(reason) =
            sources::guard_apify_run(&state.db, tenant_id, &state.config, &actor, items_limit).await
        {
            operations::audit(
                &state.db,
                tenant_id,
                "system:worker",
                "apify_run_refused",
                "actor_registry",
                Some(&actor.id),
                &json!({"reason": reason.as_str()}),
            )
            .await?;
            continue;
        }
        let input = actor.input_template.clone();
        let (actor_job_id, _) = operations::enqueue(
            &state.db,
            Some(tenant_id),
            JobKind::ActorRun,
            &json!({"actor_registry_id": actor.id, "items_limit": items_limit, "input": input}),
            None,
            state.config.worker.max_attempts,
        )
        .await?;
        let run_id = sources::insert_apify_run(
            &state.db,
            tenant_id,
            &actor.id,
            &actor_job_id,
            &json!({"input": input, "items_limit": items_limit}),
        )
        .await?;
        sqlx::query("UPDATE jobs SET payload = ? WHERE id = ?")
            .bind(json!({"apify_run_id": run_id}).to_string())
            .bind(&actor_job_id)
            .execute(&state.db.writer)
            .await?;
        queued += 1;
    }
    tracing::info!(queued, "discovery finished");
    Ok(JobOutcome::Done)
}

async fn poll_actor_run(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    #[derive(serde::Deserialize)]
    struct Payload {
        apify_run_id: String,
    }
    let payload: Payload = parse_payload(job)?;
    let run = sources::get_apify_run(&state.db, tenant_id, &payload.apify_run_id)
        .await?
        .ok_or_else(|| JobError::permanent("apify run not found"))?;
    let actor = sources::get_actor(&state.db, tenant_id, &run.actor_registry_id)
        .await?
        .ok_or_else(|| JobError::permanent("actor not found"))?;

    if run.status == "queued" {
        let input_raw = sources::apify_run_input(&state.db, tenant_id, &run.id)
            .await?
            .unwrap_or_else(|| "{}".to_string());
        let input: Value = serde_json::from_str(&input_raw).unwrap_or_else(|_| json!({}));
        let items_limit = input
            .get("items_limit")
            .and_then(Value::as_i64)
            .unwrap_or(actor.max_items_per_run);
        let request = SourceRunRequest {
            actor_id: actor.actor_id.clone(),
            input: input.get("input").cloned().unwrap_or_else(|| json!({})),
            max_items: items_limit,
        };
        match state.providers.source.start_run(&request).await {
            Ok(provider_run) => {
                sources::update_apify_run(
                    &state.db,
                    &run.id,
                    provider_run.status.as_str(),
                    Some(&provider_run.provider_run_id),
                    provider_run.dataset_id.as_deref(),
                    Some(provider_run.items_count),
                    provider_run.cost_usd,
                    provider_run.error.as_deref(),
                )
                .await?;
                return Ok(JobOutcome::Requeue {
                    delay_seconds: state.config.apify.poll_interval_seconds,
                    payload: json!({"apify_run_id": run.id}),
                });
            }
            Err(error) => {
                sources::update_apify_run(
                    &state.db,
                    &run.id,
                    "failed",
                    None,
                    None,
                    None,
                    None,
                    Some(&error.to_string()),
                )
                .await?;
                return if error.is_retryable() {
                    Err(JobError::transient(error.to_string()))
                } else {
                    Err(JobError::permanent(error.to_string()))
                };
            }
        }
    }

    let provider_run_id = run.id.clone();
    let provider_run_id = sqlx::query_scalar::<_, String>(
        "SELECT provider_run_id FROM apify_runs WHERE id = ? AND provider_run_id IS NOT NULL",
    )
    .bind(&provider_run_id)
    .fetch_optional(&state.db.reader)
    .await?
    .ok_or_else(|| JobError::permanent("apify run has no provider run id"))?;

    let provider_run = state
        .providers
        .source
        .get_run(&provider_run_id)
        .await
        .map_err(|error| {
            if error.is_retryable() {
                JobError::transient(error.to_string())
            } else {
                JobError::permanent(error.to_string())
            }
        })?;

    sources::update_apify_run(
        &state.db,
        &run.id,
        provider_run.status.as_str(),
        None,
        provider_run.dataset_id.as_deref(),
        Some(provider_run.items_count),
        provider_run.cost_usd,
        provider_run.error.as_deref(),
    )
    .await?;

    match provider_run.status {
        RunStatus::Succeeded => {
            if let Some(cost) = provider_run.cost_usd {
                operations::record_cost(
                    &state.db,
                    Some(tenant_id),
                    "apify",
                    operations::CostKind::Actual,
                    cost,
                    "USD",
                    "actor_run",
                    Some(&provider_run_id),
                    false,
                )
                .await?;
            }
            if let Some(dataset_id) = provider_run.dataset_id {
                operations::enqueue(
                    &state.db,
                    Some(tenant_id),
                    JobKind::DatasetIngestion,
                    &json!({"apify_run_id": run.id, "dataset_id": dataset_id, "offset": 0}),
                    None,
                    state.config.worker.max_attempts,
                )
                .await?;
            }
            sources::mark_actor_success(&state.db, &actor.id).await?;
            Ok(JobOutcome::Done)
        }
        status if status.terminal() => {
            let message = provider_run
                .error
                .clone()
                .unwrap_or_else(|| format!("run finished with status {}", status.as_str()));
            Err(JobError::permanent(message))
        }
        _ => Ok(JobOutcome::Requeue {
            delay_seconds: state.config.apify.poll_interval_seconds,
            payload: json!({"apify_run_id": run.id}),
        }),
    }
}

async fn ingest_dataset(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    #[derive(serde::Deserialize)]
    struct Payload {
        apify_run_id: String,
        dataset_id: String,
        #[serde(default)]
        offset: i64,
    }
    let payload: Payload = parse_payload(job)?;
    const PAGE_SIZE: i64 = 100;
    let items = state
        .providers
        .source
        .fetch_items(&payload.dataset_id, payload.offset, PAGE_SIZE)
        .await
        .map_err(|error| {
            if error.is_retryable() {
                JobError::transient(error.to_string())
            } else {
                JobError::permanent(error.to_string())
            }
        })?;
    let posts: Vec<ingestion::ImportPost> = items
        .iter()
        .filter_map(ingestion::map_dataset_item)
        .collect();
    let outcome = ingestion::import_posts(
        &state.db,
        tenant_id,
        None,
        &posts,
        "system:worker",
        state.config.worker.max_attempts,
        "apify_actor",
    )
    .await?;

    let total: i64 = sqlx::query_scalar("SELECT items_count FROM apify_runs WHERE id = ?")
        .bind(&payload.apify_run_id)
        .fetch_one(&state.db.reader)
        .await?;
    sources::update_apify_run(
        &state.db,
        &payload.apify_run_id,
        "succeeded",
        None,
        Some(&payload.dataset_id),
        Some(total + outcome.imported),
        None,
        None,
    )
    .await?;

    if items.len() as i64 == PAGE_SIZE {
        return Ok(JobOutcome::Requeue {
            delay_seconds: 1,
            payload: json!({
                "apify_run_id": payload.apify_run_id,
                "dataset_id": payload.dataset_id,
                "offset": payload.offset + PAGE_SIZE,
            }),
        });
    }
    Ok(JobOutcome::Done)
}

async fn run_benchmark(state: &AppState, job: &JobRow) -> Result<JobOutcome, JobError> {
    let tenant_id = tenant_of(job)?;
    #[derive(serde::Deserialize)]
    struct Payload {
        benchmark_run_id: String,
    }
    let payload: Payload = parse_payload(job)?;
    match benchmark::execute(state, tenant_id, &payload.benchmark_run_id).await {
        Ok(results) => {
            tracing::info!(results, "benchmark finished");
            Ok(JobOutcome::Done)
        }
        Err(benchmark::BenchmarkError::Database(error)) => Err(error.into()),
        Err(error) => Err(JobError::permanent(error.to_string())),
    }
}

/// Process at most one claimable job synchronously. Used by tests and by `make` automation; the
/// production loop uses the same `execute_job` path.
pub async fn run_once(state: &AppState, worker_id: &str) -> anyhow::Result<bool> {
    match operations::claim_next(
        &state.db,
        worker_id,
        state.config.worker.lease_seconds,
        None,
    )
    .await?
    {
        Some(job) => {
            execute_job(state, &job, worker_id).await;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Drain due jobs, up to `max` iterations. Jobs scheduled in the future stay queued.
pub async fn drain(state: &AppState, max: usize) -> anyhow::Result<usize> {
    let mut processed = 0;
    for _ in 0..max {
        if !run_once(state, "test-worker").await? {
            break;
        }
        processed += 1;
    }
    Ok(processed)
}

/// Exposed for the API: enqueue a discovery pass for every validated Actor.
pub async fn enqueue_discovery(
    state: &AppState,
    tenant_id: &str,
) -> Result<(String, bool), sqlx::Error> {
    operations::enqueue(
        &state.db,
        Some(tenant_id),
        JobKind::Discovery,
        &json!({"requested_at": now_iso()}),
        None,
        state.config.worker.max_attempts,
    )
    .await
}

/// Kept explicit for the API's job listing.
pub fn _timestamp() -> String {
    to_iso(chrono::Utc::now())
}
