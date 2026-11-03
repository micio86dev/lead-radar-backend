//! Idempotent seeds. Repeatable by construction: every statement is `ON CONFLICT DO NOTHING`, so a
//! second run changes nothing and never overwrites operator edits. No secret is ever seeded; provider
//! defaults are recorded without credentials.

use serde_json::json;

use crate::config::AppConfig;
use crate::db::Db;
use crate::domain::enums::{Role, ValidationStatus, Vertical};
use crate::domain::scoring::ScoringConfig;
use crate::domain::time::now_iso;
use crate::modules::{ingestion, new_id, qualification, sources, tenancy, to_json};

pub const DEV_USER_ID: &str = "00000000-0000-0000-0000-000000000002";
const DEV_EMAIL: &str = "dev@lead-radar.local";
const DEV_NAME: &str = "Local Developer";

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct SeedReport {
    pub tenant_id: String,
    pub profiles: usize,
    pub keywords: usize,
    pub communities: usize,
    pub scoring_version: i64,
    pub benchmark_items: usize,
    pub manual_source: bool,
    pub dev_token_created: bool,
}

/// Verticals, their keyword lists and the shared negative keywords.
fn vertical_keywords() -> Vec<(Vertical, Vec<&'static str>)> {
    vec![
        (
            Vertical::HrRecruiting,
            vec![
                "multilingual recruitment",
                "international hiring",
                "language barrier interview",
                "multilingual interviews",
                "real-time translation interview",
                "global workforce communication",
                "non-native speakers interview",
                "cross-border recruitment",
                "interview interpretation",
                "international HR communication",
            ],
        ),
        (
            Vertical::InternationalMeetings,
            vec![
                "multilingual meetings",
                "live meeting translation",
                "real-time meeting interpreter",
                "international remote teams",
                "language barrier at work",
                "translate Microsoft Teams meetings",
                "translate Zoom meetings",
                "global team communication",
                "cross-language collaboration",
                "multilingual business meetings",
            ],
        ),
        (
            Vertical::ConferencesWebinars,
            vec![
                "live interpretation",
                "multilingual webinar",
                "conference translation",
                "live captions translation",
                "virtual event interpretation",
                "simultaneous interpretation software",
                "event language accessibility",
                "conference streaming languages",
                "multilingual events",
                "international conference",
            ],
        ),
        (
            Vertical::CustomerSupport,
            vec![
                "multilingual customer support",
                "live translation customer service",
                "language barrier customer service",
                "international customer support",
                "real-time voice translation",
                "multilingual call center",
                "cross-border customer experience",
                "customer service interpretation",
                "international support teams",
            ],
        ),
        (
            Vertical::TourismHospitality,
            vec![
                "multilingual hotel staff",
                "language barrier hospitality",
                "hotel guest communication",
                "tourism translation",
                "real-time translation tourists",
                "international guest support",
                "multilingual tour guides",
                "hotel customer service languages",
                "multilingual hospitality",
                "foreign tourists communication",
            ],
        ),
    ]
}

fn negative_keywords() -> Vec<&'static str> {
    vec![
        "language learning exercises",
        "translation homework",
        "dictionary questions",
        "personal translation requests",
        "school assignments",
        "generic vocabulary questions",
        "unrelated language-learning discussions",
    ]
}

/// The initial community candidates. All are seeded `pending` and disabled: existence and terms are
/// validated by a human, never assumed.
fn community_candidates() -> Vec<(&'static str, Vertical)> {
    vec![
        ("humanresources", Vertical::HrRecruiting),
        ("recruiting", Vertical::HrRecruiting),
        ("AskHR", Vertical::HrRecruiting),
        ("InternationalBusiness", Vertical::InternationalMeetings),
        ("Work", Vertical::InternationalMeetings),
        ("remotework", Vertical::InternationalMeetings),
        ("consulting", Vertical::InternationalMeetings),
        ("ITManagers", Vertical::InternationalMeetings),
        ("sysadmin", Vertical::InternationalMeetings),
        ("CommercialAV", Vertical::ConferencesWebinars),
        ("eventprofs", Vertical::ConferencesWebinars),
        ("Events", Vertical::ConferencesWebinars),
        ("virtualevents", Vertical::ConferencesWebinars),
        ("VIDEOENGINEERING", Vertical::ConferencesWebinars),
        ("customersuccess", Vertical::CustomerSupport),
        ("CustomerService", Vertical::CustomerSupport),
        ("callcentres", Vertical::CustomerSupport),
        ("sales", Vertical::CustomerSupport),
        ("SaaS", Vertical::CustomerSupport),
        ("askhotels", Vertical::TourismHospitality),
        ("hospitality", Vertical::TourismHospitality),
        ("tourism", Vertical::TourismHospitality),
        ("travelagents", Vertical::TourismHospitality),
        ("smallbusiness", Vertical::TourismHospitality),
    ]
}

