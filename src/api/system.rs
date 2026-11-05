//! Health, readiness, dashboard and tenant settings.

use axum::extract::State;
use axum::Json;
use utoipa::OpenApi;

use super::providers::provider_statuses;
use crate::api::{ApiDoc, AppError, AppResult};
use crate::auth::TenantContext;
use crate::domain::time::{now, to_iso};
use crate::modules::{comments, decision, operations, qualification, sources, tenancy};
use crate::state::SharedState;
use crate::VERSION;

#[utoipa::path(get, path = "/health", tag = "system", responses((status = 200, description = "Process is alive")))]
pub async fn health(State(state): State<SharedState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": VERSION,
        "uptime_seconds": state.uptime_seconds(),
    }))
}

#[utoipa::path(get, path = "/ready", tag = "system", responses((status = 200, description = "Database reachable"), (status = 503, description = "Not ready")))]
pub async fn ready(State(state): State<SharedState>) -> Result<Json<serde_json::Value>, AppError> {
    if state.db.ping().await {
        Ok(Json(
            serde_json::json!({"status": "ready", "database": "ok"}),
        ))
    } else {
        Err(AppError::NotReady("database is not reachable".to_string()))
    }
}

#[utoipa::path(get, path = "/api/v1/openapi.json", tag = "system", responses((status = 200, description = "OpenAPI document")))]
pub async fn openapi() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

#[utoipa::path(get, path = "/api/v1/dashboard", tag = "system", responses((status = 200, description = "Dashboard aggregates")))]
pub async fn dashboard(
    State(state): State<SharedState>,
    context: TenantContext,
) -> AppResult<Json<serde_json::Value>> {
    let tenant = &context.tenant_id;
    let since = to_iso(now() - chrono::Duration::hours(24));

    let totals = qualification::lead_totals(&state.db, tenant, &since).await?;
    let by_vertical = qualification::lead_counts_by_vertical(&state.db, tenant).await?;
    let by_intent = qualification::lead_counts_by_intent(&state.db, tenant).await?;

    let (recent, _) = qualification::list_leads(
        &state.db,
        tenant,
        &qualification::LeadFilter {
            vertical: None,
            score_category: None,
            score_min: None,
            score_max: None,
            intent: None,
            language: None,
            source_id: None,
            community: None,
            published_after: None,
            published_before: None,
            review_status: None,
            customer_type: None,
            buying_intent: None,
            query: None,
            sort: Some("recency".to_string()),
            order: Some("desc".to_string()),
        },
        5,
        0,
    )
    .await?;

    let decision_stats = decision::decision_stats(&state.db, tenant).await?;
    let (pending_review, needs_edit, approved) =
        comments::review_queue_counts(&state.db, tenant).await?;
    let (failed_jobs, dead_jobs) = operations::job_failure_counts(&state.db, tenant).await?;
    let usage = sources::apify_usage(&state.db, tenant).await?;
    // Same source of truth as the guard, so the dashboard cannot show a ceiling the guard ignores.
    let budgets = sources::effective_budgets(&state.db, tenant, &state.config).await;
    let statuses = provider_statuses(&state, tenant).await?;
    let costs =
        operations::provider_cost_summary(&state.db, tenant, operations::days_ago(now(), 30))
            .await?;

    let recent_json: Vec<serde_json::Value> = recent
        .iter()
        .map(|row| {
            super::opportunities::lead_list_json(
                row,
                qualification::lead_evidence(row).len(),
                row.comment_count,
            )
        })
        .collect();

    Ok(Json(serde_json::json!({
        "totals": {
            "opportunities": totals.opportunities,
            "new_opportunities": totals.new_opportunities,
            "hot": totals.hot,
            "warm": totals.warm,
            "potential": totals.potential,
            "low": totals.low,
        },
        "by_vertical": by_vertical
            .iter()
            .map(|(vertical, count)| serde_json::json!({"vertical": vertical, "count": count}))
            .collect::<Vec<_>>(),
        "by_intent": by_intent
            .iter()
            .map(|(intent, count)| serde_json::json!({"intent": intent, "count": count}))
            .collect::<Vec<_>>(),
        "recent_opportunities": recent_json,
        "ai_decisions": {
            "total": decision_stats.iter().map(|(_, count)| *count).sum::<i64>(),
            "providers": decision_stats
                .iter()
                .map(|(provider, count)| serde_json::json!({"provider": provider, "count": count}))
                .collect::<Vec<_>>(),
        },
        "review_queue": {
            "pending_review": pending_review,
            "needs_edit": needs_edit,
            "approved": approved,
        },
        "providers": statuses,
        "apify": {
            "budget_eur": budgets.total_budget_eur,
            "daily_budget_eur": budgets.daily_budget_eur,
            "known_actual_usd": usage.actual_spend_usd,
            "estimated_usd": usage.estimate_spend_usd,
            "remaining_known_eur": if usage.actual_spend_usd.is_some() {
                serde_json::Value::Null
            } else {
                serde_json::json!(budgets.total_budget_eur)
            },
            "runs": usage.runs_total,
            "results": usage.results_total,
            "errors": usage.errors_total,
            "kill_switch": budgets.kill_switch,
            "cost_source": if usage.actual_spend_usd.is_some() { "actual" }
                else if usage.estimate_spend_usd.is_some() { "estimate" } else { "unknown" },
        },
        "job_failures": { "failed": failed_jobs, "dead_letter": dead_jobs },
        "costs": costs,
    })))
}

