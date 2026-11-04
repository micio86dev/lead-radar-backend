//! HTTP surface. Handlers validate input, call one module function and map the result to JSON; no
//! business rule and no SQL lives here.

mod apify;
mod comments;
mod directory;
mod imports;
mod operations;
mod opportunities;
mod profiles;
mod providers;
mod system;

use std::sync::Arc;

use axum::http::{HeaderValue, Method};
use axum::routing::{get, patch, post};
use axum::Router;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;

use crate::state::{AppState, SharedState};

/// Build the application router. `SharedState` is the only state the handlers receive.
pub fn router(state: SharedState) -> Router {
    let cors = cors_layer(&state);
    let api = Router::new()
        .route("/dashboard", get(system::dashboard))
        .route(
            "/settings",
            get(system::get_settings).patch(system::patch_settings),
        )
        // Opportunities
        .route("/opportunities", get(opportunities::list))
        .route("/reply-console", get(opportunities::reply_console))
        .route(
            "/reply-console/match",
            get(opportunities::reply_console_match),
        )
        .route(
            "/reply-console/prepare",
            post(opportunities::prepare_drafts),
        )
        .route(
            "/opportunities/{id}/reply",
            post(opportunities::record_reply),
        )
        .route(
            "/opportunities/{id}",
            get(opportunities::detail).delete(opportunities::remove),
        )
        .route("/opportunities/{id}/analyze", post(opportunities::analyze))
        .route(
            "/opportunities/{id}/generate-comments",
            post(opportunities::generate_comments),
        )
        // Comments
        .route("/comments", get(comments::list))
        .route(
            "/comments/{id}",
            get(comments::detail).patch(comments::patch),
        )
        .route("/comments/{id}/approve", post(comments::approve))
        .route("/comments/{id}/reject", post(comments::reject))
        .route(
            "/comments/{id}/record-publication",
            post(comments::record_publication),
        )
        // Engagements
        .route(
            "/engagements",
            get(operations::list_engagements).post(operations::create_engagement),
        )
        // Search profiles
        .route(
            "/search-profiles",
            get(profiles::list).post(profiles::create),
        )
        .route(
            "/search-profiles/{id}",
            get(profiles::detail)
                .patch(profiles::patch)
                .delete(profiles::remove),
        )
        // Communities
        .route(
            "/communities",
            get(directory::list_communities).post(directory::create_community),
        )
        .route("/communities/{id}", patch(directory::patch_community))
        .route(
            "/communities/{id}/validate",
            post(directory::validate_community),
        )
        // Sources
        .route(
            "/sources",
            get(directory::list_sources).post(directory::create_source),
        )
        .route("/sources/{id}", patch(directory::patch_source))
        .route("/sources/{id}/validate", post(directory::validate_source))
        .route("/sources/{id}/test", post(directory::test_source))
        // Apify
        .route(
            "/providers/apify/actors",
            get(apify::list_actors).post(apify::create_actor),
        )
        .route(
            "/providers/apify/runs",
            get(apify::list_runs).post(apify::start_run),
        )
        .route("/providers/apify/actors/{id}", patch(apify::patch_actor))
        .route("/providers/apify/runs/{id}", get(apify::get_run))
        .route("/providers/apify/runs/{id}/cancel", post(apify::cancel_run))
        .route(
            "/providers/apify/runs/{id}/reingest",
            post(apify::reingest_run),
        )
        .route("/providers/apify/usage", get(apify::usage))
        // Providers
        .route("/providers", get(providers::list))
        .route("/providers/usage", get(providers::usage))
        .route("/providers/decisions", get(providers::decisions))
        .route(
            "/providers/benchmark",
            get(providers::list_benchmarks).post(providers::start_benchmark),
        )
        .route(
            "/providers/benchmark/dataset",
            post(providers::ingest_benchmark_dataset),
        )
        .route("/providers/benchmark/{id}", get(providers::get_benchmark))
        .route("/providers/{provider}/test", post(providers::test))
        // Import
        .route("/import/json", post(imports::import_json))
        // Jobs
        .route("/jobs", get(operations::list_jobs))
        .route("/jobs/{id}/retry", post(operations::retry_job))
        .with_state(state.clone());

    Router::new()
        .route("/health", get(system::health))
        .route("/ready", get(system::ready))
        .route("/api/v1/openapi.json", get(system::openapi))
        .nest("/api/v1", api)
        .layer(TraceLayer::new_for_http())
        .layer(cors)
        .with_state(state)
}

