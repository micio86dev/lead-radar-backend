//! Worker binary: durable job processing only.

use lead_radar::config::AppConfig;
use lead_radar::constants::init_tracing;
use lead_radar::state::AppState;
use lead_radar::worker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let config = AppConfig::from_env()?;
    init_tracing(&config.log_level);
    tracing::info!(
        env = config.app_env.as_str(),
        concurrency = config.worker.concurrency,
        ai_concurrency = config.worker.ai_concurrency,
        "starting worker"
    );
    let state = std::sync::Arc::new(AppState::build(config).await?);
    state.db.migrate().await?;
    worker::run(state).await
}
