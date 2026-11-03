//! Generative analysis and comment drafting through the configured generative provider.
//!
//! Qwen never computes the score and never publishes: it drafts text that a human reviews. Drafts
//! that fail the comment policy are not stored; the validation issues are recorded instead.

use serde::Serialize;
use serde_json::json;
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::comment::{validate, CommentDraft, CommentIssue};
use crate::domain::enums::{CommentStatus, CommentVariant};
use crate::domain::time::now_iso;
use crate::modules::{new_id, operations, qualification, to_json};
use crate::providers::ProviderError;
use crate::state::AppState;

const PRODUCT_CONTEXT: &str = "VoxTranslate is a real-time multilingual voice translation product \
for live conversations: meetings, interviews, events, customer support and hospitality. It \
translates speech while people keep speaking their own language. Do not claim pricing, integrations \
or capabilities beyond this description.";

#[derive(Debug, thiserror::Error)]
pub enum GenerativeError {
    #[error("lead not found")]
    NotFound,
    #[error(transparent)]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("invalid generated content: {0}")]
    InvalidContent(String),
}

impl GenerativeError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Provider(error) => error.is_retryable(),
            Self::Database(_) => true,
            Self::NotFound | Self::InvalidContent(_) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AnalysisOutcome {
    pub id: String,
    pub summary: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone)]
pub struct CommentGenerationOutcome {
    pub stored: usize,
    pub skipped: usize,
    pub issues: Vec<String>,
}

