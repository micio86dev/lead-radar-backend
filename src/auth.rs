//! Authentication and tenant resolution.
//!
//! The tenant is derived from the bearer token and the database. A tenant identifier supplied by a
//! client is ignored everywhere. In development/test the seeded local identity is reachable with
//! `DEV_API_TOKEN`; in production that path is disabled at configuration validation and only
//! hashed tokens stored in `api_tokens` are accepted.

use axum::extract::FromRequestParts;
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use sha2::{Digest, Sha256};

use crate::config::{AppConfig, AppEnv};
use crate::db::Db;
use crate::domain::enums::Role;
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenantContext {
    pub tenant_id: String,
    pub user_id: String,
    pub role: Role,
    /// Value recorded in audit logs and review events.
    pub actor: String,
}

impl TenantContext {
    pub fn is_reader(&self) -> bool {
        self.role == Role::Viewer || self.role == Role::Reviewer
    }

    /// Review decisions require at least a reviewer.
    pub fn can_review(&self) -> bool {
        matches!(self.role, Role::Owner | Role::Admin | Role::Reviewer)
    }

    pub fn require_reviewer(&self) -> AppResult<()> {
        if self.can_review() {
            Ok(())
        } else {
            Err(AppError::Forbidden(
                "role viewer cannot review comments".to_string(),
            ))
        }
    }
}

/// SHA-256 hex digest used for stored tokens; the plaintext never touches the database.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, Clone)]
pub struct AuthService {
    db: Db,
    dev_token_hash: Option<String>,
    dev_tenant_id: String,
    dev_user_id: String,
    allow_dev_token: bool,
    app_env: AppEnv,
}

impl AuthService {
    pub fn new(config: &AppConfig, db: Db) -> Self {
        Self {
            db,
            dev_token_hash: config.dev_api_token.as_deref().map(hash_token),
            dev_tenant_id: config.default_tenant_id.clone(),
            dev_user_id: crate::seed::DEV_USER_ID.to_string(),
            allow_dev_token: config.app_env.allows_local_identity(),
            app_env: config.app_env,
        }
    }

    pub fn from_parts(
        db: Db,
        dev_token: Option<&str>,
        dev_tenant_id: &str,
        dev_user_id: &str,
        app_env: AppEnv,
    ) -> Self {
        Self {
            db,
            dev_token_hash: dev_token.map(hash_token),
            dev_tenant_id: dev_tenant_id.to_string(),
            dev_user_id: dev_user_id.to_string(),
            allow_dev_token: app_env.allows_local_identity(),
            app_env,
        }
    }

    pub async fn authenticate(
        &self,
        authorization: Option<&str>,
    ) -> Result<TenantContext, AppError> {
        let token = authorization
            .and_then(|value| value.trim().strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or(AppError::Unauthorized)?;
        let token_hash = hash_token(token);

        let row: Option<(String, String, String)> = sqlx::query_as(
            "SELECT t.tenant_id, t.user_id, tu.role \
             FROM api_tokens t \
             JOIN tenant_users tu ON tu.tenant_id = t.tenant_id AND tu.user_id = t.user_id \
             WHERE t.token_hash = ? AND t.revoked_at IS NULL",
        )
        .bind(&token_hash)
        .fetch_optional(&self.db.reader)
        .await?;

        if let Some((tenant_id, user_id, role)) = row {
            let role = Role::parse(&role).ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!("invalid role {role:?} in tenant_users"))
            })?;
            return Ok(TenantContext {
                actor: format!("user:{user_id}"),
                tenant_id,
                user_id,
                role,
            });
        }

        // Development-only local identity: the seeded tenant/user must exist.
        if self.allow_dev_token {
            if let Some(expected) = &self.dev_token_hash {
                if &token_hash == expected {
                    let role: Option<String> = sqlx::query_scalar(
                        "SELECT role FROM tenant_users WHERE tenant_id = ? AND user_id = ?",
                    )
                    .bind(&self.dev_tenant_id)
                    .bind(&self.dev_user_id)
                    .fetch_optional(&self.db.reader)
                    .await?;
                    if let Some(role) = role {
                        let role = Role::parse(&role).ok_or_else(|| {
                            AppError::Internal(anyhow::anyhow!(
                                "invalid role {role:?} for the development identity"
                            ))
                        })?;
                        return Ok(TenantContext {
                            actor: "dev-local".to_string(),
                            tenant_id: self.dev_tenant_id.clone(),
                            user_id: self.dev_user_id.clone(),
                            role,
                        });
                    }
                    return Err(AppError::Internal(anyhow::anyhow!(
                        "development identity is not seeded; run `db seed` (APP_ENV={})",
                        self.app_env.as_str()
                    )));
                }
            }
        }

        Err(AppError::Unauthorized)
    }
}

impl FromRequestParts<SharedState> for TenantContext {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &SharedState,
    ) -> Result<Self, Self::Rejection> {
        let authorization = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        state.auth.authenticate(authorization).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hashes_are_stable_and_not_reversible_looking() {
        assert_eq!(hash_token("dev-local-token").len(), 64);
        assert_eq!(hash_token("dev-local-token"), hash_token("dev-local-token"));
        assert_ne!(hash_token("a"), hash_token("b"));
    }

    #[test]
    fn reviewer_permissions_are_checked_server_side() {
        let reviewer = TenantContext {
            tenant_id: "t".into(),
            user_id: "u".into(),
            role: Role::Reviewer,
            actor: "user:u".into(),
        };
        assert!(reviewer.require_reviewer().is_ok());
        let viewer = TenantContext {
            role: Role::Viewer,
            ..reviewer
        };
        assert!(viewer.require_reviewer().is_err());
    }
}
