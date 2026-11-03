//! Database maintenance CLI: migrate, seed, purge (retention) and an explicitly confirmed reset.

use lead_radar::config::AppConfig;
use lead_radar::constants::init_tracing;
use lead_radar::db::Db;
use lead_radar::domain::time::{now_iso, to_iso};
use lead_radar::modules::tenancy;
use lead_radar::seed;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let config = AppConfig::from_env()?;
    init_tracing(&config.log_level);
    config.ensure_database_parent()?;
    let db = Db::connect(&config.database_url).await?;
    db.migrate().await?;

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("migrate") => {
            println!("migrations applied");
        }
        Some("seed") => {
            let report = seed::seed_all(&config, &db).await?;
            println!("{}", seed::report_json(&report));
        }
        Some("purge") => {
            let days = args
                .get(1)
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(config.retention_days);
            let cutoff = to_iso(chrono::Utc::now() - chrono::Duration::days(days.max(1)));
            let result = sqlx::query(
                "UPDATE discovered_posts SET content = '[removed]', title = NULL, author = NULL, \
                 source_url = NULL, raw = '{}', deleted_at = ? WHERE deleted_at IS NULL \
                 AND created_at < ?",
            )
            .bind(now_iso())
            .bind(&cutoff)
            .execute(&db.writer)
            .await?;
            println!(
                "removed content for {} posts older than {days} days",
                result.rows_affected()
            );
        }
        Some("reset") => {
            if !args.iter().any(|arg| arg == "--confirm") {
                anyhow::bail!(
                    "reset deletes every tenant, lead and audit record. Re-run with `db reset --confirm`."
                );
            }
            seed::reset_data(&db).await?;
            println!("database reset");
        }
        Some("rescore") => {
            let outcome = lead_radar::modules::qualification::rescore_tenant(
                &db,
                &config.default_tenant_id,
                chrono::Utc::now(),
            )
            .await?;
            println!("{}", serde_json::to_string(&outcome)?);
        }
        Some("token") => {
            let token = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("usage: db token <plaintext-token> (prints the stored hash)")
            })?;
            println!("{}", lead_radar::auth::hash_token(token));
        }
        Some("settings") => {
            let settings = tenancy::all_settings(&db, &config.default_tenant_id).await?;
            println!("{}", serde_json::to_string_pretty(&settings)?);
        }
        other => {
            anyhow::bail!(
                "unknown command {other:?}; use migrate, seed, rescore, purge [days], reset --confirm, token <value>, settings"
            )
        }
    }
    db.close().await;
    Ok(())
}