fn lead_context(lead: &qualification::LeadRow, post_content: &str, evidence: &[String]) -> String {
    let evidence_block = if evidence.is_empty() {
        "(no configured keyword excerpt found; do not invent one)".to_string()
    } else {
        evidence
            .iter()
            .map(|excerpt| format!("- {excerpt}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "lead summary\n\
         title: {title}\n\
         community: {community}\n\
         language: {language}\n\
         vertical: {vertical}\n\
         intent: {intent}\n\
         customer_type: {customer_type}\n\
         buying_intent: {buying_intent}\n\
         product_fit: {product_fit}\n\
         score: {score}\n\n\
         original content:\n{content}\n\n\
         evidence excerpts (verbatim from the source):\n{evidence_block}",
        title = lead
            .title
            .clone()
            .unwrap_or_else(|| "(no title)".to_string()),
        community = lead
            .community
            .clone()
            .unwrap_or_else(|| "unknown".to_string()),
        language = lead.language.clone().unwrap_or_else(|| "und".to_string()),
        vertical = lead.vertical_primary,
        intent = lead.intent,
        customer_type = lead.customer_type,
        buying_intent = lead.buying_intent,
        product_fit = lead.product_fit,
        score = lead.score.unwrap_or(0),
        content = post_content,
        evidence_block = evidence_block,
    )
}

/// Deeper analysis for a selected opportunity. Only selected leads reach this call.
pub async fn analyze_lead(
    state: &AppState,
    tenant_id: &str,
    lead_id: &str,
) -> Result<AnalysisOutcome, GenerativeError> {
    let lead = qualification::get_lead(&state.db, tenant_id, lead_id)
        .await?
        .ok_or(GenerativeError::NotFound)?;
    let post = crate::modules::ingestion::get_post(&state.db, tenant_id, &lead.post_id)
        .await?
        .ok_or(GenerativeError::NotFound)?;
    let evidence: Vec<String> = qualification::lead_evidence(&lead)
        .into_iter()
        .map(|item| item.excerpt)
        .collect();

    let system = "You analyse B2B opportunities for a supplier of real-time multilingual voice \
translation. Ground every statement in the supplied content; never invent budget, company size, \
authority, timeline or willingness to buy. Answer with JSON.";
    let user = format!(
        "{}\n\n{}\n\nlanguage: {}\n\nSummarise the need, the risk of misreading it and a suggested \
commercial approach for a human reviewer.",
        lead_context(&lead, &post.content, &evidence),
        PRODUCT_CONTEXT,
        lead.language.clone().unwrap_or_else(|| "und".to_string()),
    );

    let result = state
        .providers
        .generative
        .generate_json(system, &user, "opportunity_analysis")
        .await?;

    let summary = result
        .content
        .get("summary")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    if summary.trim().is_empty() {
        return Err(GenerativeError::InvalidContent(
            "analysis summary is missing".to_string(),
        ));
    }

    let id = new_id();
    sqlx::query(
        "INSERT INTO ai_analyses (id, tenant_id, post_id, kind, provider, model, prompt_summary, \
         summary, raw, input_tokens, output_tokens, latency_ms, created_at) \
         VALUES (?, ?, ?, 'generative_analysis', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(&lead.post_id)
    .bind(&result.provider)
    .bind(&result.model)
    .bind(format!("lead {lead_id}"))
    .bind(&summary)
    .bind(to_json(&result.content))
    .bind(result.input_tokens)
    .bind(result.output_tokens)
    .bind(result.latency_ms)
    .bind(now_iso())
    .execute(&state.db.writer)
    .await?;

    record_generation_usage(
        &state.db,
        tenant_id,
        "generative_analysis",
        &result.provider,
        result.input_tokens,
        result.output_tokens,
        result.latency_ms,
        result.request_id.as_deref(),
    )
    .await?;

    Ok(AnalysisOutcome {
        id,
        summary,
        provider: result.provider,
        model: result.model,
    })
}

#[allow(clippy::too_many_arguments)]
async fn record_generation_usage(
    db: &Db,
    tenant_id: &str,
    operation: &str,
    provider: &str,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    latency_ms: i64,
    request_id: Option<&str>,
) -> Result<(), sqlx::Error> {
    operations::record_usage(
        db,
        Some(tenant_id),
        &operations::UsageRecord {
            provider,
            operation,
            request_id,
            input_tokens,
            output_tokens,
            latency_ms: Some(latency_ms),
            status: "succeeded",
            error: None,
        },
    )
    .await?;
    operations::record_cost(
        db,
        Some(tenant_id),
        provider,
        operations::CostKind::Actual,
        0.0,
        "USD",
        operation,
        request_id,
        true,
    )
    .await?;
    Ok(())
}

fn parse_variants(
    content: &serde_json::Value,
) -> Result<Vec<(CommentVariant, String)>, GenerativeError> {
    let variants = content
        .get("variants")
        .and_then(|value| value.as_array())
        .ok_or_else(|| GenerativeError::InvalidContent("variants array is missing".to_string()))?;
    let mut parsed = Vec::new();
    for variant in variants {
        let label = variant
            .get("variant")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                GenerativeError::InvalidContent("variant name is missing".to_string())
            })?;
        let body = variant
            .get("body")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                GenerativeError::InvalidContent("variant body is missing".to_string())
            })?;
        let variant = CommentVariant::parse(label).ok_or_else(|| {
            GenerativeError::InvalidContent(format!("unknown comment variant {label:?}"))
        })?;
        parsed.push((variant, body.trim().to_string()));
    }
    Ok(parsed)
}

