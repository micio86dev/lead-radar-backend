//! Opportunity inbox, detail, analysis triggers and the data-removal path.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use serde_json::json;
use utoipa::IntoParams;

use super::{paged, AppError, AppResult, Pagination};
use crate::auth::TenantContext;
use crate::domain::enums::{BuyingIntent, CustomerType, Intent, ScoreCategory, Vertical};
use crate::domain::time::parse_iso;
use crate::modules::{
    comments, decision, engagement as engagement_module, operations, qualification, tenancy,
};
use crate::state::SharedState;

/// One lead as returned by the list endpoints.
pub fn lead_list_json(
    row: &qualification::LeadRow,
    evidence_count: usize,
    comment_count: i64,
) -> serde_json::Value {
    json!({
        "id": row.id,
        "title": row.title,
        "snippet": row.title.clone().unwrap_or_default(),
        "language": row.language,
        "source_id": row.source_id,
        "source_name": row.source_name,
        "community": row.community,
        "url": row.source_url,
        "published_at": row.published_at,
        "discovered_at": row.discovered_at,
        "vertical_primary": row.vertical_primary,
        "vertical_secondary": qualification::lead_secondary_verticals(row),
        "intent": row.intent,
        "customer_type": row.customer_type,
        "buying_intent": row.buying_intent,
        "business_potential": row.business_potential,
        "product_fit": row.product_fit,
        "score": row.score,
        "score_category": row.score_category,
        "qualification_status": row.qualification_status,
        "review_status": row.review_status,
        "confidence": row.confidence,
        "evidence_count": evidence_count,
        "comment_count": comment_count,
        "created_at": row.created_at,
    })
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct OpportunityQuery {
    #[serde(default)]
    pub page: Option<i64>,
    #[serde(default)]
    pub per_page: Option<i64>,
    #[serde(default)]
    pub vertical: Option<String>,
    #[serde(default)]
    pub score_category: Option<String>,
    #[serde(default)]
    pub score_min: Option<i64>,
    #[serde(default)]
    pub score_max: Option<i64>,
    #[serde(default)]
    pub intent: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub community: Option<String>,
    #[serde(default)]
    pub published_after: Option<String>,
    #[serde(default)]
    pub published_before: Option<String>,
    #[serde(default)]
    pub review_status: Option<String>,
    #[serde(default)]
    pub customer_type: Option<String>,
    #[serde(default)]
    pub buying_intent: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub order: Option<String>,
}

fn validate_enum<T: std::str::FromStr>(value: &Option<String>, field: &str) -> AppResult<()> {
    if let Some(raw) = value {
        if raw.parse::<T>().is_err() {
            return Err(AppError::BadRequest(format!(
                "{field} has an unknown value {raw:?}"
            )));
        }
    }
    Ok(())
}

pub fn parse_filter(query: &OpportunityQuery) -> AppResult<qualification::LeadFilter> {
    validate_enum::<Vertical>(&query.vertical, "vertical")?;
    validate_enum::<ScoreCategory>(&query.score_category, "score_category")?;
    validate_enum::<Intent>(&query.intent, "intent")?;
    validate_enum::<CustomerType>(&query.customer_type, "customer_type")?;
    validate_enum::<BuyingIntent>(&query.buying_intent, "buying_intent")?;
    if let Some(sort) = &query.sort {
        if !["score", "recency", "buying_intent", "confidence"].contains(&sort.as_str()) {
            return Err(AppError::BadRequest(format!(
                "sort has an unknown value {sort:?}"
            )));
        }
    }
    if let Some(order) = &query.order {
        if !["asc", "desc"].contains(&order.as_str()) {
            return Err(AppError::BadRequest(format!(
                "order has an unknown value {order:?}"
            )));
        }
    }
    for (value, field) in [
        (&query.published_after, "published_after"),
        (&query.published_before, "published_before"),
    ] {
        if let Some(raw) = value {
            if parse_iso(raw).is_none() {
                return Err(AppError::BadRequest(format!(
                    "{field} must be an RFC 3339 timestamp"
                )));
            }
        }
    }
    for (value, field) in [
        (query.score_min, "score_min"),
        (query.score_max, "score_max"),
    ] {
        if let Some(score) = value {
            if !(0..=100).contains(&score) {
                return Err(AppError::BadRequest(format!(
                    "{field} must be within 0..100"
                )));
            }
        }
    }

    Ok(qualification::LeadFilter {
        vertical: query.vertical.clone(),
        score_category: query.score_category.clone(),
        score_min: query.score_min,
        score_max: query.score_max,
        intent: query.intent.clone(),
        language: query.language.clone(),
        source_id: query.source_id.clone(),
        community: query.community.clone(),
        published_after: query.published_after.clone(),
        published_before: query.published_before.clone(),
        review_status: query.review_status.clone(),
        customer_type: query.customer_type.clone(),
        buying_intent: query.buying_intent.clone(),
        query: query.q.clone(),
        sort: query.sort.clone(),
        order: query.order.clone(),
    })
}

#[utoipa::path(get, path = "/api/v1/opportunities", tag = "opportunities", responses((status = 200, description = "Paginated opportunity list")))]
pub async fn list(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<OpportunityQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let pagination = Pagination {
        page: query.page,
        per_page: query.per_page,
    };
    let filter = parse_filter(&query)?;
    let (rows, total) = qualification::list_leads(
        &state.db,
        &context.tenant_id,
        &filter,
        pagination.limit(),
        pagination.offset(),
    )
    .await?;
    let items: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            lead_list_json(
                row,
                qualification::lead_evidence(row).len(),
                row.comment_count,
            )
        })
        .collect();
    Ok(Json(paged(json!(items), &pagination, total)))
}

