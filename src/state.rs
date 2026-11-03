//! Shared application state for the API and the worker.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;

use crate::auth::AuthService;
use crate::config::AppConfig;
use crate::db::Db;
use crate::providers::Providers;

pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: Db,
    pub auth: AuthService,
    pub providers: Providers,
    pub started_at: Instant,
}

pub type SharedState = Arc<AppState>;

impl AppState {
    pub async fn build(config: AppConfig) -> Result<Self> {
        config.ensure_database_parent()?;
        let db = Db::connect(&config.database_url).await?;
        let auth = AuthService::new(&config, db.clone());
        let providers = Providers::from_config(&config)?;
        Ok(Self {
            config: Arc::new(config),
            db,
            auth,
            providers,
            started_at: Instant::now(),
        })
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }
}
