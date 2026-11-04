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

#[utoipa::path(get, path = "/api/v1/reply-console", tag = "opportunities", responses((status = 200, description = "Compact reply queue")))]
pub async fn reply_console(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<Pagination>,
) -> AppResult<Json<serde_json::Value>> {
    let tenant = &context.tenant_id;
    let limit = query.limit().min(50);
    let rows = sqlx::query_as::<_, qualification::LeadRow>(
        "SELECT l.*, s.name AS source_name, \
         (SELECT COUNT(*) FROM generated_comments c WHERE c.lead_id = l.id) AS comment_count \
         FROM leads l LEFT JOIN sources s ON s.id = l.source_id \
         WHERE l.tenant_id = ? AND l.is_active = 1 \
         AND l.qualification_status IN ('qualified', 'needs_review') \
         ORDER BY CASE l.qualification_status WHEN 'qualified' THEN 0 ELSE 1 END, \
         l.score DESC, l.created_at DESC LIMIT ?",
    )
    .bind(tenant)
    .bind(limit)
    .fetch_all(&state.db.reader)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    let mut ready = 0usize;
    let mut needs_drafts = 0usize;
    for row in &rows {
        let post = crate::modules::ingestion::get_post(&state.db, tenant, &row.post_id).await?;
        let comment_rows = comments::comments_for_lead(&state.db, tenant, &row.id).await?;
        if comment_rows.is_empty() {
            needs_drafts += 1;
        } else {
            ready += 1;
        }
        let mut lead = lead_list_json(
            row,
            qualification::lead_evidence(row).len(),
            row.comment_count,
        );
        lead["content_snippet"] = serde_json::json!(post
            .as_ref()
            .map(|post| post.content.chars().take(700).collect::<String>())
            .unwrap_or_default());
        items.push(json!({
            "lead": lead,
            "suggested_reply": post.as_ref().and_then(|post| post.author.clone()),
            "comments": comment_rows
                .iter()
                .map(|comment| comments::summary(comment.clone()))
                .collect::<Vec<_>>(),
        }));
    }

    Ok(Json(json!({
        "items": items,
        "stats": { "ready": ready, "needs_drafts": needs_drafts, "returned": rows.len() },
    })))
}

/// Normalise a Reddit URL so the extension can find the lead from the page it is on.
fn normalize_url(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    without_query.trim_end_matches('/').to_lowercase()
}

#[derive(Debug, Deserialize)]
pub struct MatchQuery {
    pub url: String,
}