#[utoipa::path(get, path = "/api/v1/opportunities/{id}", tag = "opportunities", responses((status = 200, description = "Opportunity detail"), (status = 404, description = "Unknown opportunity")))]
pub async fn detail(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<Json<serde_json::Value>> {
    let tenant = &context.tenant_id;
    let lead = qualification::get_lead(&state.db, tenant, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let post = crate::modules::ingestion::get_post(&state.db, tenant, &lead.post_id)
        .await?
        .ok_or(AppError::NotFound)?;
    let evidence = qualification::lead_evidence(&lead);
    let history = qualification::score_history(&state.db, &lead.id, 1).await?;
    let latest_score = history.first();
    let decision = decision::latest_decision(&state.db, tenant, &lead.post_id).await?;
    let analyses =
        crate::modules::generative::analyses_for_post(&state.db, tenant, &lead.post_id).await?;
    let comment_rows = comments::comments_for_lead(&state.db, tenant, &lead.id).await?;
    let engagements = engagement_module::engagements_for_lead(&state.db, tenant, &lead.id).await?;

    let mut detail = lead_list_json(&lead, evidence.len(), comment_rows.len() as i64);
    detail["content"] = json!(post.content);
    detail["author"] = json!(post.author);
    detail["engagement"] = match &post.engagement {
        Some(engagement) => json!({
            "comments": engagement.comments,
            "upvotes": engagement.upvotes,
            "shares": engagement.shares,
        }),
        None => serde_json::Value::Null,
    };
    detail["evidence"] = json!(evidence);
    detail["score_breakdown"] = match latest_score {
        Some(score) => json!({
            "version": score.score_version,
            "score": score.score,
            "category": score.category,
            "components": serde_json::from_str::<serde_json::Value>(&score.components)
                .unwrap_or(serde_json::Value::Null),
            "inputs": serde_json::from_str::<serde_json::Value>(&score.inputs)
                .unwrap_or(serde_json::Value::Null),
            "created_at": score.created_at,
        }),
        None => serde_json::Value::Null,
    };
    detail["decision"] = match decision {
        Some(latest) => {
            let answers: Vec<serde_json::Value> =
                latest.answers.iter().map(decision::answer_to_api).collect();
            let confidences: Vec<f64> = latest
                .answers
                .iter()
                .filter_map(|answer| {
                    answer.confidence.or_else(|| {
                        answer
                            .noul
                            .map(crate::domain::decision::DecisionAnswer::probability_confidence)
                    })
                })
                .collect();
            json!({
                "provider": latest.run.provider,
                "model": latest.run.model,
                "model_version": latest.run.model_version,
                "provider_request_id": latest.run.provider_request_id,
                "confidence": if confidences.is_empty() {
                    serde_json::Value::Null
                } else {
                    json!(confidences.iter().sum::<f64>() / confidences.len() as f64)
                },
                "latency_ms": latest.run.latency_ms,
                "input_tokens": latest.run.input_tokens,
                "output_tokens": latest.run.output_tokens,
                "created_at": latest.run.created_at,
                "answers": answers,
            })
        }
        None => serde_json::Value::Null,
    };
    detail["analyses"] = json!(analyses);
    detail["comments"] = json!(comment_rows
        .iter()
        .map(|row| comments::summary(row.clone()))
        .collect::<Vec<_>>());
    detail["engagements"] = json!(engagements);
    Ok(Json(detail))
}

#[utoipa::path(post, path = "/api/v1/opportunities/{id}/analyze", tag = "opportunities", responses((status = 202, description = "Analysis queued"), (status = 404, description = "Unknown opportunity")))]
pub async fn analyze(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let tenant = &context.tenant_id;
    let lead = qualification::get_lead(&state.db, tenant, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let (job_id, _) = operations::enqueue(
        &state.db,
        Some(tenant),
        crate::domain::enums::JobKind::DecisionEvaluation,
        &json!({"post_id": lead.post_id, "trigger": "manual"}),
        None,
        state.config.worker.max_attempts,
    )
    .await?;
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "analysis_requested",
        "lead",
        Some(&id),
        &json!({"job_id": job_id}),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"job_id": job_id, "lead_id": id})),
    ))
}