fn cors_layer(state: &AppState) -> CorsLayer {
    let origins: Vec<HeaderValue> = state
        .config
        .cors_allowed_origins
        .iter()
        .filter_map(|origin| origin.parse::<HeaderValue>().ok())
        .collect();
    if origins.is_empty() {
        CorsLayer::new()
            .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
            .allow_headers(Any)
    } else {
        CorsLayer::new()
            .allow_origin(origins)
            .allow_methods([Method::GET, Method::POST, Method::PATCH, Method::DELETE])
            .allow_headers(Any)
    }
}

/// Serve the API until the process is asked to stop.
pub async fn serve(state: SharedState) -> anyhow::Result<()> {
    let host = state.config.host.clone();
    let port = state.config.port;
    let listener = tokio::net::TcpListener::bind(format!("{host}:{port}"))
        .await
        .map_err(|error| anyhow::anyhow!("binding {host}:{port}: {error}"))?;
    tracing::info!(%host, port, "api listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutdown signal received");
        })
        .await?;
    Ok(())
}

/// Build the shared state for tests and binaries.
pub async fn build_state(config: crate::config::AppConfig) -> anyhow::Result<Arc<AppState>> {
    Ok(Arc::new(AppState::build(config).await?))
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Lead Radar API",
        version = "0.1.0",
        description = "Multi-tenant B2B opportunity discovery. Manual publication only."
    ),
    paths(
        system::health,
        system::ready,
        system::dashboard,
        system::get_settings,
        system::patch_settings,
        opportunities::list,
        opportunities::reply_console,
        opportunities::reply_console_match,
        opportunities::prepare_drafts,
        opportunities::record_reply,
        opportunities::detail,
        opportunities::analyze,
        opportunities::generate_comments,
        opportunities::remove,
        comments::list,
        comments::detail,
        comments::patch,
        comments::approve,
        comments::reject,
        comments::record_publication,
        operations::list_engagements,
        operations::create_engagement,
        operations::list_jobs,
        operations::retry_job,
        profiles::list,
        profiles::create,
        profiles::detail,
        profiles::patch,
        profiles::remove,
        directory::list_communities,
        directory::create_community,
        directory::patch_community,
        directory::validate_community,
        directory::list_sources,
        directory::create_source,
        directory::patch_source,
        directory::validate_source,
        directory::test_source,
        apify::list_actors,
        apify::create_actor,
        apify::patch_actor,
        apify::list_runs,
        apify::start_run,
        apify::get_run,
        apify::cancel_run,
        apify::reingest_run,
        apify::usage,
        providers::list,
        providers::usage,
        providers::decisions,
        providers::list_benchmarks,
        providers::ingest_benchmark_dataset,
        providers::start_benchmark,
        providers::get_benchmark,
        providers::test,
        imports::import_json
    ),
    tags(
        (name = "system", description = "Health, readiness, dashboard and settings"),
        (name = "opportunities", description = "Discovered opportunities and lead qualification"),
        (name = "comments", description = "Human-reviewed comment workflow"),
        (name = "engagements", description = "Manually recorded engagement"),
        (name = "search-profiles", description = "Verticals, keywords and negative keywords"),
        (name = "communities", description = "Community validation state"),
        (name = "sources", description = "Authorized data sources"),
        (name = "apify", description = "Actor registry, runs and spending"),
        (name = "providers", description = "AI providers, usage and benchmarks"),
        (name = "import", description = "Manual JSON import"),
        (name = "jobs", description = "Durable job queue")
    )
)]
pub struct ApiDoc;

// ---------------------------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------------------------

use serde::Deserialize;

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Pagination {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
}

impl Pagination {
    pub fn limit(&self) -> i64 {
        self.per_page.unwrap_or(25).clamp(1, 100)
    }

    pub fn offset(&self) -> i64 {
        (self.page.unwrap_or(1).max(1) - 1) * self.limit()
    }

    pub fn page(&self) -> i64 {
        self.page.unwrap_or(1).max(1)
    }

    pub fn per_page(&self) -> i64 {
        self.limit()
    }
}

pub fn paged(items: serde_json::Value, pagination: &Pagination, total: i64) -> serde_json::Value {
    serde_json::json!({
        "items": items,
        "page": pagination.page(),
        "per_page": pagination.per_page(),
        "total": total,
    })
}

use crate::error::{AppError, AppResult};

pub fn require_non_empty(value: &str, field: &str) -> AppResult<()> {
    if value.trim().is_empty() {
        return Err(AppError::Validation(vec![crate::error::FieldError::new(
            field,
            "must not be empty",
        )]));
    }
    Ok(())
}

pub fn require_one_of(value: &str, field: &str, allowed: &[&str]) -> AppResult<()> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(AppError::Validation(vec![crate::error::FieldError::new(
            field,
            format!("must be one of {}", allowed.join(", ")),
        )]))
    }
}