/// Synthetic, clearly labeled evaluation fixtures. They exercise the metrics path offline and are
/// **not** real conversations; real data must be imported through an authorized source.
fn benchmark_fixtures() -> Vec<(
    &'static str,
    &'static str,
    &'static str,
    bool,
    &'static str,
    Vertical,
)> {
    vec![
        ("bench-001", "en", "We cannot interview candidates who only speak Spanish. Any recommendation for real-time translation in interviews?", true, "explicit_solution_request", Vertical::HrRecruiting),
        ("bench-002", "en", "Our international remote teams struggle with language barrier at work during weekly multilingual meetings.", true, "active_problem", Vertical::InternationalMeetings),
        ("bench-003", "en", "We run a multilingual webinar every month and the live captions translation is always the hardest part to organise.", true, "latent_problem", Vertical::ConferencesWebinars),
        ("bench-004", "en", "Comparing live interpretation options for our conference: simultaneous interpretation software vs hiring interpreters.", true, "product_comparison", Vertical::ConferencesWebinars),
        ("bench-005", "en", "Interesting discussion about how language shapes the way teams think about deadlines.", false, "general_discussion", Vertical::InternationalMeetings),
        ("bench-006", "es", "Nuestro equipo de soporte no puede atender clientes que hablan otro idioma, necesitamos traducción en tiempo real.", true, "active_problem", Vertical::CustomerSupport),
        ("bench-007", "es", "Buscamos recomendaciones para traducir entrevistas con candidatos que no hablan español.", true, "explicit_solution_request", Vertical::HrRecruiting),
        ("bench-008", "pt", "Nos hotéis, a comunicação com hóspedes estrangeiros é um problema todos os dias e precisamos traduzir na recepção.", true, "latent_problem", Vertical::TourismHospitality),
        ("bench-009", "en", "I need help with my translation homework, it is a grammar exercise for my language learning class.", false, "irrelevant", Vertical::Other),
        ("bench-010", "de", "Unser Support-Team braucht eine Übersetzung in Echtzeit für internationale Kunden, die kein Deutsch sprechen.", true, "active_problem", Vertical::CustomerSupport),
        ("bench-011", "fr", "Nous organisons une conférence internationale et cherchons une solution de traduction en direct pour les participants.", true, "active_problem", Vertical::ConferencesWebinars),
        ("bench-012", "en", "What is your favourite language to learn and why?", false, "irrelevant", Vertical::Other),
    ]
}

