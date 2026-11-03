//! API binary: HTTP surface only.

use lead_radar::api;
use lead_radar::config::AppConfig;
use lead_radar::constants::init_tracing;
use lead_radar::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let config = AppConfig::from_env()?;
    init_tracing(&config.log_level);
    tracing::info!(
        env = config.app_env.as_str(),
        decision_provider = config.decision_provider.as_str(),
        generative_provider = config.generative_provider.as_str(),
        source_provider = config.source_provider.as_str(),
        "starting api"
    );
    let state = std::sync::Arc::new(AppState::build(config).await?);
    state.db.migrate().await?;
    api::serve(state).await
}
