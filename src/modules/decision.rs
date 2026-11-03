//! Structured decision evaluation: persist intent, call the provider outside any transaction,
//! persist the outcome, then score and qualify deterministically.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::decision::{lead_questions, DecisionResult, QuestionKind};
use crate::domain::enums::{
    BusinessPotential, BuyingIntent, CustomerType, Intent, ProductFit, Vertical,
};
use crate::domain::evidence::{self, Evidence};
use crate::domain::qualification::{qualify, QualificationOutcome};
use crate::domain::scoring::{score, EngagementCounts, ScoreInputs};
use crate::domain::time::parse_iso;
use crate::domain::time::{now, now_iso};
use crate::modules::ingestion::Post;
use crate::modules::{new_id, operations, qualification, sources, to_json};
use crate::providers::{DecisionRequest, ProviderError};
use crate::state::AppState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    Auto,
    Manual,
}

impl Trigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    /// Automatic runs are idempotent per post content; a manual re-analysis is a new run.
    pub fn idempotency_key(&self, post_id: &str, content_hash: &str) -> Option<String> {
        match self {
            Self::Auto => Some(format!("decision:{post_id}:{content_hash}")),
            Self::Manual => None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EvaluationError {
    #[error("post not found")]
    NotFound,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("{0}")]
    Internal(String),
}

impl EvaluationError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Provider(error) => error.is_retryable(),
            Self::Database(_) | Self::Internal(_) => true,
            Self::NotFound => false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct EvaluationOutcome {
    pub lead_id: String,
    pub decision_run_id: String,
    pub provider: String,
    pub score: i64,
    pub score_category: String,
    pub qualification_status: String,
    pub confidence: Option<f64>,
    pub evidence_count: usize,
}

/// Classification features extracted from a decision result. Missing answers fall back to the
/// conservative value (no signal), never to an invented one.
#[derive(Debug, Clone, PartialEq)]
pub struct Features {
    pub relevance: f64,
    pub vertical: Vertical,
    pub vertical_secondary: Vec<String>,
    pub intent: Intent,
    pub customer_type: CustomerType,
    pub buying_intent: BuyingIntent,
    pub business_potential: BusinessPotential,
    pub product_fit: ProductFit,
    pub needs_human_review: bool,
    pub evidence_sufficient: bool,
    pub ambiguous: bool,
    pub language: String,
    pub confidence: Option<f64>,
}

fn noul_or(result: &DecisionResult, question: &str, default: f64) -> f64 {
    result.noul(question).unwrap_or(default)
}

fn label_or<'a>(result: &'a DecisionResult, question: &str, default: &'a str) -> &'a str {
    result.choice(question).unwrap_or(default)
}

pub fn features_from(result: &DecisionResult, post: &Post) -> Features {
    let noise = noul_or(result, "irrelevant_noise", 0.0);
    let relevance = if noise > 0.6 {
        noul_or(result, "relevance", 0.0).min(0.2)
    } else {
        noul_or(result, "relevance", 0.0)
    };

    let mut vertical =
        Vertical::parse(label_or(result, "vertical", "other")).unwrap_or(Vertical::Other);
    let mut intent =
        Intent::parse(label_or(result, "intent", "irrelevant")).unwrap_or(Intent::Irrelevant);
    if noise > 0.6 {
        intent = Intent::Irrelevant;
        vertical = Vertical::Other;
    }

    let customer_type = CustomerType::parse(label_or(result, "customer_type", "unknown"))
        .unwrap_or(CustomerType::Unknown);
    let buying_intent = BuyingIntent::parse(label_or(result, "buying_intent", "none"))
        .unwrap_or(BuyingIntent::None);
    let business_potential =
        BusinessPotential::parse(label_or(result, "business_potential", "low"))
            .unwrap_or(BusinessPotential::Low);
    let product_fit =
        ProductFit::parse(label_or(result, "product_fit", "poor")).unwrap_or(ProductFit::Poor);

    let mut vertical_secondary = Vec::new();
    if let Some(answer) = result.answer("vertical") {
        if let Some(probabilities) = &answer.probabilities {
            for (label, probability) in probabilities {
                if *probability >= 0.15 && label != label_or(result, "vertical", "other") {
                    if let Some(parsed) = Vertical::parse(label) {
                        if parsed != Vertical::Other {
                            vertical_secondary.push(parsed.as_str().to_string());
                        }
                    }
                }
            }
        }
    }

    let language = result
        .choice("language")
        .map(|value| value.to_string())
        .filter(|value| value != "other")
        .unwrap_or_else(|| {
            post.language
                .clone()
                .unwrap_or_else(|| crate::domain::language::detect(&post.content))
        });

    Features {
        relevance,
        vertical,
        vertical_secondary,
        intent,
        customer_type,
        buying_intent,
        business_potential,
        product_fit,
        needs_human_review: noul_or(result, "needs_human_review", 0.0) > 0.5,
        evidence_sufficient: noul_or(result, "evidence_sufficient", 0.0) > 0.5,
        ambiguous: noul_or(result, "too_ambiguous", 0.0) > 0.5
            || noul_or(result, "needs_human_review", 0.0) > 0.5,
        language,
        confidence: result.mean_confidence(),
    }
}

fn state_text(post: &Post) -> String {
    let mut state = String::new();
    if let Some(title) = &post.title {
        state.push_str(&format!("Title: {title}\n"));
    }
    if let Some(community) = &post.community {
        state.push_str(&format!("Community: {community}\n"));
    }
    if let Some(language) = &post.language {
        state.push_str(&format!("Source language hint: {language}\n"));
    }
    state.push_str("Content:\n");
    state.push_str(&post.content);
    state
}

fn evidence_for(post: &Post, keywords: &crate::domain::filtering::KeywordSet) -> Vec<Evidence> {
    let outcome = crate::domain::filtering::evaluate(&post.content, keywords);
    let terms = if outcome.matched_positive.is_empty() {
        keywords
            .positive
            .values()
            .flatten()
            .take(30)
            .cloned()
            .collect::<Vec<String>>()
    } else {
        outcome.matched_positive
    };
    evidence::extract(&post.content, &terms)
}

/// Evaluate one post end to end: decision, persistence, deterministic score, qualification.
pub async fn evaluate_post(
    state: &AppState,
    tenant_id: &str,
    post_id: &str,
    trigger: Trigger,
) -> Result<EvaluationOutcome, EvaluationError> {
    let post = crate::modules::ingestion::get_post(&state.db, tenant_id, post_id)
        .await?
        .filter(|post| post.deleted_at.is_none())
        .ok_or(EvaluationError::NotFound)?;

    if let Some(false) = crate::modules::tenancy::provider_toggle(
        &state.db,
        tenant_id,
        &state.providers.decision.metadata().provider,
    )
    .await?
    {
        return Err(EvaluationError::Internal(
            "decision provider disabled by tenant settings".to_string(),
        ));
    }
    let config = qualification::active_scoring_config(&state.db, tenant_id).await?;
    let provider_metadata = state.providers.decision.metadata();
    let run_id = new_id();
    let started_at = now_iso();
    let excerpt: String = post.content.chars().take(500).collect();
    let provider_label = provider_metadata.provider.clone();
    let model_label = provider_metadata
        .model
        .clone()
        .unwrap_or_else(|| "unspecified".to_string());

    sqlx::query(
        "INSERT INTO decision_runs (id, tenant_id, post_id, provider, model, state_excerpt, status, \
         created_at) VALUES (?, ?, ?, ?, ?, ?, 'running', ?)",
    )
    .bind(&run_id)
    .bind(tenant_id)
    .bind(post_id)
    .bind(&provider_label)
    .bind(&model_label)
    .bind(&excerpt)
    .bind(&started_at)
    .execute(&state.db.writer)
    .await?;

    let request = DecisionRequest {
        state: state_text(&post),
        questions: lead_questions(),
        idempotency_key: Some(run_id.clone()),
    };
    let result = match state.providers.decision.evaluate(&request).await {
        Ok(result) => result,
        Err(error) => {
            let message = error.to_string();
            sqlx::query(
                "UPDATE decision_runs SET status = 'failed', error = ?, completed_at = ? WHERE id = ?",
            )
            .bind(&message)
            .bind(now_iso())
            .bind(&run_id)
            .execute(&state.db.writer)
            .await?;
            operations::record_usage(
                &state.db,
                Some(tenant_id),
                &operations::UsageRecord {
                    provider: &provider_label,
                    operation: "decision",
                    request_id: None,
                    input_tokens: None,
                    output_tokens: None,
                    latency_ms: None,
                    status: "error",
                    error: Some(&message),
                },
            )
            .await?;
            operations::audit(
                &state.db,
                tenant_id,
                "system:worker",
                "decision_failed",
                "discovered_post",
                Some(post_id),
                &json!({"trigger": trigger.as_str(), "provider": provider_label, "error": message}),
            )
            .await?;
            return Err(EvaluationError::Provider(error));
        }
    };

    persist_answers(&state.db, &run_id, &result).await?;
    sqlx::query(
        "UPDATE decision_runs SET status = 'succeeded', model = ?, model_version = ?, \
         provider_request_id = ?, latency_ms = ?, input_tokens = ?, output_tokens = ?, \
         charged_tokens = ?, completed_at = ? WHERE id = ?",
    )
    .bind(&result.model)
    .bind(&result.model_version)
    .bind(&result.provider_request_id)
    .bind(result.latency_ms)
    .bind(result.usage.input_tokens)
    .bind(result.usage.output_tokens)
    .bind(result.usage.charged_tokens)
    .bind(now_iso())
    .bind(&run_id)
    .execute(&state.db.writer)
    .await?;

    operations::record_usage(
        &state.db,
        Some(tenant_id),
        &operations::UsageRecord {
            provider: &result.provider,
            operation: "decision",
            request_id: result.provider_request_id.as_deref(),
            input_tokens: result.usage.input_tokens,
            output_tokens: result.usage.output_tokens,
            latency_ms: Some(result.latency_ms),
            status: "succeeded",
            error: None,
        },
    )
    .await?;
    // Providers bill in their own currency and do not report a unit price here: the cost is unknown
    // and is recorded as such rather than guessed.
    operations::record_cost(
        &state.db,
        Some(tenant_id),
        &result.provider,
        operations::CostKind::Actual,
        0.0,
        "USD",
        "decision",
        Some(&run_id),
        true,
    )
    .await?;

    let features = features_from(&result, &post);
    let keywords = sources::keyword_set(&state.db, tenant_id).await;
    let evidence = evidence_for(&post, &keywords);

    let inputs = ScoreInputs {
        relevance_probability: features.relevance,
        buying_intent: features.buying_intent,
        business_potential: features.business_potential,
        product_fit: features.product_fit,
        published_at: post.published_at.clone(),
        engagement: post.engagement.as_ref().map(|engagement| EngagementCounts {
            comments: engagement.comments,
            upvotes: engagement.upvotes,
            shares: engagement.shares,
        }),
    };
    let scored = score(&config, &inputs, now());

    let outcome = qualify(&crate::domain::qualification::QualificationInput {
        relevance: features.relevance,
        intent: features.intent,
        customer_type: features.customer_type,
        evidence_sufficient: features.evidence_sufficient && !evidence.is_empty(),
        ambiguous: features.ambiguous,
        confidence: features.confidence.unwrap_or(0.0),
        vertical: features.vertical,
        score: scored.score,
        thresholds: config.thresholds.clone(),
    });

    let lead_id = qualification::upsert_lead(
        &state.db,
        &qualification::LeadUpsert {
            tenant_id,
            post_id,
            title: post.title.as_deref(),
            language: Some(&features.language),
            source_id: post.source_id.as_deref(),
            community: post.community.as_deref(),
            source_url: post.source_url.as_deref(),
            author: post.author.as_deref(),
            published_at: post.published_at.as_deref(),
            discovered_at: &post.discovered_at,
            vertical_primary: features.vertical.as_str(),
            vertical_secondary: &features.vertical_secondary,
            intent: features.intent.as_str(),
            customer_type: features.customer_type.as_str(),
            buying_intent: features.buying_intent.as_str(),
            business_potential: features.business_potential.as_str(),
            product_fit: features.product_fit.as_str(),
            confidence: features.confidence,
            evidence: &evidence,
            score: scored.score,
            score_category: scored.category.as_str(),
            qualification_status: outcome.status().as_str(),
        },
    )
    .await?;

    let inputs_json = json!({
        "relevance_probability": inputs.relevance_probability,
        "buying_intent": inputs.buying_intent.as_str(),
        "business_potential": inputs.business_potential.as_str(),
        "product_fit": inputs.product_fit.as_str(),
        "published_at": inputs.published_at,
        "engagement": inputs.engagement.map(|engagement| json!({
            "comments": engagement.comments,
            "upvotes": engagement.upvotes,
            "shares": engagement.shares,
        })),
        "missing": scored.missing,
    });
    qualification::insert_score_history(
        &state.db,
        &qualification::ScoreHistoryInsert {
            lead_id: &lead_id,
            score_version: config.version,
            score: scored.score,
            category: scored.category.as_str(),
            components: &to_json_value(&scored.components),
            inputs: &inputs_json,
            decision_run_id: Some(&run_id),
        },
    )
    .await?;

    operations::audit(
        &state.db,
        tenant_id,
        "system:worker",
        "lead_scored",
        "lead",
        Some(&lead_id),
        &json!({
            "trigger": trigger.as_str(),
            "score": scored.score,
            "category": scored.category.as_str(),
            "score_version": config.version,
            "qualification": outcome.status().as_str(),
            "reasons": match &outcome {
                QualificationOutcome::NeedsReview { reasons } | QualificationOutcome::Rejected { reasons } => reasons.clone(),
                QualificationOutcome::Qualified => Vec::new(),
            },
        }),
    )
    .await?;

    Ok(EvaluationOutcome {
        lead_id,
        decision_run_id: run_id,
        provider: result.provider,
        score: scored.score,
        score_category: scored.category.as_str().to_string(),
        qualification_status: outcome.status().as_str().to_string(),
        confidence: features.confidence,
        evidence_count: evidence.len(),
    })
}

fn to_json_value<T: Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).unwrap_or(serde_json::Value::Null)
}

