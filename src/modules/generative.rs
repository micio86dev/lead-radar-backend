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

fn truncate(text: &str, limit: usize) -> String {
    let mut truncated: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        truncated.push('…');
    }
    truncated
}

/// Rules quoted in the prompt. They mirror `domain::comment::validate` exactly.
fn policy_rules(language: &str) -> String {
    let examples = crate::domain::comment::DISCLOSURE_EXAMPLES
        .iter()
        .map(|(code, sentence)| format!("{code}: \"{sentence}\""))
        .collect::<Vec<_>>()
        .join(" | ");
    format!(
        "Rules checked automatically after generation (a draft that breaks one is rejected):\n\
         - The discussion language is {language}; write every body in it.\n\
         - soft_pitch and direct_pitch must name VoxTranslate and open with an affiliation \
         disclosure. Use the documented form for the language: {examples}\n\
         - Never use any of these phrases: {}.\n\
         - The helpful variant must stay non-promotional, must not name the product, and must not \
         contain: {}.\n\
         Voice — a draft that fails these is rejected too: write the way a person types in a \
         forum, in plain words and contractions, two to four sentences, no headings, no bullet \
         lists, no emoji, no exclamation marks. Open with the concrete answer to what was asked, \
         not with a preamble about the topic. Say something specific to this thread, including one \
         detail only this thread would produce. Allow a mild opinion or an honest limit (\"depends \
         on\", \"not sure\", \"that bit I have not tried\"). Never offer to help further, never ask \
         for a DM, never suggest a call, never sound enthusiastic about a product. Never use any of \
         these phrases: {}.",
        crate::domain::comment::FORBIDDEN_PHRASES.join(", "),
        crate::domain::comment::PROMOTIONAL_PHRASES.join(", "),
        crate::domain::comment::MACHINE_PHRASES.join(", "),
    )
}

#[derive(Debug, Default)]
struct DraftOutcome {
    stored: usize,
    skipped: usize,
    issues: Vec<String>,
}

type RejectedDraft = (CommentVariant, String, Vec<CommentIssue>);

/// Validate every draft, store the ones that pass and return the rejected ones with their reasons.
async fn validate_and_store(
    state: &AppState,
    tenant_id: &str,
    lead_id: &str,
    language: &str,
    actor: &str,
    variants: Vec<(CommentVariant, String)>,
    outcome: &mut DraftOutcome,
) -> Result<Vec<RejectedDraft>, GenerativeError> {
    let mut rejected: Vec<RejectedDraft> = Vec::new();
    let mut seen: Vec<CommentVariant> = Vec::new();
    for (variant, body) in variants {
        if seen.contains(&variant) {
            outcome.skipped += 1;
            outcome
                .issues
                .push(format!("duplicate variant {}", variant.as_str()));
            continue;
        }
        let draft = CommentDraft {
            variant,
            language: language.to_string(),
            body: body.clone(),
        };
        let problems = validate(&draft);
        if problems.is_empty() {
            seen.push(variant);
            store_comment(
                &state.db, tenant_id, lead_id, variant, &body, language, actor,
            )
            .await?;
            outcome.stored += 1;
        } else {
            outcome.skipped += 1;
            for problem in &problems {
                outcome
                    .issues
                    .push(format!("{}: {problem:?}", variant.as_str()));
            }
            rejected.push((variant, body, problems));
        }
    }
    Ok(rejected)
}