pub async fn seed_all(config: &AppConfig, db: &Db) -> anyhow::Result<SeedReport> {
    let tenant_id = config.default_tenant_id.clone();
    let mut report = SeedReport {
        tenant_id: tenant_id.clone(),
        ..Default::default()
    };

    tenancy::ensure_tenant(db, &tenant_id, "Lead Radar (local)", "lead-radar").await?;
    tenancy::ensure_user(db, DEV_USER_ID, DEV_EMAIL, DEV_NAME).await?;
    tenancy::ensure_membership(db, &tenant_id, DEV_USER_ID, Role::Owner).await?;

    if let Some(token) = &config.dev_api_token {
        report.dev_token_created = tenancy::ensure_api_token(
            db,
            &tenant_id,
            DEV_USER_ID,
            &crate::auth::hash_token(token),
            "development token",
        )
        .await?;
    }

    // Provider registry rows (names only, no credentials).
    for (name, kind, enabled) in [
        ("apify", "source", config.apify.configured()),
        ("manual_import", "source", true),
        ("jev", "decision", config.jev.configured()),
        ("laya", "decision", config.laya.usable()),
        ("qwen", "generative", config.qwen.configured()),
        (
            "mock",
            "decision",
            config.decision_provider == crate::config::DecisionChoice::Mock,
        ),
    ] {
        let now = now_iso();
        sqlx::query(
            "INSERT INTO source_providers (id, name, kind, enabled, config, created_at, updated_at) \
             VALUES (?, ?, ?, ?, '{}', ?, ?) ON CONFLICT(name) DO NOTHING",
        )
        .bind(new_id())
        .bind(name)
        .bind(kind)
        .bind(if enabled { 1 } else { 0 })
        .bind(&now)
        .bind(&now)
        .execute(&db.writer)
        .await?;
    }

    // Provider configuration defaults without credentials.
    for (provider, kind, enabled, model, base_url) in [
        (
            "jev",
            "decision",
            config.jev.configured(),
            Some(config.jev.model.clone()),
            Some(config.jev.base_url.clone()),
        ),
        (
            "laya",
            "decision",
            config.laya.usable(),
            config.laya.model.clone(),
            Some(config.laya.base_url.clone()),
        ),
        (
            "qwen",
            "generative",
            config.qwen.configured(),
            config.qwen.model.clone(),
            config.qwen.base_url.clone(),
        ),
        (
            "apify",
            "source",
            config.apify.configured(),
            None,
            Some(config.apify.base_url.clone()),
        ),
    ] {
        let now = now_iso();
        sqlx::query(
            "INSERT INTO provider_configs (id, tenant_id, provider, kind, enabled, model, base_url, \
             config, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, '{}', ?) \
             ON CONFLICT(tenant_id, provider) DO NOTHING",
        )
        .bind(new_id())
        .bind(&tenant_id)
        .bind(provider)
        .bind(kind)
        .bind(if enabled { 1 } else { 0 })
        .bind(model)
        .bind(base_url)
        .bind(&now)
        .execute(&db.writer)
        .await?;
    }

    // The manual import source is the one path that must always work.
    ingestion::ensure_manual_source(db, &tenant_id).await?;
    report.manual_source = true;

    // An Apify source placeholder stays disabled until an operator configures and validates it.
    if sources::find_source_by_name(db, &tenant_id, "apify-default")
        .await?
        .is_none()
    {
        let source = sources::create_source(
            db,
            &tenant_id,
            &sources::NewSource {
                name: "apify-default",
                provider: "apify",
                source_type: "community_feed",
                config: &json!({"note": "configure a validated Actor before enabling"}),
            },
        )
        .await?;
        let _ = source;
    }

    // Search profiles and keyword sets.
    for (vertical, keywords) in vertical_keywords() {
        let existing = sources::list_search_profiles(db, &tenant_id).await?;
        if !existing
            .iter()
            .any(|profile| profile.vertical == vertical.as_str())
        {
            sources::create_search_profile(
                db,
                &tenant_id,
                &sources::NewSearchProfile {
                    name: &format!("{} profile", vertical.as_str()),
                    vertical,
                    enabled: true,
                    keywords: &keywords
                        .iter()
                        .map(|term| term.to_string())
                        .collect::<Vec<_>>(),
                    negative_keywords: &[],
                },
            )
            .await?;
        }
        report.profiles += 1;
        report.keywords += keywords.len();
    }
    for term in negative_keywords() {
        sqlx::query(
            "INSERT INTO keywords (id, tenant_id, vertical, term, polarity, created_at) \
             VALUES (?, ?, 'all', ?, 'negative', ?) \
             ON CONFLICT(tenant_id, vertical, term, polarity) DO NOTHING",
        )
        .bind(new_id())
        .bind(&tenant_id)
        .bind(term)
        .bind(now_iso())
        .execute(&db.writer)
        .await?;
        report.keywords += 1;
    }

    // Communities stay pending and disabled.
    for (name, vertical) in community_candidates() {
        let now = now_iso();
        sqlx::query(
            "INSERT INTO communities (id, tenant_id, name, vertical, validation_status, enabled, \
             notes, created_at, updated_at) VALUES (?, ?, ?, ?, 'pending', 0, NULL, ?, ?) \
             ON CONFLICT(tenant_id, name) DO NOTHING",
        )
        .bind(new_id())
        .bind(&tenant_id)
        .bind(name)
        .bind(vertical.as_str())
        .bind(&now)
        .bind(&now)
        .execute(&db.writer)
        .await?;
        report.communities += 1;
    }

    // Default scoring configuration.
    let scoring = ScoringConfig::default_v1();
    scoring
        .validate()
        .map_err(|error| anyhow::anyhow!("invalid default scoring configuration: {error}"))?;
    qualification::save_scoring_version(db, &tenant_id, &scoring, true).await?;
    report.scoring_version = scoring.version;

    // Benchmark dataset (synthetic evaluation fixtures).
    for (reference, language, text, relevant, intent, vertical) in benchmark_fixtures() {
        sqlx::query(
            "INSERT INTO benchmark_items (id, tenant_id, dataset, external_ref, text, language, \
             expected_relevant, expected_intent, expected_vertical, split, created_at) \
             VALUES (?, ?, 'seed', ?, ?, ?, ?, ?, ?, 'evaluation', ?) \
             ON CONFLICT(dataset, external_ref) DO NOTHING",
        )
        .bind(new_id())
        .bind(&tenant_id)
        .bind(reference)
        .bind(text)
        .bind(language)
        .bind(if relevant { 1 } else { 0 })
        .bind(intent)
        .bind(vertical.as_str())
        .bind(now_iso())
        .execute(&db.writer)
        .await?;
        report.benchmark_items += 1;
    }

    // Operational limits and budgets mirror the environment, so the dashboard shows what the guards
    // actually enforce.
    let defaults = [
        (
            "operational",
            json!({
                "worker_concurrency": config.worker.concurrency,
                "ai_concurrency": config.worker.ai_concurrency,
                "job_max_attempts": config.worker.max_attempts,
                "retention_days": config.retention_days,
            }),
        ),
        (
            "budgets",
            json!({
                "apify_total_budget_eur": config.apify.total_budget_eur,
                "apify_daily_budget_eur": config.apify.daily_budget_eur,
                "apify_max_run_cost_eur": config.apify.max_run_cost_eur,
                "apify_max_items_per_run": config.apify.max_items_per_run,
                "apify_max_concurrent_runs": config.apify.max_concurrent_runs,
                "kill_switch": config.apify.kill_switch,
                "currency": "EUR",
            }),
        ),
        (
            "providers",
            json!({
                "decision_provider": config.decision_provider.as_str(),
                "generative_provider": config.generative_provider.as_str(),
                "source_provider": config.source_provider.as_str(),
                "laya_enabled": config.laya.enabled,
            }),
        ),
    ];
    for (key, value) in defaults {
        if tenancy::get_setting(db, &tenant_id, key).await?.is_none() {
            tenancy::set_setting(db, &tenant_id, key, &value).await?;
        }
    }

    // A disabled placeholder Actor is not created: Actor ids must come from the operator.

    tracing::info!(
        tenant = %tenant_id,
        profiles = report.profiles,
        keywords = report.keywords,
        communities = report.communities,
        benchmark_items = report.benchmark_items,
        "seed complete"
    );
    Ok(report)
}