#[utoipa::path(get, path = "/api/v1/reply-console/match", tag = "opportunities", responses((status = 200, description = "Queue item for the current page"), (status = 404, description = "No lead for this URL")))]
pub async fn reply_console_match(
    State(state): State<SharedState>,
    context: TenantContext,
    Query(query): Query<MatchQuery>,
) -> AppResult<Json<serde_json::Value>> {
    let tenant = &context.tenant_id;
    let wanted = normalize_url(&query.url);
    if wanted.is_empty() {
        return Err(AppError::BadRequest("url is required".to_string()));
    }
    let row = sqlx::query_as::<_, qualification::LeadRow>(
        "SELECT l.*, s.name AS source_name, \
         (SELECT COUNT(*) FROM generated_comments c WHERE c.lead_id = l.id) AS comment_count \
         FROM leads l LEFT JOIN sources s ON s.id = l.source_id \
         WHERE l.tenant_id = ? AND l.is_active = 1 AND l.source_url IS NOT NULL \
         ORDER BY l.updated_at DESC LIMIT 200",
    )
    .bind(tenant)
    .fetch_all(&state.db.reader)
    .await?
    .into_iter()
    .find(|lead| {
        lead.source_url
            .as_deref()
            .map(normalize_url)
            .map(|url| url == wanted)
            .unwrap_or(false)
    })
    .ok_or(AppError::NotFound)?;

    let comment_rows = comments::comments_for_lead(&state.db, tenant, &row.id).await?;
    let post = crate::modules::ingestion::get_post(&state.db, tenant, &row.post_id).await?;
    let mut lead = lead_list_json(
        &row,
        qualification::lead_evidence(&row).len(),
        row.comment_count,
    );
    lead["content_snippet"] = json!(post
        .as_ref()
        .map(|post| post.content.chars().take(700).collect::<String>())
        .unwrap_or_default());
    Ok(Json(json!({
        "lead": lead,
        "comments": comment_rows
            .iter()
            .map(|comment| comments::summary(comment.clone()))
            .collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct ReplyBody {
    pub variant: String,
    pub body: Option<String>,
    pub engagement_kind: Option<String>,
    pub note: Option<String>,
}

#[utoipa::path(post, path = "/api/v1/opportunities/{id}/reply", tag = "opportunities", responses((status = 200, description = "Reply recorded as manually posted"), (status = 409, description = "Draft missing or already used")))]
pub async fn record_reply(
    State(state): State<SharedState>,
    context: TenantContext,
    Path(id): Path<String>,
    Json(body): Json<ReplyBody>,
) -> AppResult<Json<serde_json::Value>> {
    let tenant = &context.tenant_id;
    context.require_reviewer()?;
    let lead = qualification::get_lead(&state.db, tenant, &id)
        .await?
        .ok_or(AppError::NotFound)?;
    let variant = crate::domain::enums::CommentVariant::parse(&body.variant).ok_or_else(|| {
        AppError::Validation(vec![crate::error::FieldError::new(
            "variant",
            "must be helpful, soft_pitch or direct_pitch",
        )])
    })?;
    let comment = comments::comments_for_lead(&state.db, tenant, &lead.id)
        .await?
        .into_iter()
        .find(|comment| comment.variant == variant.as_str())
        .ok_or_else(|| {
            AppError::Conflict(
                "draft_missing: generate the comment variants for this lead first".to_string(),
            )
        })?;

    let mut current = comment;
    if let Some(text) = body
        .body
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        if text != current.body {
            current = comments::patch_comment(
                &state.db,
                tenant,
                &current.id,
                Some(text),
                None,
                &context.actor,
            )
            .await
            .map_err(map_comment_error)?;
        }
    }
    if current.status != "approved" {
        current = comments::approve_comment(
            &state.db,
            tenant,
            &current.id,
            &context.actor,
            Some("approved from the reply console"),
        )
        .await
        .map_err(map_comment_error)?;
    }
    let note = body
        .note
        .as_deref()
        .filter(|note| !note.trim().is_empty())
        .unwrap_or("posted manually from the reply console");
    let published = comments::record_publication(
        &state.db,
        tenant,
        &current.id,
        &context.actor,
        None,
        Some(note),
    )
    .await
    .map_err(map_comment_error)?;

    let mut engagement = serde_json::Value::Null;
    if let Some(kind) = body
        .engagement_kind
        .as_deref()
        .filter(|kind| !kind.trim().is_empty())
    {
        let kind = crate::modules::engagement::EngagementKind::parse(kind).ok_or_else(|| {
            AppError::Validation(vec![crate::error::FieldError::new(
                "engagement_kind",
                "must be reply, dm, meeting, interest, lost or other",
            )])
        })?;
        engagement = crate::modules::engagement::create_engagement(
            &state.db,
            tenant,
            &crate::modules::engagement::NewEngagement {
                lead_id: &lead.id,
                comment_id: Some(&published.id),
                kind,
                note: body.note.as_deref(),
                occurred_at: None,
            },
        )
        .await?
        .map(|row| json!(row))
        .unwrap_or(serde_json::Value::Null);
    }
    qualification::set_review_status(&state.db, tenant, &lead.id, "resolved").await?;
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "reply_recorded_as_posted",
        "generated_comment",
        Some(&published.id),
        &json!({"lead_id": lead.id, "variant": variant.as_str(), "engagement": body.engagement_kind}),
    )
    .await?;

    Ok(Json(json!({
        "comment": comments::summary(published),
        "engagement": engagement,
        "lead_id": lead.id,
    })))
}

fn map_comment_error(error: comments::CommentError) -> AppError {
    match error {
        comments::CommentError::NotFound => AppError::NotFound,
        comments::CommentError::InvalidTransition(message)
        | comments::CommentError::InvalidInput(message) => AppError::Conflict(message),
    }
}

#[derive(Debug, Deserialize)]
pub struct PrepareBody {
    pub limit: Option<i64>,
    /// True when the console prepares drafts on page load: bounded to three and rate limited so a
    /// refresh cannot queue paid generation repeatedly.
    #[serde(default)]
    pub auto: bool,
}

/// Minimum spacing between automatic preparations for one tenant.
const AUTO_PREPARE_COOLDOWN_MINUTES: i64 = 10;

#[utoipa::path(post, path = "/api/v1/reply-console/prepare", tag = "opportunities", responses((status = 202, description = "Comment drafts queued"), (status = 503, description = "Generative provider unconfigured")))]
pub async fn prepare_drafts(
    State(state): State<SharedState>,
    context: TenantContext,
    Json(body): Json<PrepareBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let tenant = &context.tenant_id;
    if !state.providers.generative.metadata().configured {
        return Err(AppError::NotReady(
            "generative provider is not configured (QWEN_API_KEY, QWEN_BASE_URL, QWEN_MODEL)"
                .to_string(),
        ));
    }
    let mut limit = body.limit.unwrap_or(5).clamp(1, 10);
    if body.auto {
        limit = limit.min(3);
        let last: Option<String> = sqlx::query_scalar(
            "SELECT created_at FROM audit_logs WHERE tenant_id = ? \
             AND action = 'reply_console_prepared' ORDER BY created_at DESC LIMIT 1",
        )
        .bind(tenant)
        .fetch_optional(&state.db.reader)
        .await?;
        if let Some(last) = last.and_then(|raw| crate::domain::time::parse_iso(&raw)) {
            if (chrono::Utc::now() - last).num_minutes() < AUTO_PREPARE_COOLDOWN_MINUTES {
                return Ok((
                    StatusCode::ACCEPTED,
                    Json(
                        json!({ "queued": 0, "job_ids": [], "cooldown_minutes": AUTO_PREPARE_COOLDOWN_MINUTES }),
                    ),
                ));
            }
        }
    }
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT l.id, l.post_id FROM leads l \
         WHERE l.tenant_id = ? AND l.is_active = 1 \
         AND l.qualification_status IN ('qualified', 'needs_review') \
         AND NOT EXISTS (SELECT 1 FROM generated_comments c WHERE c.lead_id = l.id) \
         ORDER BY CASE l.qualification_status WHEN 'qualified' THEN 0 ELSE 1 END, \
         l.score DESC LIMIT ?",
    )
    .bind(tenant)
    .bind(limit)
    .fetch_all(&state.db.reader)
    .await?;

    let mut job_ids = Vec::new();
    for (lead_id, _) in &rows {
        let (job_id, _) = operations::enqueue(
            &state.db,
            Some(tenant),
            crate::domain::enums::JobKind::CommentGeneration,
            &json!({ "lead_id": lead_id, "actor": context.actor }),
            None,
            state.config.worker.max_attempts,
        )
        .await?;
        job_ids.push(job_id);
    }
    operations::audit(
        &state.db,
        tenant,
        &context.actor,
        "reply_console_prepared",
        "tenant",
        Some(tenant),
        &json!({ "queued": job_ids.len() }),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "queued": job_ids.len(), "job_ids": job_ids })),
    ))
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