/// The rules the community itself published, phrased so the model writes what the moderators allow.
///
/// The publication path enforces the same policy, so a prompt that ignores this block cannot turn
/// into a recorded pitch — but a prompt that respects it produces a draft a human can actually use.
async fn engagement_rules(state: &AppState, tenant_id: &str, community: Option<&str>) -> String {
    let policy = match community {
        Some(name) => crate::modules::sources::community_policy_by_name(&state.db, tenant_id, name)
            .await
            .ok()
            .flatten(),
        None => None,
    };
    let Some(policy) = policy else {
        return "COMMUNITY RULES: this community has no recorded policy. Promotion is not allowed: \
                write the helpful variant only and do not name the product."
            .to_string();
    };
    format!(
        "COMMUNITY RULES (r/{name}): promotion {promotion}; minimum account age {age} days; \
minimum comment karma {karma}; flair required {flair}; wait {cooldown} days between promotional \
replies. Recorded rules: {rules}\n\
Hard rules: answer the question first and in the discussion's own terms. Name the product only \
when promotion is allowed, in at most one sentence, with the affiliation disclosure. Never list \
features, prices, integrations or capabilities, never quote customer stories, and never answer \
with marketing phrasing.",
        name = policy.name,
        promotion = policy.self_promotion,
        age = policy.min_account_age_days,
        karma = policy.min_comment_karma,
        flair = if policy.flair_required { "yes" } else { "no" },
        cooldown = policy.promo_cooldown_days,
        rules = if policy.rules_summary.is_empty() {
            "none recorded"
        } else {
            &policy.rules_summary
        },
    )
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
public discussion. Answer the actual problem, in the discussion's language, and never invent \
personal experience, testimonials, pricing or integrations. Answer with JSON only.";

    // The prompt quotes the same rules the validator enforces, so the model is never asked to guess
    // them and the two cannot drift apart.
    let policy = policy_rules(&language);
    let engagement = engagement_rules(state, tenant_id, lead.community.as_deref()).await;
    let user = format!(
        "{}\n\n{}\n\nlanguage: {}\n\n{}\n\nProduce exactly three variants as JSON: \
{{\"variants\":[{{\"variant\":\"helpful\",\"body\":\"...\"}},{{\"variant\":\"soft_pitch\",\
\"body\":\"...\"}},{{\"variant\":\"direct_pitch\",\"body\":\"...\"}}]}}. Each body must be one \
paragraph between 40 and 1200 characters.",
        lead_context(&lead, &post.content, &evidence),
        PRODUCT_CONTEXT,
        language,
        policy,
    );
    let user = format!("{user}\n\n{engagement}");

    if let Some(false) =
        crate::modules::tenancy::provider_toggle(&state.db, tenant_id, "qwen").await?
    {
        return Err(GenerativeError::InvalidContent(
            "generative provider disabled by tenant settings".to_string(),
        ));
    }

    let mut calls = Vec::new();
    let result = state
        .providers
        .generative
        .generate_json(system, &user, "comment_variants")
        .await?;
    let pending = parse_variants(&result.content)?;
    if pending.len() != 3 {
        return Err(GenerativeError::InvalidContent(format!(
            "expected 3 variants, got {}",
            pending.len()
        )));
    }
    let mut draft = DraftOutcome::default();
    let rejected = validate_and_store(
        state, tenant_id, lead_id, &language, actor, pending, &mut draft,
    )
    .await?;
    let mut corrective_content = None;

    // One bounded corrective pass, same provider, only for the variants that failed the policy.
    if !rejected.is_empty() {
        let failures = rejected
            .iter()
            .map(|(variant, body, problems)| {
                format!(
                    "- {} ({}) failed: {}",
                    variant.as_str(),
                    truncate(body, 120),
                    problems
                        .iter()
                        .map(|problem| format!("{problem:?}"))
                        .collect::<Vec<_>>()
                        .join("; ")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let corrective_prompt = format!(
            "{user}\n\nYour previous drafts failed these automatic checks:\n{failures}\n\n\
             Return the same JSON shape with all three variants and rewrite the failed ones so that \
             they pass every rule. Keep the bodies that already passed unchanged."
        );
        let retry = state
            .providers
            .generative
            .generate_json(system, &corrective_prompt, "comment_variants")
            .await?;
        let failed: Vec<CommentVariant> = rejected.iter().map(|(variant, _, _)| *variant).collect();
        let mut retry_variants = parse_variants(&retry.content)?;
        retry_variants.retain(|(variant, _)| failed.contains(variant));
        // A second rejection is final: the remaining variants stay unstored and are reported.
        validate_and_store(
            state,
            tenant_id,
            lead_id,
            &language,
            actor,
            retry_variants,
            &mut draft,
        )
        .await?;
        corrective_content = Some(retry.content.clone());
        calls.push(retry);
    }
    calls.push(result);

    let stored = draft.stored;
    let skipped = draft.skipped;
    let issues = draft.issues;
    let primary = calls.pop().expect("primary call recorded");
    let corrective_pass = corrective_content.is_some();

    sqlx::query(
        "INSERT INTO ai_analyses (id, tenant_id, post_id, kind, provider, model, prompt_summary, \
         summary, raw, input_tokens, output_tokens, latency_ms, created_at) \
         VALUES (?, ?, ?, 'comment_generation', ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(&lead.post_id)
    .bind(&primary.provider)
    .bind(&primary.model)
    .bind(format!("lead {lead_id}"))
    .bind(format!(
        "stored {stored} variants, skipped {skipped}, corrective pass: {corrective_pass}"
    ))
    .bind(to_json(&json!({
        "content": primary.content,
        "corrective_content": corrective_content,
        "issues": issues,
        "corrective_pass": corrective_pass,
    })))
    .bind(primary.input_tokens)
    .bind(primary.output_tokens)
    .bind(primary.latency_ms)
    .bind(now_iso())
    .execute(&state.db.writer)
    .await?;

    for call in std::iter::once(&primary).chain(calls.iter()) {
        record_generation_usage(
            &state.db,
            tenant_id,
            "comment_generation",
            &call.provider,
            call.input_tokens,
            call.output_tokens,
            call.latency_ms,
            call.request_id.as_deref(),
        )
        .await?;
    }

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