/// Resetting is destructive and must be explicit; the API never calls this.
pub async fn reset_data(db: &Db) -> anyhow::Result<()> {
    for table in [
        "engagements",
        "review_events",
        "comment_versions",
        "generated_comments",
        "lead_scores",
        "leads",
        "decision_answers",
        "decision_runs",
        "ai_analyses",
        "ingestion_items",
        "post_sources",
        "discovered_posts",
        "ingestion_runs",
        "benchmark_results",
        "benchmark_runs",
        "benchmark_items",
        "apify_runs",
        "provider_costs",
        "provider_usage",
        "job_attempts",
        "jobs",
        "audit_logs",
        "keywords",
        "search_profiles",
        "communities",
        "actor_registry",
        "sources",
        "provider_configs",
        "lead_score_versions",
        "app_settings",
        "api_tokens",
        "tenant_users",
        "users",
        "tenants",
    ] {
        sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&db.writer)
            .await?;
    }
    Ok(())
}

/// Validation status used by seeds; keeps the intent explicit at the call site.
pub const SEEDED_VALIDATION_STATUS: ValidationStatus = ValidationStatus::Pending;

/// JSON helper for the seed report.
pub fn report_json(report: &SeedReport) -> String {
    to_json(report)
}

/// Exposed for tests: benchmark fixture count.
pub fn fixture_count() -> usize {
    benchmark_fixtures().len()
}

/// Exposed for tests: community candidate names.
pub fn community_names() -> Vec<&'static str> {
    community_candidates()
        .into_iter()
        .map(|(name, _)| name)
        .collect()
}

/// Exposed for tests: seeded keywords.
pub fn keyword_terms() -> (Vec<&'static str>, Vec<&'static str>) {
    let positive = vertical_keywords()
        .into_iter()
        .flat_map(|(_, keywords)| keywords)
        .collect();
    (positive, negative_keywords())
}
