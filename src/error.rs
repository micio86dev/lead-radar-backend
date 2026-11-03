//! Error type shared by the API surface. Domain code returns these; HTTP mapping lives here so
//! handlers stay free of status codes.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

/// One field-level validation problem.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FieldError {
    pub field: String,
    pub message: String,
}

impl FieldError {
    pub fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

/// Reasons the spend guard refuses to queue a billable run. Serialized verbatim to the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostGuardReason {
    KillSwitch,
    BudgetExhausted,
    DailyCeilingReached,
    ConcurrencyLimit,
    ActorNotEnabled,
    ActorNotValidated,
    RunCostAboveLimit,
    ProviderUnconfigured,
    ItemsLimitInvalid,
}

impl CostGuardReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::KillSwitch => "kill_switch",
            Self::BudgetExhausted => "budget_exhausted",
            Self::DailyCeilingReached => "daily_ceiling_reached",
            Self::ConcurrencyLimit => "concurrency_limit",
            Self::ActorNotEnabled => "actor_not_enabled",
            Self::ActorNotValidated => "actor_not_validated",
            Self::RunCostAboveLimit => "run_cost_above_limit",
            Self::ProviderUnconfigured => "provider_unconfigured",
            Self::ItemsLimitInvalid => "limit_invalid",
        }
    }

    pub fn human_message(&self) -> &'static str {
        match self {
            Self::KillSwitch => "Apify kill switch is engaged; no billable run may be queued",
            Self::BudgetExhausted => "configured Apify budget is exhausted",
            Self::DailyCeilingReached => "daily Apify spending ceiling reached",
            Self::ConcurrencyLimit => "maximum concurrent Apify runs already in flight",
            Self::ActorNotEnabled => "Actor is disabled in the registry",
            Self::ActorNotValidated => "Actor validation has not been recorded",
            Self::RunCostAboveLimit => "estimated run cost exceeds the configured per-run limit",
            Self::ProviderUnconfigured => "Apify credentials are not configured",
            Self::ItemsLimitInvalid => "requested item limit is outside the configured bounds",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("validation failed")]
    Validation(Vec<FieldError>),
    #[error("{0}")]
    BadRequest(String),
    #[error("unauthorized")]
    Unauthorized,
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("not found")]
    NotFound,
    #[error("{0}")]
    Conflict(String),
    #[error("cost guard rejected: {}", .0.as_str())]
    CostGuardRejected(CostGuardReason),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("not ready: {0}")]
    NotReady(String),
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("internal error: {0}")]
    Internal(#[from] anyhow::Error),
}

impl From<serde_json::Error> for AppError {
    fn from(error: serde_json::Error) -> Self {
        AppError::Internal(anyhow::Error::new(error))
    }
}

impl AppError {
    pub fn conflict_detail(code: &str, message: impl Into<String>) -> Self {
        AppError::Conflict(format!("{code}: {}", message.into()))
    }
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message, details) = match self {
            AppError::Validation(errors) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_failed",
                "Request body is invalid".to_string(),
                Some(serde_json::to_value(errors).unwrap_or(serde_json::Value::Null)),
            ),
            AppError::BadRequest(message) => {
                (StatusCode::BAD_REQUEST, "bad_request", message, None)
            }
            AppError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "Missing or invalid bearer token".to_string(),
                None,
            ),
            AppError::Forbidden(message) => (StatusCode::FORBIDDEN, "forbidden", message, None),
            AppError::NotFound => (
                StatusCode::NOT_FOUND,
                "not_found",
                "Resource not found".to_string(),
                None,
            ),
            AppError::Conflict(message) => (StatusCode::CONFLICT, "conflict", message, None),
            AppError::CostGuardRejected(reason) => (
                StatusCode::CONFLICT,
                "cost_guard_rejected",
                reason.human_message().to_string(),
                Some(serde_json::json!({ "reason": reason.as_str() })),
            ),
            AppError::Provider(message) => {
                (StatusCode::BAD_GATEWAY, "provider_error", message, None)
            }
            AppError::NotReady(message) => {
                (StatusCode::SERVICE_UNAVAILABLE, "not_ready", message, None)
            }
            AppError::Database(error) => {
                tracing::error!(error = %error, "database error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Database operation failed".to_string(),
                    None,
                )
            }
            AppError::Internal(error) => {
                tracing::error!(error = %error, "internal error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal_error",
                    "Internal server error".to_string(),
                    None,
                )
            }
        };

        let envelope = ErrorEnvelope {
            error: ErrorBody {
                code,
                message,
                details,
            },
        };
        (status, Json(envelope)).into_response()
    }
}

pub type AppResult<T> = Result<T, AppError>;
