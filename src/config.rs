//! Environment configuration.
//!
//! Rules: secrets come from the environment only, every documented variable has a name here,
//! optional providers never prevent startup, and invalid values fail fast with a clear message.

use std::collections::HashMap;
use std::env;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnv {
    Development,
    Test,
    Production,
}

impl AppEnv {
    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "development" | "dev" => Ok(Self::Development),
            "test" => Ok(Self::Test),
            "production" | "prod" => Ok(Self::Production),
            other => bail!("APP_ENV must be development, test or production (got {other:?})"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Test => "test",
            Self::Production => "production",
        }
    }

    /// Development-only conveniences are allowed here (local identity provider, mock providers).
    pub fn allows_local_identity(&self) -> bool {
        matches!(self, Self::Development | Self::Test)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionChoice {
    Jev,
    Mock,
}

impl DecisionChoice {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "jev" => Ok(Self::Jev),
            "mock" => Ok(Self::Mock),
            other => bail!("DECISION_PROVIDER must be jev or mock (got {other:?})"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::Mock => "mock",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenerativeChoice {
    Qwen,
    Mock,
}

impl GenerativeChoice {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "qwen" => Ok(Self::Qwen),
            "mock" => Ok(Self::Mock),
            other => bail!("GENERATIVE_PROVIDER must be qwen or mock (got {other:?})"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Qwen => "qwen",
            Self::Mock => "mock",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceChoice {
    Apify,
    Mock,
}

impl SourceChoice {
    fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "apify" => Ok(Self::Apify),
            "mock" => Ok(Self::Mock),
            other => bail!("SOURCE_PROVIDER must be apify or mock (got {other:?})"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Apify => "apify",
            Self::Mock => "mock",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ApifyConfig {
    pub api_token: Option<String>,
    pub base_url: String,
    pub max_concurrent_runs: i64,
    pub max_items_per_run: i64,
    pub max_run_cost_eur: f64,
    /// Provider-side per-run cap in USD (`maxTotalChargeUsd`), enforced by Apify itself.
    pub max_total_charge_usd: f64,
    /// Daily ceiling on provider-reported USD spend. Apify reports USD and the EUR ceilings cannot be
    /// compared with it without inventing an exchange rate, so this is a separate, explicit ceiling.
    pub daily_charge_usd: f64,
    pub daily_budget_eur: f64,
    pub total_budget_eur: f64,
    pub kill_switch: bool,
    pub run_timeout_seconds: i64,
    pub poll_interval_seconds: i64,
}

impl ApifyConfig {
    pub fn configured(&self) -> bool {
        self.api_token
            .as_deref()
            .map(|token| !token.trim().is_empty())
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone)]
pub struct JevConfig {
    pub api_key: Option<String>,
    pub base_url: String,
    pub api_path: String,
    pub model: String,
    pub timeout_seconds: i64,
}

impl JevConfig {
    pub fn configured(&self) -> bool {
        has_secret(&self.api_key)
    }

    pub fn endpoint(&self) -> String {
        join_url(&self.base_url, &self.api_path)
    }
}

/// Models served by the Laya decision API (from its published OpenAPI document).
pub const LAYA_MODELS: [&str; 3] = ["english", "multilingual", "typed-decisions"];

#[derive(Debug, Clone)]
pub struct LayaConfig {
    pub enabled: bool,
    pub api_key: Option<String>,
    pub base_url: String,
    /// Empty means "let the provider route automatically".
    pub model: Option<String>,
    pub timeout_seconds: i64,
}

impl LayaConfig {
    pub fn configured(&self) -> bool {
        has_secret(&self.api_key)
    }

    pub fn usable(&self) -> bool {
        self.enabled && self.configured()
    }

    pub fn endpoint(&self) -> String {
        join_url(&self.base_url, "/v1/systemone")
    }
}

#[derive(Debug, Clone)]
pub struct QwenConfig {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub timeout_seconds: i64,
    pub max_tokens: i64,
}

impl QwenConfig {
    pub fn configured(&self) -> bool {
        has_secret(&self.api_key) && non_empty(&self.base_url) && non_empty(&self.model)
    }

    /// OpenAI-compatible chat completions endpoint.
    ///
    /// Hosted providers document their base URL either as `https://host` or as
    /// `https://host/compatible-mode/v1` (the OpenAI SDK convention). Both shapes are accepted:
    /// the version segment is added only when it is missing.
    pub fn endpoint(&self) -> Option<String> {
        self.base_url
            .as_deref()
            .filter(|base| !base.trim().is_empty())
            .map(|base| {
                let base = base.trim().trim_end_matches('/');
                if base.ends_with("/v1") {
                    join_url(base, "/chat/completions")
                } else {
                    join_url(base, "/v1/chat/completions")
                }
            })
    }
}

#[derive(Debug, Clone)]
pub struct WorkerConfig {
    pub concurrency: i64,
    pub ai_concurrency: i64,
    pub poll_interval_seconds: i64,
    pub max_attempts: i64,
    pub retry_backoff_seconds: i64,
    pub lease_seconds: i64,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub app_env: AppEnv,
    pub host: String,
    pub port: u16,
    pub database_url: String,
    pub log_level: String,
    pub default_tenant_id: String,
    pub dev_api_token: Option<String>,
    /// Production bootstrap: a strong token stored (hashed) during seeding so the dashboard can
    /// authenticate before any token-management UI exists. Rotate it like any other credential.
    pub bootstrap_api_token: Option<String>,
    pub cors_allowed_origins: Vec<String>,
    pub retention_days: i64,
    pub decision_provider: DecisionChoice,
    pub generative_provider: GenerativeChoice,
    pub source_provider: SourceChoice,
    pub apify: ApifyConfig,
    pub jev: JevConfig,
    pub laya: LayaConfig,
    pub qwen: QwenConfig,
    pub worker: WorkerConfig,
    /// Conservative wait applied after a recorded publication. Reddit's real limit belongs to the
    /// account; this floor only stops the dashboard from encouraging a reply too soon.
    pub reply_min_interval_minutes: i64,
}

impl AppConfig {
    /// Load configuration from the process environment (after `.env` is loaded by the binary).
    pub fn from_env() -> Result<Self> {
        let mut vars = HashMap::new();
        for (key, value) in env::vars() {
            vars.insert(key, value);
        }
        Self::from_map(&vars)
    }

    /// Load configuration from an explicit map; used by tests and by `from_env`.
    pub fn from_map(vars: &HashMap<String, String>) -> Result<Self> {
        let get = |key: &str| vars.get(key).map(|value| value.trim().to_string());
        let required = |key: &str| {
            get(key).ok_or_else(|| anyhow!("missing required environment variable {key}"))
        };
        let number = |key: &str, default: f64| -> Result<f64> {
            match get(key) {
                Some(raw) if !raw.is_empty() => raw
                    .parse::<f64>()
                    .with_context(|| format!("{key} must be a number (got {raw:?})")),
                _ => Ok(default),
            }
        };
        let integer = |key: &str, default: i64| -> Result<i64> {
            match get(key) {
                Some(raw) if !raw.is_empty() => raw
                    .parse::<i64>()
                    .with_context(|| format!("{key} must be an integer (got {raw:?})")),
                _ => Ok(default),
            }
        };
        let flag = |key: &str, default: bool| -> Result<bool> {
            match get(key) {
                Some(raw) if !raw.is_empty() => match raw.to_ascii_lowercase().as_str() {
                    "true" | "1" | "yes" | "on" => Ok(true),
                    "false" | "0" | "no" | "off" => Ok(false),
                    other => bail!("{key} must be true or false (got {other:?})"),
                },
                _ => Ok(default),
            }
        };

        let reply_min_interval_minutes = integer("REPLY_MIN_INTERVAL_MINUTES", 10)?;
        let app_env = AppEnv::parse(&required("APP_ENV")?)?;
        let port = integer("APP_PORT", 8080)?;
        let port: u16 = port
            .try_into()
            .map_err(|_| anyhow!("APP_PORT must be between 1 and 65535"))?;

        let default_tenant_id = get("DEFAULT_TENANT_ID").unwrap_or_default();
        if default_tenant_id.is_empty() {
            bail!("DEFAULT_TENANT_ID is required");
        }

        let decision_provider =
            DecisionChoice::parse(&get("DECISION_PROVIDER").unwrap_or_else(|| "jev".to_string()))?;
        let generative_provider = GenerativeChoice::parse(
            &get("GENERATIVE_PROVIDER").unwrap_or_else(|| "qwen".to_string()),
        )?;
        let source_provider =
            SourceChoice::parse(&get("SOURCE_PROVIDER").unwrap_or_else(|| "apify".to_string()))?;

        let laya_enabled = flag("LAYA_ENABLED", false)?;
        let laya_model = get("LAYA_MODEL").filter(|model| !model.is_empty());
        if laya_enabled {
            if let Some(model) = &laya_model {
                if !LAYA_MODELS.contains(&model.as_str()) {
                    bail!(
                        "LAYA_MODEL must be one of {:?} (got {:?}); Jev model ids are not valid \
                         Laya model ids",
                        LAYA_MODELS,
                        model
                    );
                }
            }
        }

        let config = Self {
            app_env,
            host: get("APP_HOST").unwrap_or_else(|| "0.0.0.0".to_string()),
            port,
            database_url: required("DATABASE_URL")?,
            log_level: get("LOG_LEVEL").unwrap_or_else(|| "info".to_string()),
            default_tenant_id,
            dev_api_token: get("DEV_API_TOKEN").filter(|token| !token.is_empty()),
            bootstrap_api_token: get("BOOTSTRAP_API_TOKEN").filter(|token| !token.is_empty()),
            cors_allowed_origins: get("CORS_ALLOWED_ORIGINS")
                .filter(|origins| !origins.is_empty())
                .map(|origins| {
                    origins
                        .split(',')
                        .map(|origin| origin.trim().to_string())
                        .filter(|origin| !origin.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            retention_days: integer("RETENTION_DAYS", 180)?,
            decision_provider,
            generative_provider,
            source_provider,
            apify: ApifyConfig {
                api_token: get("APIFY_API_TOKEN").filter(|token| !token.is_empty()),
                base_url: get("APIFY_BASE_URL")
                    .filter(|base| !base.is_empty())
                    .unwrap_or_else(|| "https://api.apify.com".to_string()),
                max_concurrent_runs: integer("APIFY_MAX_CONCURRENT_RUNS", 2)?,
                max_items_per_run: integer("APIFY_MAX_ITEMS_PER_RUN", 50)?,
                max_run_cost_eur: number("APIFY_MAX_RUN_COST", 0.5)?,
                max_total_charge_usd: number("APIFY_MAX_TOTAL_CHARGE_USD", 0.25)?,
                daily_charge_usd: number("APIFY_DAILY_CHARGE_USD", 1.0)?,
                daily_budget_eur: number("APIFY_DAILY_BUDGET", 2.0)?,
                total_budget_eur: number("APIFY_TOTAL_BUDGET", 25.0)?,
                kill_switch: flag("APIFY_KILL_SWITCH", false)?,
                run_timeout_seconds: integer("APIFY_RUN_TIMEOUT_SECONDS", 900)?,
                poll_interval_seconds: integer("APIFY_POLL_INTERVAL", 15)?,
            },
            jev: JevConfig {
                api_key: get("JEV_API_KEY").filter(|key| !key.is_empty()),
                base_url: get("JEV_BASE_URL")
                    .filter(|base| !base.is_empty())
                    .unwrap_or_else(|| "https://jev-ai.org".to_string()),
                api_path: get("JEV_API_PATH")
                    .filter(|path| !path.is_empty())
                    .unwrap_or_else(|| "/api/v1/systemone/".to_string()),
                model: get("JEV_MODEL")
                    .filter(|model| !model.is_empty())
                    .unwrap_or_else(|| "jev-1.13".to_string()),
                timeout_seconds: integer("JEV_TIMEOUT", 30)?,
            },
            laya: LayaConfig {
                enabled: laya_enabled,
                api_key: get("LAYA_API_KEY").filter(|key| !key.is_empty()),
                base_url: get("LAYA_BASE_URL")
                    .filter(|base| !base.is_empty())
                    .unwrap_or_else(|| "https://api.laya.studio".to_string()),
                model: laya_model,
                timeout_seconds: integer("LAYA_TIMEOUT", 30)?,
            },
            qwen: QwenConfig {
                api_key: get("QWEN_API_KEY").filter(|key| !key.is_empty()),
                base_url: get("QWEN_BASE_URL").filter(|base| !base.is_empty()),
                model: get("QWEN_MODEL").filter(|model| !model.is_empty()),
                timeout_seconds: integer("QWEN_TIMEOUT", 60)?,
                max_tokens: integer("QWEN_MAX_TOKENS", 1200)?,
            },
            worker: WorkerConfig {
                concurrency: integer("WORKER_CONCURRENCY", 4)?,
                ai_concurrency: integer("WORKER_AI_CONCURRENCY", 2)?,
                poll_interval_seconds: integer("WORKER_POLL_INTERVAL", 2)?,
                max_attempts: integer("JOB_MAX_ATTEMPTS", 5)?,
                retry_backoff_seconds: integer("JOB_RETRY_BACKOFF", 5)?,
                lease_seconds: integer("JOB_LEASE_SECONDS", 120)?,
            },
            reply_min_interval_minutes,
        };
        config.validate()?;
        Ok(config)
    }

    /// Startup validation. Optional providers may be absent; nonsense values may not.
    pub fn validate(&self) -> Result<()> {
        if !(1..=65535).contains(&self.port) {
            bail!("APP_PORT must be between 1 and 65535");
        }
        if self.database_url.trim().is_empty() {
            bail!("DATABASE_URL must not be empty");
        }
        if self.reply_min_interval_minutes < 1 || self.reply_min_interval_minutes > 1440 {
            bail!("REPLY_MIN_INTERVAL_MINUTES must be between 1 and 1440");
        }
        if self.retention_days < 1 || self.retention_days > 3650 {
            bail!("RETENTION_DAYS must be between 1 and 3650");
        }
        let apify = &self.apify;
        for (name, value) in [
            ("APIFY_MAX_CONCURRENT_RUNS", apify.max_concurrent_runs),
            ("APIFY_MAX_ITEMS_PER_RUN", apify.max_items_per_run),
            ("APIFY_RUN_TIMEOUT_SECONDS", apify.run_timeout_seconds),
            ("APIFY_POLL_INTERVAL", apify.poll_interval_seconds),
        ] {
            if value < 1 {
                bail!("{name} must be at least 1");
            }
        }
        for (name, value) in [
            ("APIFY_MAX_RUN_COST", apify.max_run_cost_eur),
            ("APIFY_MAX_TOTAL_CHARGE_USD", apify.max_total_charge_usd),
            ("APIFY_DAILY_CHARGE_USD", apify.daily_charge_usd),
            ("APIFY_DAILY_BUDGET", apify.daily_budget_eur),
            ("APIFY_TOTAL_BUDGET", apify.total_budget_eur),
        ] {
            if !value.is_finite() || value < 0.0 {
                bail!("{name} must be a non-negative number");
            }
        }
        if self.apify.daily_budget_eur > self.apify.total_budget_eur {
            bail!("APIFY_DAILY_BUDGET must not exceed APIFY_TOTAL_BUDGET");
        }
        if self.apify.max_run_cost_eur > self.apify.daily_budget_eur {
            bail!("APIFY_MAX_RUN_COST must not exceed APIFY_DAILY_BUDGET");
        }
        for (name, value) in [
            ("WORKER_CONCURRENCY", self.worker.concurrency),
            ("WORKER_AI_CONCURRENCY", self.worker.ai_concurrency),
            ("WORKER_POLL_INTERVAL", self.worker.poll_interval_seconds),
            ("JOB_MAX_ATTEMPTS", self.worker.max_attempts),
            ("JOB_RETRY_BACKOFF", self.worker.retry_backoff_seconds),
            ("JOB_LEASE_SECONDS", self.worker.lease_seconds),
        ] {
            if value < 1 {
                bail!("{name} must be at least 1");
            }
        }
        if self.worker.ai_concurrency > self.worker.concurrency {
            bail!("WORKER_AI_CONCURRENCY must not exceed WORKER_CONCURRENCY");
        }
        if let Some(token) = &self.bootstrap_api_token {
            if token.chars().count() < 24 {
                bail!("BOOTSTRAP_API_TOKEN must be at least 24 characters long");
            }
        }
        if self.app_env == AppEnv::Production && self.dev_api_token.is_some() {
            bail!(
                "DEV_API_TOKEN must not be set in production; store API tokens in the database \
                 and clear the variable"
            );
        }
        if self.app_env == AppEnv::Production
            && (self.decision_provider == DecisionChoice::Mock
                || self.generative_provider == GenerativeChoice::Mock
                || self.source_provider == SourceChoice::Mock)
        {
            bail!("mock providers are development/test only; production requires real adapters");
        }
        Ok(())
    }

    /// Ensure the SQLite parent directory exists before the pool tries to create the file.
    pub fn ensure_database_parent(&self) -> Result<()> {
        if let Some(path) = sqlite_file_path(&self.database_url) {
            if let Some(parent) = Path::new(&path).parent() {
                if !parent.as_os_str().is_empty() && !parent.exists() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating database directory {parent:?}"))?;
                }
            }
        }
        Ok(())
    }
}

fn has_secret(value: &Option<String>) -> bool {
    value
        .as_deref()
        .map(|secret| !secret.trim().is_empty())
        .unwrap_or(false)
}

fn non_empty(value: &Option<String>) -> bool {
    has_secret(value)
}

fn join_url(base: &str, path: &str) -> String {
    format!(
        "{}/{}",
        base.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// Path of a file-backed SQLite database URL, or `None` for in-memory databases.
pub fn sqlite_file_path(database_url: &str) -> Option<String> {
    let url = database_url.trim();
    if url.contains(":memory:") {
        return None;
    }
    let without_query = url.split('?').next().unwrap_or(url);
    let path = without_query
        .strip_prefix("sqlite://")
        .or_else(|| without_query.strip_prefix("sqlite:"))
        .unwrap_or(without_query);
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_map() -> HashMap<String, String> {
        let mut vars = HashMap::new();
        vars.insert("APP_ENV".into(), "test".into());
        vars.insert("DATABASE_URL".into(), "sqlite::memory:".into());
        vars.insert(
            "DEFAULT_TENANT_ID".into(),
            "00000000-0000-0000-0000-000000000001".into(),
        );
        vars
    }

    #[test]
    fn defaults_are_conservative() {
        let config = AppConfig::from_map(&base_map()).expect("config");
        assert_eq!(config.apify.max_items_per_run, 50);
        assert_eq!(config.apify.max_concurrent_runs, 2);
        assert_eq!(config.apify.total_budget_eur, 25.0);
        assert_eq!(config.apify.max_total_charge_usd, 0.25);
        assert_eq!(config.apify.daily_charge_usd, 1.0);
        assert!(config.apify.daily_budget_eur <= config.apify.total_budget_eur);
        assert!(!config.apify.kill_switch);
        assert_eq!(config.jev.model, "jev-1.13");
        assert_eq!(
            config.jev.endpoint(),
            "https://jev-ai.org/api/v1/systemone/"
        );
        assert_eq!(config.decision_provider, DecisionChoice::Jev);
        assert!(!config.laya.usable());
        assert_eq!(config.worker.max_attempts, 5);
    }

    #[test]
    fn invalid_budgets_are_rejected() {
        let mut vars = base_map();
        vars.insert("APIFY_MAX_RUN_COST".into(), "5".into());
        vars.insert("APIFY_DAILY_BUDGET".into(), "1".into());
        let error = AppConfig::from_map(&vars).expect_err("must fail");
        assert!(error.to_string().contains("APIFY_MAX_RUN_COST"));
    }

    #[test]
    fn laya_rejects_jev_model_ids() {
        let mut vars = base_map();
        vars.insert("LAYA_ENABLED".into(), "true".into());
        vars.insert("LAYA_MODEL".into(), "jev-1.13".into());
        let error = AppConfig::from_map(&vars).expect_err("must fail");
        assert!(error.to_string().contains("LAYA_MODEL"));
    }

    #[test]
    fn production_rejects_mock_providers_and_dev_token() {
        let mut vars = base_map();
        vars.insert("APP_ENV".into(), "production".into());
        vars.insert("DECISION_PROVIDER".into(), "mock".into());
        let error = AppConfig::from_map(&vars).expect_err("must fail");
        assert!(error.to_string().contains("mock providers"));

        let mut vars = base_map();
        vars.insert("APP_ENV".into(), "production".into());
        vars.insert("DEV_API_TOKEN".into(), "dev".into());
        let error = AppConfig::from_map(&vars).expect_err("must fail");
        assert!(error.to_string().contains("DEV_API_TOKEN"));
    }

    #[test]
    fn missing_optional_providers_is_not_fatal() {
        let config = AppConfig::from_map(&base_map()).expect("config");
        assert!(!config.jev.configured());
        assert!(!config.qwen.configured());
        assert!(!config.apify.configured());
    }

    #[test]
    fn qwen_endpoint_accepts_both_base_url_conventions() {
        let mut vars = base_map();
        vars.insert("QWEN_BASE_URL".into(), "https://host.example".into());
        let config = AppConfig::from_map(&vars).expect("config");
        assert_eq!(
            config.qwen.endpoint().as_deref(),
            Some("https://host.example/v1/chat/completions")
        );

        vars.insert(
            "QWEN_BASE_URL".into(),
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/".into(),
        );
        let config = AppConfig::from_map(&vars).expect("config");
        assert_eq!(
            config.qwen.endpoint().as_deref(),
            Some("https://dashscope-intl.aliyuncs.com/compatible-mode/v1/chat/completions")
        );
    }

    #[test]
    fn a_short_bootstrap_token_is_rejected() {
        let mut vars = base_map();
        vars.insert("BOOTSTRAP_API_TOKEN".into(), "too-short".into());
        let error = AppConfig::from_map(&vars).expect_err("must fail");
        assert!(error.to_string().contains("BOOTSTRAP_API_TOKEN"));
    }

    #[test]
    fn sqlite_paths_are_extracted() {
        assert_eq!(
            sqlite_file_path("sqlite://data/lead-radar.db?mode=rwc").as_deref(),
            Some("data/lead-radar.db")
        );
        assert_eq!(sqlite_file_path("sqlite::memory:"), None);
    }
}