async fn persist_answers(
    db: &Db,
    run_id: &str,
    result: &DecisionResult,
) -> Result<(), sqlx::Error> {
    for answer in &result.answers {
        sqlx::query(
            "INSERT INTO decision_answers (id, decision_run_id, question, answer_type, noul, choice, \
             score, legend, probabilities, confidence, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(new_id())
        .bind(run_id)
        .bind(&answer.question)
        .bind(answer.kind.as_str())
        .bind(answer.noul)
        .bind(answer.choice.as_deref())
        .bind(answer.score)
        .bind(answer.legend.as_ref().map(to_json))
        .bind(answer.probabilities.as_ref().map(to_json))
        .bind(answer.confidence)
        .bind(now_iso())
        .execute(&db.writer)
        .await?;
    }
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct DecisionRunRow {
    pub id: String,
    pub post_id: String,
    pub provider: String,
    pub model: String,
    pub model_version: Option<String>,
    pub provider_request_id: Option<String>,
    pub status: String,
    pub latency_ms: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub error: Option<String>,
    pub created_at: String,
    pub completed_at: Option<String>,
}

#[derive(Debug, Clone, FromRow)]
pub struct DecisionAnswerRow {
    pub question: String,
    pub answer_type: String,
    pub noul: Option<f64>,
    pub choice: Option<String>,
    pub score: Option<f64>,
    pub legend: Option<String>,
    pub probabilities: Option<String>,
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct LatestDecision {
    pub run: DecisionRunRow,
    pub answers: Vec<DecisionAnswerRow>,
}

pub async fn latest_decision(
    db: &Db,
    tenant_id: &str,
    post_id: &str,
) -> Result<Option<LatestDecision>, sqlx::Error> {
    let run = sqlx::query_as::<_, DecisionRunRow>(
        "SELECT id, post_id, provider, model, model_version, provider_request_id, status, \
         latency_ms, input_tokens, output_tokens, error, created_at, completed_at \
         FROM decision_runs WHERE tenant_id = ? AND post_id = ? AND status = 'succeeded' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(tenant_id)
    .bind(post_id)
    .fetch_optional(&db.reader)
    .await?;
    let Some(run) = run else { return Ok(None) };
    let answers = sqlx::query_as::<_, DecisionAnswerRow>(
        "SELECT question, answer_type, noul, choice, score, legend, probabilities, confidence \
         FROM decision_answers WHERE decision_run_id = ? ORDER BY created_at, question",
    )
    .bind(&run.id)
    .fetch_all(&db.reader)
    .await?;
    Ok(Some(LatestDecision { run, answers }))
}

#[derive(Debug, Clone, FromRow)]
pub struct DecisionSummaryRow {
    pub id: String,
    pub post_id: String,
    pub lead_id: Option<String>,
    pub provider: String,
    pub model: String,
    pub model_version: Option<String>,
    pub created_at: String,
    pub latency_ms: Option<i64>,
    pub confidence: Option<f64>,
    pub intent: Option<String>,
    pub vertical: Option<String>,
}

pub async fn list_decisions(
    db: &Db,
    tenant_id: &str,
    provider: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<(Vec<DecisionSummaryRow>, i64), sqlx::Error> {
    let rows = sqlx::query_as::<_, DecisionSummaryRow>(
        "SELECT r.id, r.post_id, l.id AS lead_id, r.provider, r.model, r.model_version, \
         r.created_at, r.latency_ms, l.confidence, l.intent, l.vertical_primary AS vertical \
         FROM decision_runs r \
         LEFT JOIN leads l ON l.post_id = r.post_id AND l.tenant_id = r.tenant_id \
         WHERE r.tenant_id = ? AND (? IS NULL OR r.provider = ?) \
         ORDER BY r.created_at DESC LIMIT ? OFFSET ?",
    )
    .bind(tenant_id)
    .bind(provider)
    .bind(provider)
    .bind(limit)
    .bind(offset)
    .fetch_all(&db.reader)
    .await?;
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM decision_runs WHERE tenant_id = ? AND (? IS NULL OR provider = ?)",
    )
    .bind(tenant_id)
    .bind(provider)
    .bind(provider)
    .fetch_one(&db.reader)
    .await?;
    Ok((rows, total))
}

/// Mean confidence per provider, for the dashboard.
pub async fn decision_stats(db: &Db, tenant_id: &str) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT provider, COUNT(*) FROM decision_runs WHERE tenant_id = ? AND status = 'succeeded' \
         GROUP BY provider ORDER BY COUNT(*) DESC",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await
}

/// Parse a stored answer back into the typed contract (used by API detail responses).
pub fn answer_to_api(row: &DecisionAnswerRow) -> Value {
    let kind = QuestionKind::parse_type(&row.answer_type);
    let probabilities: Option<BTreeMap<String, f64>> = row
        .probabilities
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok());
    json!({
        "question": row.question,
        "type": kind.map(|kind| kind.as_str()).unwrap_or("unknown"),
        "value": row.noul.or(row.score).or_else(|| row.choice.as_deref().map(|_| 0.0)),
        "noul": row.noul,
        "choice": row.choice,
        "score": row.score,
        "legend": row.legend.as_deref().and_then(|raw| serde_json::from_str::<Value>(raw).ok()),
        "probabilities": probabilities,
        "confidence": row.confidence,
    })
}

/// Kept for API compatibility: parses a stored answer type.
pub trait QuestionKindExt {
    fn parse_type(raw: &str) -> Option<QuestionKind>;
}

impl QuestionKindExt for QuestionKind {
    fn parse_type(raw: &str) -> Option<QuestionKind> {
        match raw {
            "noul" => Some(QuestionKind::Noul),
            "choice" => Some(QuestionKind::Choice),
            "score" => Some(QuestionKind::Score),
            _ => None,
        }
    }
}

/// Timestamp helper used by tests.
pub fn published_is_recent(published_at: Option<&str>) -> bool {
    published_at
        .and_then(parse_iso)
        .map(|value| (now() - value).num_days() <= 30)
        .unwrap_or(false)
}

/// Decisions returned by the mock provider are still auditable: the run and answers are stored.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trigger_keys_are_idempotent_only_for_automatic_runs() {
        assert_eq!(
            Trigger::Auto.idempotency_key("p1", "hash"),
            Some("decision:p1:hash".to_string())
        );
        assert_eq!(Trigger::Manual.idempotency_key("p1", "hash"), None);
    }

    #[test]
    fn answer_types_parse() {
        assert_eq!(QuestionKind::parse_type("score"), Some(QuestionKind::Score));
        assert_eq!(QuestionKind::parse_type("unknown"), None);
    }
}