/// Draft the three comment variants for a lead. Only drafts that satisfy the comment policy are
/// stored, and they are stored as `pending_review`: no publication path exists in code.
pub async fn generate_comments(
    state: &AppState,
    tenant_id: &str,
    lead_id: &str,
    actor: &str,
) -> Result<CommentGenerationOutcome, GenerativeError> {
    let lead = qualification::get_lead(&state.db, tenant_id, lead_id)
        .await?
        .ok_or(GenerativeError::NotFound)?;
    let post = crate::modules::ingestion::get_post(&state.db, tenant_id, &lead.post_id)
        .await?
        .ok_or(GenerativeError::NotFound)?;
    let evidence: Vec<String> = qualification::lead_evidence(&lead)
        .into_iter()
        .map(|item| item.excerpt)
        .collect();
    let language = lead
        .language
        .clone()
        .unwrap_or_else(|| crate::domain::language::detect(&post.content));

    let system = "You write short, specific replies for a human to review before posting in a \
public discussion. Rules: write in the discussion's language; answer the actual problem; never \
invent personal experience, testimonials, pricing or integrations; disclose the affiliation \
whenever pitching. The \"helpful\" variant must not sell. Answer with JSON only.";
    let user = format!(
        "{}\n\n{}\n\nlanguage: {}\n\nProduce exactly three variants as JSON: \
{{\"variants\":[{{\"variant\":\"helpful\",\"body\":\"...\"}},{{\"variant\":\"soft_pitch\",\
\"body\":\"...\"}},{{\"variant\":\"direct_pitch\",\"body\":\"...\"}}]}}. Each body must be one \
paragraph between 40 and 1200 characters.",
        lead_context(&lead, &post.content, &evidence),
        PRODUCT_CONTEXT,
        language,
    );

    if let Some(false) =
        crate::modules::tenancy::provider_toggle(&state.db, tenant_id, "qwen").await?
    {
        return Err(GenerativeError::InvalidContent(
            "generative provider disabled by tenant settings".to_string(),
        ));
    }
    let result = state
        .providers
        .generative
        .generate_json(system, &user, "comment_variants")
        .await?;
    let parsed = parse_variants(&result.content)?;
    if parsed.len() != 3 {
        return Err(GenerativeError::InvalidContent(format!(
            "expected 3 variants, got {}",
            parsed.len()
        )));
    }

    let mut stored = 0usize;
    let mut skipped = 0usize;
    let mut issues = Vec::new();
    let mut seen: Vec<CommentVariant> = Vec::new();

    for (variant, body) in parsed {
        if seen.contains(&variant) {
            skipped += 1;
            issues.push(format!("duplicate variant {}", variant.as_str()));
            continue;
        }
        let draft = CommentDraft {
            variant,
            language: language.clone(),
            body: body.clone(),
        };
        let problems: Vec<CommentIssue> = validate(&draft);
        if !problems.is_empty() {
            skipped += 1;
            for problem in problems {
                issues.push(format!("{}: {problem:?}", variant.as_str()));
            }
            continue;
        }
        seen.push(variant);
        store_comment(
            &state.db, tenant_id, lead_id, variant, &body, &language, actor,
        )
        .await?;
        stored += 1;
    }

    sqlx::query(
        "INSERT INTO ai_analyses (id, tenant_id, post_id, kind, provider, model, prompt_summary, \
         summary, raw, input_tokens, output_tokens, latency_ms, created_at) \
         VALUES (?, ?, ?, 'comment_generation', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(&lead.post_id)
    .bind(&result.provider)
    .bind(&result.model)
    .bind(format!("lead {lead_id}"))
    .bind(format!("stored {stored} variants, skipped {skipped}"))
    .bind(to_json(
        &json!({"content": result.content, "issues": issues}),
    ))
    .bind(result.input_tokens)
    .bind(result.output_tokens)
    .bind(result.latency_ms)
    .bind(now_iso())
    .execute(&state.db.writer)
    .await?;

    record_generation_usage(
        &state.db,
        tenant_id,
        "comment_generation",
        &result.provider,
        result.input_tokens,
        result.output_tokens,
        result.latency_ms,
        result.request_id.as_deref(),
    )
    .await?;

    operations::audit(
        &state.db,
        tenant_id,
        actor,
        "comments_generated",
        "lead",
        Some(lead_id),
        &json!({"stored": stored, "skipped": skipped, "issues": issues}),
    )
    .await?;

    if stored == 0 {
        return Err(GenerativeError::InvalidContent(issues.join("; ")));
    }
    Ok(CommentGenerationOutcome {
        stored,
        skipped,
        issues,
    })
}

async fn store_comment(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
    variant: CommentVariant,
    body: &str,
    language: &str,
    actor: &str,
) -> Result<(), sqlx::Error> {
    let now = now_iso();
    let existing: Option<(String, i64)> = sqlx::query_as(
        "SELECT id, version FROM generated_comments WHERE tenant_id = ? AND lead_id = ? AND variant = ?",
    )
    .bind(tenant_id)
    .bind(lead_id)
    .bind(variant.as_str())
    .fetch_optional(&db.reader)
    .await?;

    match existing {
        Some((id, version)) => {
            let next = version + 1;
            sqlx::query(
                "UPDATE generated_comments SET body = ?, language = ?, status = 'pending_review', \
                 version = ?, updated_at = ?, published_at = NULL, publication_note = NULL \
                 WHERE id = ?",
            )
            .bind(body)
            .bind(language)
            .bind(next)
            .bind(&now)
            .bind(&id)
            .execute(&db.writer)
            .await?;
            insert_version(db, &id, next, body, actor).await?;
            insert_event(db, tenant_id, &id, "regenerated", actor, None).await?;
        }
        None => {
            let id = new_id();
            sqlx::query(
                "INSERT INTO generated_comments (id, tenant_id, lead_id, variant, body, language, \
                 status, version, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, 'pending_review', 1, ?, ?)",
            )
            .bind(&id)
            .bind(tenant_id)
            .bind(lead_id)
            .bind(variant.as_str())
            .bind(body)
            .bind(language)
            .bind(&now)
            .bind(&now)
            .execute(&db.writer)
            .await?;
            insert_version(db, &id, 1, body, actor).await?;
            insert_event(db, tenant_id, &id, "generated", actor, None).await?;
        }
    }
    Ok(())
}

pub async fn insert_version(
    db: &Db,
    comment_id: &str,
    version: i64,
    body: &str,
    actor: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO comment_versions (id, comment_id, version, body, actor, created_at) \
         VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(comment_id, version) DO NOTHING",
    )
    .bind(new_id())
    .bind(comment_id)
    .bind(version)
    .bind(body)
    .bind(actor)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

pub async fn insert_event(
    db: &Db,
    tenant_id: &str,
    comment_id: &str,
    event: &str,
    actor: &str,
    note: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO review_events (id, tenant_id, comment_id, event, actor, note, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(comment_id)
    .bind(event)
    .bind(actor)
    .bind(note)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

/// Latest generative analysis for a post, for the opportunity detail page.
#[derive(Debug, Clone, FromRow, Serialize)]
pub struct AnalysisRow {
    pub id: String,
    pub kind: String,
    pub provider: String,
    pub model: String,
    pub summary: Option<String>,
    pub error: Option<String>,
    pub created_at: String,
}

pub async fn analyses_for_post(
    db: &Db,
    tenant_id: &str,
    post_id: &str,
) -> Result<Vec<AnalysisRow>, sqlx::Error> {
    sqlx::query_as::<_, AnalysisRow>(
        "SELECT id, kind, provider, model, summary, error, created_at FROM ai_analyses \
         WHERE tenant_id = ? AND post_id = ? ORDER BY created_at DESC LIMIT 20",
    )
    .bind(tenant_id)
    .bind(post_id)
    .fetch_all(&db.reader)
    .await
}

/// Exposed for tests: generation always leaves comments in `pending_review`.
pub fn initial_status() -> CommentStatus {
    CommentStatus::PendingReview
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_comments_start_pending_review() {
        assert_eq!(initial_status(), CommentStatus::PendingReview);
    }

    #[test]
    fn variant_payloads_are_parsed_strictly() {
        let content = json!({
            "variants": [
                {"variant": "helpful", "body": "a".repeat(60)},
                {"variant": "soft_pitch", "body": "b".repeat(60)},
                {"variant": "direct_pitch", "body": "c".repeat(60)}
            ]
        });
        let parsed = parse_variants(&content).expect("parsed");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].0, CommentVariant::Helpful);
        assert!(parse_variants(&json!({"variants": [{"variant": "promo", "body": "x"}]})).is_err());
        assert!(parse_variants(&json!({"nope": []})).is_err());
    }
}