#[utoipa::path(get, path = "/api/v1/settings", tag = "system", responses((status = 200, description = "Tenant settings")))]
pub async fn get_settings(
    State(state): State<SharedState>,
    context: TenantContext,
) -> AppResult<Json<serde_json::Value>> {
    let tenant = &context.tenant_id;
    let name = tenancy::tenant_name(&state.db, tenant)
        .await?
        .unwrap_or_else(|| "unknown tenant".to_string());
    let settings = tenancy::all_settings(&state.db, tenant).await?;
    let operational = settings
        .get("operational")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let budgets = settings
        .get("budgets")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    Ok(Json(serde_json::json!({
        "tenant": { "id": tenant, "name": name },
        "operational": operational,
        "budgets": budgets,
        "providers": provider_statuses(&state, tenant).await?,
    })))
}

#[derive(Debug, serde::Deserialize)]
pub struct SettingsPatch {
    pub retention_days: Option<i64>,
    pub budgets: Option<serde_json::Value>,
    pub providers: Option<serde_json::Value>,
}

#[utoipa::path(patch, path = "/api/v1/settings", tag = "system", responses((status = 200, description = "Updated settings"), (status = 422, description = "Invalid values")))]
pub async fn patch_settings(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(patch): Json<SettingsPatch>,
) -> AppResult<Json<serde_json::Value>> {
    let tenant = &context.tenant_id;
    let audit_details = serde_json::json!({
        "retention_days": patch.retention_days,
        "budgets": patch.budgets,
        "providers": patch.providers,
    });
    if let Some(days) = patch.retention_days {
        if !(1..=3650).contains(&days) {
            return Err(AppError::BadRequest(
                "retention_days must be between 1 and 3650".to_string(),
            ));
        }
        tenancy::set_setting(
            &state.db,
            tenant,
            "retention_days",
            &serde_json::json!(days),
        )
        .await?;
    }
    if let Some(budgets) = patch.budgets {
        if !budgets.is_object() {
            return Err(AppError::BadRequest(
                "budgets must be an object".to_string(),
            ));
        }
        tenancy::set_setting(&state.db, tenant, "budgets", &budgets).await?;
    }
    if let Some(providers) = patch.providers {
        if !providers.is_object() {
            return Err(AppError::BadRequest(
                "providers must be an object".to_string(),
            ));
        }
        tenancy::set_setting(&state.db, tenant, "providers", &providers).await?;
    }
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "settings_updated",
        "tenant",
        Some(tenant),
        &audit_details,
    )
    .await?;

    get_settings(State(state), context).await
}
