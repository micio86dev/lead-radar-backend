//! External provider adapters behind explicit traits. Provider-specific payloads never leave this
//! module: the rest of the system only sees `DecisionResult`, `GenerativeResult` and `SourceRun`.

pub mod apify;
pub mod mock;
pub mod qwen;
pub mod remote;

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, DecisionChoice, GenerativeChoice, SourceChoice};
use crate::domain::decision::{DecisionQuestion, DecisionResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderMetadata {
    pub provider: String,
    pub kind: String,
    pub model: Option<String>,
    pub configured: bool,
    pub enabled: bool,
    pub base_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderHealth {
    pub provider: String,
    /// `ok`, `error`, `unconfigured`, `disabled`
    pub status: String,
    pub latency_ms: Option<i64>,
    pub message: Option<String>,
}

impl ProviderHealth {
    pub fn unconfigured(provider: &str) -> Self {
        Self {
            provider: provider.to_string(),
            status: "unconfigured".to_string(),
            latency_ms: None,
            message: Some("credentials are not configured".to_string()),
        }
    }

    pub fn ok(provider: &str, latency_ms: i64) -> Self {
        Self {
            provider: provider.to_string(),
            status: "ok".to_string(),
            latency_ms: Some(latency_ms),
            message: None,
        }
    }

    pub fn error(provider: &str, message: impl Into<String>) -> Self {
        Self {
            provider: provider.to_string(),
            status: "error".to_string(),
            latency_ms: None,
            message: Some(message.into()),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("provider {0} is not configured")]
    Unconfigured(String),
    #[error("provider {0} is disabled")]
    Disabled(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("unauthorized: {0}")]
    Unauthorized(String),
    #[error("rate limited")]
    RateLimited { retry_after_seconds: Option<i64> },
    #[error("upstream error {status}: {message}")]
    Upstream { status: u16, message: String },
    #[error("provider unavailable: {0}")]
    Unavailable(String),
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
    #[error("transport error: {0}")]
    Transport(String),
}

impl ProviderError {
    /// Only transient failures may be retried. A malformed request or a rejected key never is.
    pub fn is_retryable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. }
            | ProviderError::Unavailable(_)
            | ProviderError::Transport(_) => true,
            ProviderError::Upstream { status, .. } => *status >= 500,
            ProviderError::Unconfigured(_)
            | ProviderError::Disabled(_)
            | ProviderError::InvalidRequest(_)
            | ProviderError::Unauthorized(_)
            | ProviderError::InvalidResponse(_) => false,
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            ProviderError::Unconfigured(_) => "provider_unconfigured",
            ProviderError::Disabled(_) => "provider_disabled",
            ProviderError::InvalidRequest(_) => "invalid_request",
            ProviderError::Unauthorized(_) => "unauthorized",
            ProviderError::RateLimited { .. } => "rate_limited",
            ProviderError::Upstream { .. } => "upstream_error",
            ProviderError::Unavailable(_) => "unavailable",
            ProviderError::InvalidResponse(_) => "invalid_response",
            ProviderError::Transport(_) => "transport_error",
        }
    }

    pub fn retry_after_seconds(&self) -> Option<i64> {
        match self {
            ProviderError::RateLimited {
                retry_after_seconds,
            } => *retry_after_seconds,
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecisionRequest {
    pub state: String,
    pub questions: Vec<DecisionQuestion>,
    /// Sent as `Idempotency-Key` so a retry can never buy a second decision.
    pub idempotency_key: Option<String>,
}

#[async_trait]
pub trait DecisionProvider: Send + Sync {
    fn metadata(&self) -> ProviderMetadata;

    async fn health_check(&self) -> ProviderHealth;

    async fn evaluate(&self, request: &DecisionRequest) -> Result<DecisionResult, ProviderError>;
}

#[derive(Debug, Clone)]
pub struct GenerativeResult {
    pub provider: String,
    pub model: String,
    pub request_id: Option<String>,
    pub content: serde_json::Value,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub latency_ms: i64,
}

#[async_trait]
pub trait GenerativeProvider: Send + Sync {
    fn metadata(&self) -> ProviderMetadata;

    async fn health_check(&self) -> ProviderHealth;

    /// Ask for a JSON object. The adapter validates that the payload parses; the caller validates
    /// the schema it needs.
    async fn generate_json(
        &self,
        system: &str,
        user: &str,
        schema_hint: &str,
    ) -> Result<GenerativeResult, ProviderError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Aborted,
    TimedOut,
}

impl RunStatus {
    pub fn terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Aborted | RunStatus::TimedOut
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Queued => "queued",
            RunStatus::Running => "running",
            RunStatus::Succeeded => "succeeded",
            RunStatus::Failed => "failed",
            RunStatus::Aborted => "aborted",
            RunStatus::TimedOut => "timed_out",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SourceRunRequest {
    pub actor_id: String,
    pub input: serde_json::Value,
    pub max_items: i64,
}

#[derive(Debug, Clone)]
pub struct SourceRun {
    pub provider_run_id: String,
    pub status: RunStatus,
    pub dataset_id: Option<String>,
    pub items_count: i64,
    /// Provider-reported cost in its own currency, with that currency named.
    pub cost_usd: Option<f64>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

#[async_trait]
pub trait SourceProvider: Send + Sync {
    fn metadata(&self) -> ProviderMetadata;

    async fn health_check(&self) -> ProviderHealth;

    async fn start_run(&self, request: &SourceRunRequest) -> Result<SourceRun, ProviderError>;

    async fn get_run(&self, provider_run_id: &str) -> Result<SourceRun, ProviderError>;

    async fn fetch_items(
        &self,
        dataset_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<serde_json::Value>, ProviderError>;

    async fn cancel_run(&self, provider_run_id: &str) -> Result<(), ProviderError>;
}

/// Runtime providers selected by configuration. A missing credential never blocks startup: the
/// adapter exists and reports `unconfigured`, which is visible in the dashboard.
pub struct Providers {
    pub decision: Arc<dyn DecisionProvider>,
    /// Second decision provider, used only by benchmarks.
    pub laya: Option<Arc<dyn DecisionProvider>>,
    pub generative: Arc<dyn GenerativeProvider>,
    pub source: Arc<dyn SourceProvider>,
}

impl Providers {
    pub fn from_config(config: &AppConfig) -> anyhow::Result<Self> {
        let decision: Arc<dyn DecisionProvider> = match config.decision_provider {
            DecisionChoice::Jev => Arc::new(remote::JevProvider::new(config.jev.clone())?),
            DecisionChoice::Mock => Arc::new(mock::MockDecisionProvider::new()),
        };
        let laya: Option<Arc<dyn DecisionProvider>> = if config.laya.enabled {
            Some(Arc::new(remote::LayaProvider::new(config.laya.clone())?))
        } else {
            None
        };
        let generative: Arc<dyn GenerativeProvider> = match config.generative_provider {
            GenerativeChoice::Qwen => Arc::new(qwen::QwenProvider::new(config.qwen.clone())?),
            GenerativeChoice::Mock => Arc::new(mock::MockGenerativeProvider::new()),
        };
        let source: Arc<dyn SourceProvider> = match config.source_provider {
            SourceChoice::Apify => Arc::new(apify::ApifyProvider::new(config.apify.clone())?),
            SourceChoice::Mock => Arc::new(mock::MockSourceProvider::new()),
        };
        Ok(Self {
            decision,
            laya,
            generative,
            source,
        })
    }
}