#[utoipa::path(post, path = "/api/v1/opportunities/{id}/generate-comments", tag = "opportunities", responses((status = 202, description = "Comment generation queued"), (status = 503, description = "Generative provider not configured")))]
pub async fn generate_comments(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let tenant = &context.tenant_id;
    let lead = qualification::get_lead(&state.db, tenant, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    if !state.providers.generative.metadata().configured {
        return Err(AppError::NotReady(
            "generative provider is not configured (QWEN_API_KEY, QWEN_BASE_URL, QWEN_MODEL)"
                .to_string(),
        ));
    }
    let (job_id, _) = operations::enqueue(
        &state.db,
        Some(tenant),
        crate::domain::enums::JobKind::CommentGeneration,
        &json!({"lead_id": lead.id, "actor": context.actor}),
        None,
        state.config.worker.max_attempts,
    )
    .await?;
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "comment_generation_requested",
        "lead",
        Some(&id),
        &json!({"job_id": job_id}),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({"job_id": job_id}))))
}

#[utoipa::path(delete, path = "/api/v1/opportunities/{id}", tag = "opportunities", responses((status = 204, description = "Content removed"), (status = 404, description = "Unknown opportunity")))]
pub async fn remove(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
) -> AppResult<StatusCode> {
    let tenant = &context.tenant_id;
    let lead = qualification::get_lead(&state.db, tenant, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let retention = tenancy::get_setting(&state.db, tenant, "retention_days")
        .await?
        .and_then(|value| value.as_i64())
        .unwrap_or(state.config.retention_days);
    let now = crate::domain::time::now_iso();
    let mut tx = state.db.writer.begin().await?;
    sqlx::query(
        "UPDATE discovered_posts SET content = '[removed]', title = NULL, author = NULL, \
         source_url = NULL, raw = '{}', deleted_at = ?, updated_at = ? WHERE tenant_id = ? AND id = ?",
    )
    .bind(&now)
    .bind(&now)
    .bind(tenant)
    .bind(&lead.post_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE leads SET is_active = 0, updated_at = ? WHERE tenant_id = ? AND id = ?")
        .bind(&now)
        .bind(tenant)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "DELETE FROM generated_comments WHERE tenant_id = ? AND lead_id = ? \
         AND status IN ('draft', 'pending_review', 'needs_edit')",
    )
    .bind(tenant)
    .bind(&id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "content_removed",
        "lead",
        Some(&id),
        &json!({"post_id": lead.post_id, "retention_days": retention}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
