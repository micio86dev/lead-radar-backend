//! Projects: the product a tenant works on, and the communities that fit it.
//!
//! The product used to be a constant in the drafting prompt. A project holds the operator's own
//! words plus the public pages that describe it, and the matching analysis ranks the communities
//! that are already validated and enabled against that material. Matching is advisory: it writes a
//! ranking, never a permission, and it collects nothing.

use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::time::now_iso;
use crate::domain::webfetch::{read_pages, FetchOutcome, MAX_URLS};
use crate::modules::{new_id, operations, to_json};
use crate::providers::ProviderError;
use crate::state::AppState;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Project {
    pub id: String,
    pub tenant_id: String,
    pub name: String,
    pub summary: String,
    /// JSON array of public URLs, kept as text in the row and parsed here.
    pub website_urls: String,
    pub is_default: bool,
    pub created_at: String,
    pub updated_at: String,
}

impl Project {
    pub fn urls(&self) -> Vec<String> {
        from_json_array(&self.website_urls)
    }

    /// The text the drafting prompts use: name, summary and the pages' own words.
    pub fn prompt_context(&self) -> String {
        if self.summary.trim().is_empty() {
            format!("{} is the product this analysis is for.", self.name)
        } else {
            self.summary.trim().to_string()
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CommunityMatch {
    pub community_id: String,
    pub name: String,
    pub vertical: String,
    pub enabled: bool,
    pub relevance: i64,
    pub reason: String,
    pub matched_at: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectError {
    #[error("project not found")]
    NotFound,
    #[error("database error")]
    Database(#[from] sqlx::Error),
    #[error("no validated and enabled community to match against")]
    NoCandidates,
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error("the analysis returned nothing usable: {0}")]
    Unusable(String),
    #[error("invalid input: {0}")]
    Invalid(String),
}

pub struct NewProject<'a> {
    pub name: &'a str,
    pub summary: &'a str,
    pub website_urls: &'a [String],
}

pub struct ProjectPatch<'a> {
    pub name: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub website_urls: Option<&'a [String]>,
    pub is_default: Option<bool>,
}

fn from_json_array(raw: &str) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn to_urls_json(urls: &[String]) -> String {
    to_json(&urls.iter().take(MAX_URLS).cloned().collect::<Vec<_>>())
}

pub async fn list_projects(db: &Db, tenant_id: &str) -> Result<Vec<Project>, sqlx::Error> {
    sqlx::query_as::<_, Project>(
        "SELECT * FROM projects WHERE tenant_id = ? ORDER BY is_default DESC, name",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await
}

/// Projects with their stored ranking, for the page that lists them.
pub async fn list_with_matches(
    db: &Db,
    tenant_id: &str,
) -> Result<Vec<(Project, Vec<CommunityMatch>)>, sqlx::Error> {
    let projects = list_projects(db, tenant_id).await?;
    let rows = sqlx::query_as::<_, ProjectMatchRow>(
        "SELECT pc.project_id, c.id AS community_id, c.name, c.vertical, c.enabled, \
                pc.relevance, pc.reason, pc.matched_at \
         FROM project_communities pc \
         JOIN communities c ON c.id = pc.community_id \
         JOIN projects p ON p.id = pc.project_id \
         WHERE p.tenant_id = ? \
         ORDER BY pc.relevance DESC, c.name",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await?;
    let mut grouped: std::collections::BTreeMap<String, Vec<CommunityMatch>> = Default::default();
    for row in rows {
        grouped
            .entry(row.project_id.clone())
            .or_default()
            .push(CommunityMatch::from(CommunityMatchRow {
                community_id: row.community_id,
                name: row.name,
                vertical: row.vertical,
                enabled: row.enabled,
                relevance: row.relevance,
                reason: row.reason,
                matched_at: row.matched_at,
            }));
    }
    Ok(projects
        .into_iter()
        .map(|project| {
            let matches = grouped.remove(&project.id).unwrap_or_default();
            (project, matches)
        })
        .collect())
}

#[derive(FromRow)]
struct ProjectMatchRow {
    project_id: String,
    community_id: String,
    name: String,
    vertical: String,
    enabled: i64,
    relevance: i64,
    reason: String,
    matched_at: String,
}

pub async fn get_project(
    db: &Db,
    tenant_id: &str,
    id: &str,
) -> Result<Option<Project>, sqlx::Error> {
    sqlx::query_as::<_, Project>("SELECT * FROM projects WHERE tenant_id = ? AND id = ?")
        .bind(tenant_id)
        .bind(id)
        .fetch_optional(&db.reader)
        .await
}

/// The project the prompts use: the flagged default, else the only one, else nothing.
pub async fn default_project(db: &Db, tenant_id: &str) -> Result<Option<Project>, sqlx::Error> {
    let row = sqlx::query_as::<_, Project>(
        "SELECT * FROM projects WHERE tenant_id = ? ORDER BY is_default DESC, name LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(&db.reader)
    .await?;
    Ok(row)
}

pub async fn create_project(
    db: &Db,
    tenant_id: &str,
    input: &NewProject<'_>,
) -> Result<Project, ProjectError> {
    let name = input.name.trim();
    if name.is_empty() {
        return Err(ProjectError::Invalid("name must not be empty".to_string()));
    }
    let duplicate: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM projects WHERE tenant_id = ? AND lower(name) = lower(?)",
    )
    .bind(tenant_id)
    .bind(name)
    .fetch_one(&db.reader)
    .await?;
    if duplicate > 0 {
        return Err(ProjectError::Invalid(format!(
            "a project named {name:?} already exists in this tenant"
        )));
    }
    let existing: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM projects WHERE tenant_id = ?")
        .bind(tenant_id)
        .fetch_one(&db.reader)
        .await?;
    let now = now_iso();
    let id = new_id();
    // The first project of a tenant is its default: a single-product tenant never has to pick.
    let is_default = existing == 0;
    sqlx::query(
        "INSERT INTO projects (id, tenant_id, name, summary, website_urls, is_default, created_at, \
         updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(tenant_id)
    .bind(name)
    .bind(input.summary.trim())
    .bind(to_urls_json(input.website_urls))
    .bind(i64::from(is_default))
    .bind(&now)
    .bind(&now)
    .execute(&db.writer)
    .await?;
    get_project(db, tenant_id, &id)
        .await?
        .ok_or(ProjectError::NotFound)
}

pub async fn patch_project(
    db: &Db,
    tenant_id: &str,
    id: &str,
    patch: &ProjectPatch<'_>,
) -> Result<Project, ProjectError> {
    let current = get_project(db, tenant_id, id)
        .await?
        .ok_or(ProjectError::NotFound)?;
    if let Some(name) = patch.name {
        if name.trim().is_empty() {
            return Err(ProjectError::Invalid("name must not be empty".to_string()));
        }
        sqlx::query("UPDATE projects SET name = ?, updated_at = ? WHERE id = ? AND tenant_id = ?")
            .bind(name.trim())
            .bind(now_iso())
            .bind(id)
            .bind(tenant_id)
            .execute(&db.writer)
            .await?;
    }
    if let Some(summary) = patch.summary {
        sqlx::query(
            "UPDATE projects SET summary = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(summary.trim())
        .bind(now_iso())
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(urls) = patch.website_urls {
        sqlx::query(
            "UPDATE projects SET website_urls = ?, updated_at = ? WHERE id = ? AND tenant_id = ?",
        )
        .bind(to_urls_json(urls))
        .bind(now_iso())
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    }
    if let Some(is_default) = patch.is_default {
        if is_default {
            // One default per tenant, enforced by a partial unique index: clear the other first.
            sqlx::query("UPDATE projects SET is_default = 0 WHERE tenant_id = ? AND id != ?")
                .bind(tenant_id)
                .bind(id)
                .execute(&db.writer)
                .await?;
            sqlx::query(
                "UPDATE projects SET is_default = 1, updated_at = ? WHERE id = ? AND tenant_id = ?",
            )
            .bind(now_iso())
            .bind(id)
            .bind(tenant_id)
            .execute(&db.writer)
            .await?;
        } else if current.is_default {
            return Err(ProjectError::Invalid(
                "a tenant keeps one default project: make another one the default instead"
                    .to_string(),
            ));
        }
    }
    get_project(db, tenant_id, id)
        .await?
        .ok_or(ProjectError::NotFound)
}

pub async fn delete_project(db: &Db, tenant_id: &str, id: &str) -> Result<bool, ProjectError> {
    let result = sqlx::query("DELETE FROM projects WHERE id = ? AND tenant_id = ?")
        .bind(id)
        .bind(tenant_id)
        .execute(&db.writer)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn project_matches(
    db: &Db,
    tenant_id: &str,
    project_id: &str,
) -> Result<Vec<CommunityMatch>, sqlx::Error> {
    sqlx::query_as::<_, CommunityMatchRow>(
        "SELECT c.id AS community_id, c.name, c.vertical, c.enabled, pc.relevance, pc.reason, \
                pc.matched_at \
         FROM project_communities pc \
         JOIN communities c ON c.id = pc.community_id \
         JOIN projects p ON p.id = pc.project_id \
         WHERE p.tenant_id = ? AND pc.project_id = ? \
         ORDER BY pc.relevance DESC, c.name",
    )
    .bind(tenant_id)
    .bind(project_id)
    .fetch_all(&db.reader)
    .await
    .map(|rows| rows.into_iter().map(CommunityMatch::from).collect())
}

#[derive(FromRow)]
struct CommunityMatchRow {
    community_id: String,
    name: String,
    vertical: String,
    enabled: i64,
    relevance: i64,
    reason: String,
    matched_at: String,
}

impl From<CommunityMatchRow> for CommunityMatch {
    fn from(row: CommunityMatchRow) -> Self {
        Self {
            community_id: row.community_id,
            name: row.name,
            vertical: row.vertical,
            enabled: row.enabled != 0,
            relevance: row.relevance,
            reason: row.reason,
            matched_at: row.matched_at,
        }
    }
}

/// Which provider answers the ranking question. The generative one reads the whole candidate list in
/// one call and explains each score; the decision provider answers one probability per community and
/// costs one call per community, with no prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scorer {
    Generative,
    Decision,
}

impl Scorer {
    pub fn parse(raw: &str) -> Result<Self, ProjectError> {
        match raw.trim() {
            "" | "generative" | "qwen" => Ok(Self::Generative),
            "decision" | "jev" => Ok(Self::Decision),
            other => Err(ProjectError::Invalid(format!(
                "unknown scorer {other:?}: use generative or decision"
            ))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Generative => "generative",
            Self::Decision => "decision",
        }
    }
}

pub struct AnalysisOutcome {
    pub project_id: String,
    pub candidates: usize,
    pub matched: usize,
    pub pages: Vec<FetchOutcome>,
    pub provider: String,
    pub model: String,
    pub scorer: Scorer,
}

/// Rank the communities that are already validated and enabled against the project material.
///
/// Nothing is enabled, disabled or validated here: the analysis writes a ranking and the operator
/// decides what to do with it.
pub async fn analyze_communities(
    state: &AppState,
    tenant_id: &str,
    project_id: &str,
    scorer: Scorer,
) -> Result<AnalysisOutcome, ProjectError> {
    let project = get_project(&state.db, tenant_id, project_id)
        .await?
        .ok_or(ProjectError::NotFound)?;

    let candidates: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT id, name, vertical FROM communities \
         WHERE tenant_id = ? AND validation_status = 'validated' AND enabled = 1 \
         ORDER BY name",
    )
    .bind(tenant_id)
    .fetch_all(&state.db.reader)
    .await?;
    if candidates.is_empty() {
        return Err(ProjectError::NoCandidates);
    }

    // Read the operator's pages. A failure is reported per URL and never hides the rest.
    let pages = read_pages(&project.urls()).await;
    let page_text = pages
        .iter()
        .filter_map(|page| {
            page.text
                .as_ref()
                .map(|text| (page.url.as_str(), text.as_str()))
        })
        .map(|(url, text)| format!("--- {url} ---\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n");

    if scorer == Scorer::Decision {
        return analyze_with_decision(state, tenant_id, &project, &candidates, &page_text).await;
    }

    let candidate_list = candidates
        .iter()
        .map(|(_, name, vertical)| format!("{name} ({vertical})"))
        .collect::<Vec<_>>()
        .join("\n");

    let system = "You score how well online communities fit a product, for a human who decides \
what to do with the ranking. Judge topical fit only: would the people in this community discuss \
the problem this product solves? Answer with JSON only.";
    let user = format!(
        "PROJECT: {name}\n\nWHAT IT IS:\n{summary}\n\nWEBSITE CONTENT:\n{website}\n\n\
         COMMUNITIES (already validated and active):\n{candidates}\n\n\
         Score every community from 0 to 100 for topical fit with this product, and give one short \
         reason grounded in what the product does. Do not reward communities just because they are \
         large or general. Use exactly the community names above.",
        name = project.name,
        summary = project.prompt_context(),
        website = if page_text.is_empty() {
            "no page could be read; judge from the description alone".to_string()
        } else {
            page_text
        },
        candidates = candidate_list,
    );
    let schema =
        "{\"matches\":[{\"community\":\"name\",\"relevance\":0-100,\"reason\":\"one line\"}]}";

    let result = state
        .providers
        .generative
        .generate_json(system, &user, schema)
        .await?;

    let parsed = parse_matches(&result.content, &candidates)?;
    store_matches(&state.db, project_id, &parsed).await?;

    operations::record_usage(
        &state.db,
        Some(tenant_id),
        &operations::UsageRecord {
            provider: &result.provider,
            operation: "project_community_match",
            request_id: result.request_id.as_deref(),
            input_tokens: result.input_tokens,
            output_tokens: result.output_tokens,
            latency_ms: Some(result.latency_ms),
            status: "succeeded",
            error: None,
        },
    )
    .await?;
    operations::audit(
        &state.db,
        tenant_id,
        "system:match",
        "project_communities_matched",
        "project",
        Some(project_id),
        &json!({"candidates": candidates.len(), "matched": parsed.len()}),
    )
    .await?;

    Ok(AnalysisOutcome {
        project_id: project_id.to_string(),
        candidates: candidates.len(),
        matched: parsed.len(),
        pages,
        provider: result.provider,
        model: result.model,
        scorer,
    })
}

/// One decision call per candidate: a probability, no prose. Chosen when the operator prefers the
/// decision provider's calibration over a generative explanation.
async fn analyze_with_decision(
    state: &AppState,
    tenant_id: &str,
    project: &Project,
    candidates: &[(String, String, String)],
    page_text: &str,
) -> Result<AnalysisOutcome, ProjectError> {
    let question = crate::domain::decision::DecisionQuestion {
        id: "community_fit".to_string(),
        kind: crate::domain::decision::QuestionKind::Noul,
        instructions: "Probability that the people in this community discuss the problem the \
                       product solves. Judge topical fit only."
            .to_string(),
        criteria: None,
    };
    let mut parsed: Vec<(String, i64, String)> = Vec::new();
    let mut provider = String::from("decision");
    let mut model = String::new();
    for (id, name, vertical) in candidates {
        let request = crate::providers::DecisionRequest {
            state: format!(
                "PRODUCT: {name}\n{summary}\n\nWEBSITE:\n{website}\n\nCOMMUNITY: r/{community} \
                 ({vertical})",
                name = project.name,
                summary = project.prompt_context(),
                website = if page_text.is_empty() {
                    "not read"
                } else {
                    page_text
                },
                community = name,
                vertical = vertical,
            ),
            questions: vec![crate::domain::decision::DecisionQuestion {
                id: question.id.clone(),
                kind: question.kind,
                instructions: question.instructions.clone(),
                criteria: None,
            }],
            idempotency_key: Some(format!("project-fit:{}-{id}", project.id)),
        };
        let result = state.providers.decision.evaluate(&request).await?;
        provider = result.provider.clone();
        model = result.model.clone();
        let probability = result.noul("community_fit").unwrap_or(0.0).clamp(0.0, 1.0);
        parsed.push((
            id.clone(),
            (probability * 100.0).round() as i64,
            format!("probability {probability:.2} that this community discusses the problem"),
        ));
        operations::record_usage(
            &state.db,
            Some(tenant_id),
            &operations::UsageRecord {
                provider: &result.provider,
                operation: "project_community_match",
                request_id: result.provider_request_id.as_deref(),
                input_tokens: result.usage.input_tokens,
                output_tokens: result.usage.output_tokens,
                latency_ms: Some(result.latency_ms),
                status: "succeeded",
                error: None,
            },
        )
        .await?;
    }
    parsed.sort_by(|left, right| right.1.cmp(&left.1));
    store_matches(&state.db, &project.id, &parsed).await?;
    operations::audit(
        &state.db,
        tenant_id,
        "system:match",
        "project_communities_matched",
        "project",
        Some(&project.id),
        &json!({"candidates": candidates.len(), "matched": parsed.len(), "scorer": "decision"}),
    )
    .await?;
    Ok(AnalysisOutcome {
        project_id: project.id.clone(),
        candidates: candidates.len(),
        matched: parsed.len(),
        pages: Vec::new(),
        provider,
        model,
        scorer: Scorer::Decision,
    })
}

/// Keep only the communities that exist in the candidate list, clamp the score, drop empty names.
fn parse_matches(
    content: &serde_json::Value,
    candidates: &[(String, String, String)],
) -> Result<Vec<(String, i64, String)>, ProjectError> {
    let items = content
        .get("matches")
        .and_then(|value| value.as_array())
        .ok_or_else(|| ProjectError::Unusable("matches array is missing".to_string()))?;
    let mut parsed: Vec<(String, i64, String)> = Vec::new();
    for item in items {
        let Some(name) = item.get("community").and_then(|value| value.as_str()) else {
            continue;
        };
        let Some((id, canonical, _)) = candidates
            .iter()
            .find(|(_, candidate, _)| candidate.eq_ignore_ascii_case(name.trim()))
        else {
            continue;
        };
        let relevance = item
            .get("relevance")
            .and_then(|value| {
                value
                    .as_i64()
                    .or_else(|| value.as_f64().map(|number| number.round() as i64))
            })
            .unwrap_or(0)
            .clamp(0, 100);
        let reason = item
            .get("reason")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .trim()
            .chars()
            .take(400)
            .collect::<String>();
        // The table already shows the community name; repeating it in every reason is noise.
        let _ = canonical;
        parsed.push((id.clone(), relevance, reason));
    }
    if parsed.is_empty() {
        return Err(ProjectError::Unusable(
            "no community name in the answer matched a candidate".to_string(),
        ));
    }
    parsed.sort_by(|left, right| right.1.cmp(&left.1));
    Ok(parsed)
}

async fn store_matches(
    db: &Db,
    project_id: &str,
    matches: &[(String, i64, String)],
) -> Result<(), sqlx::Error> {
    let now = now_iso();
    let mut tx = db.writer.begin().await?;
    sqlx::query("DELETE FROM project_communities WHERE project_id = ?")
        .bind(project_id)
        .execute(&mut *tx)
        .await?;
    for (community_id, relevance, reason) in matches {
        sqlx::query(
            "INSERT INTO project_communities (project_id, community_id, relevance, reason, \
             matched_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(project_id)
        .bind(community_id)
        .bind(relevance)
        .bind(reason)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates() -> Vec<(String, String, String)> {
        vec![
            (
                "id-1".to_string(),
                "humanresources".to_string(),
                "hr_recruiting".to_string(),
            ),
            (
                "id-2".to_string(),
                "eventprofs".to_string(),
                "conferences_webinars".to_string(),
            ),
        ]
    }

    #[test]
    fn answers_are_matched_by_name_and_clamped() {
        let content = json!({"matches": [
            {"community": "HumanResources", "relevance": 88, "reason": "language barrier threads"},
            {"community": "eventprofs", "relevance": 140.4, "reason": "conference organisers"},
            {"community": "not-a-candidate", "relevance": 99, "reason": "ignored"}
        ]});
        let parsed = parse_matches(&content, &candidates()).expect("parsed");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].0, "id-2");
        assert_eq!(parsed[0].1, 100);
        assert_eq!(parsed[1].0, "id-1");
        assert_eq!(parsed[1].1, 88);
    }

    #[test]
    fn an_answer_without_known_communities_is_unusable() {
        let content = json!({"matches": [{"community": "made-up", "relevance": 50}]});
        assert!(matches!(
            parse_matches(&content, &candidates()),
            Err(ProjectError::Unusable(_))
        ));
        assert!(matches!(
            parse_matches(&json!({"nope": []}), &candidates()),
            Err(ProjectError::Unusable(_))
        ));
    }
}
