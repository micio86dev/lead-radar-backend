//! Versioned scoring configuration, leads and score history.

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::FromRow;

use crate::db::Db;
use crate::domain::enums::{
    BusinessPotential, BuyingIntent, CustomerType, Intent, ProductFit, Vertical,
};
use crate::domain::evidence::Evidence;
use crate::domain::qualification::qualify;
use crate::domain::scoring::{QualificationThresholds, ScoreBands, ScoringConfig, ScoringWeights};
use crate::domain::time::now_iso;
use crate::modules::{from_json_or, new_id, to_json};

/// Load the active scoring configuration; falls back to the documented defaults (version 1) when the
/// seed has not run, so scoring is always deterministic and explainable.
pub async fn active_scoring_config(db: &Db, tenant_id: &str) -> Result<ScoringConfig, sqlx::Error> {
    let row: Option<(i64, String, String, String)> = sqlx::query_as(
        "SELECT version, weights, bands, thresholds FROM lead_score_versions \
         WHERE tenant_id = ? AND active = 1 ORDER BY version DESC LIMIT 1",
    )
    .bind(tenant_id)
    .fetch_optional(&db.reader)
    .await?;
    let Some((version, weights, bands, thresholds)) = row else {
        return Ok(ScoringConfig::default_v1());
    };
    Ok(ScoringConfig {
        version,
        weights: from_json_or::<ScoringWeights>(&weights, Default::default()),
        bands: from_json_or::<ScoreBands>(&bands, ScoreBands::default()),
        thresholds: from_json_or::<QualificationThresholds>(
            &thresholds,
            QualificationThresholds::default(),
        ),
    })
}

/// Persist a scoring version (used by the seed and by future configuration changes).
pub async fn save_scoring_version(
    db: &Db,
    tenant_id: &str,
    config: &ScoringConfig,
    activate: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = db.writer.begin().await?;
    if activate {
        sqlx::query("UPDATE lead_score_versions SET active = 0 WHERE tenant_id = ?")
            .bind(tenant_id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO lead_score_versions (id, tenant_id, version, weights, bands, thresholds, \
         active, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(tenant_id, version) DO NOTHING",
    )
    .bind(new_id())
    .bind(tenant_id)
    .bind(config.version)
    .bind(to_json(&config.weights))
    .bind(to_json(&config.bands))
    .bind(to_json(&config.thresholds))
    .bind(if activate { 1 } else { 0 })
    .bind(now_iso())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct LeadRow {
    pub id: String,
    pub tenant_id: String,
    pub post_id: String,
    pub title: Option<String>,
    pub language: Option<String>,
    pub source_id: Option<String>,
    pub source_name: Option<String>,
    pub community: Option<String>,
    pub source_url: Option<String>,
    pub author: Option<String>,
    pub published_at: Option<String>,
    pub discovered_at: String,
    pub vertical_primary: String,
    pub vertical_secondary: String,
    pub intent: String,
    pub customer_type: String,
    pub buying_intent: String,
    pub business_potential: String,
    pub product_fit: String,
    pub confidence: Option<f64>,
    pub evidence: String,
    pub score: Option<i64>,
    pub score_category: Option<String>,
    pub qualification_status: String,
    pub review_status: String,
    pub is_active: i64,
    pub analysis_count: i64,
    pub comment_count: i64,
    pub created_at: String,
    pub updated_at: String,
}

const LEAD_SELECT: &str = "SELECT l.*, s.name AS source_name, \
    (SELECT COUNT(*) FROM generated_comments c WHERE c.lead_id = l.id) AS comment_count \
    FROM leads l LEFT JOIN sources s ON s.id = l.source_id";

#[derive(Debug, Clone)]
pub struct LeadFilter {
    pub vertical: Option<String>,
    pub score_category: Option<String>,
    pub score_min: Option<i64>,
    pub score_max: Option<i64>,
    pub intent: Option<String>,
    pub language: Option<String>,
    pub source_id: Option<String>,
    pub community: Option<String>,
    pub published_after: Option<String>,
    pub published_before: Option<String>,
    pub review_status: Option<String>,
    pub customer_type: Option<String>,
    pub buying_intent: Option<String>,
    pub query: Option<String>,
    pub sort: Option<String>,
    pub order: Option<String>,
}

enum Param {
    Text(String),
    Int(i64),
}

pub async fn list_leads(
    db: &Db,
    tenant_id: &str,
    filter: &LeadFilter,
    limit: i64,
    offset: i64,
) -> Result<(Vec<LeadRow>, i64), sqlx::Error> {
    let mut conditions: Vec<&str> = vec!["l.tenant_id = ?", "l.is_active = 1"];
    let mut params: Vec<Param> = vec![Param::Text(tenant_id.to_string())];

    macro_rules! text_filter {
        ($value:expr, $clause:expr) => {
            if let Some(value) = &$value {
                conditions.push($clause);
                params.push(Param::Text(value.clone()));
            }
        };
    }

    text_filter!(filter.vertical, "l.vertical_primary = ?");
    text_filter!(filter.score_category, "l.score_category = ?");
    text_filter!(filter.intent, "l.intent = ?");
    text_filter!(filter.language, "l.language = ?");
    text_filter!(filter.source_id, "l.source_id = ?");
    text_filter!(filter.community, "l.community = ?");
    text_filter!(filter.review_status, "l.review_status = ?");
    text_filter!(filter.customer_type, "l.customer_type = ?");
    text_filter!(filter.buying_intent, "l.buying_intent = ?");
    if let Some(score_min) = filter.score_min {
        conditions.push("l.score >= ?");
        params.push(Param::Int(score_min));
    }
    if let Some(score_max) = filter.score_max {
        conditions.push("l.score <= ?");
        params.push(Param::Int(score_max));
    }
    if let Some(value) = &filter.published_after {
        conditions.push("l.published_at >= ?");
        params.push(Param::Text(value.clone()));
    }
    if let Some(value) = &filter.published_before {
        conditions.push("l.published_at <= ?");
        params.push(Param::Text(value.clone()));
    }
    if let Some(query) = &filter.query {
        conditions.push("(l.title LIKE ? OR p.content LIKE ?)");
        let pattern = format!("%{query}%");
        params.push(Param::Text(pattern.clone()));
        params.push(Param::Text(pattern));
    }

    let where_clause = conditions.join(" AND ");
    let order_clause = match filter.sort.as_deref().unwrap_or("score") {
        "recency" => "l.published_at",
        "buying_intent" => {
            "CASE l.buying_intent WHEN 'explicit' THEN 5 WHEN 'strong' THEN 4 \
             WHEN 'moderate' THEN 3 WHEN 'weak' THEN 2 ELSE 1 END"
        }
        "confidence" => "l.confidence",
        _ => "l.score",
    };
    let direction = match filter.order.as_deref().unwrap_or("desc") {
        "asc" => "ASC",
        _ => "DESC",
    };

    let sql = format!(
        "SELECT l.*, s.name AS source_name, \
         (SELECT COUNT(*) FROM generated_comments c WHERE c.lead_id = l.id) AS comment_count \
         FROM leads l LEFT JOIN sources s ON s.id = l.source_id \
         LEFT JOIN discovered_posts p ON p.id = l.post_id \
         WHERE {where_clause} ORDER BY {order_clause} {direction}, l.created_at DESC \
         LIMIT ? OFFSET ?"
    );
    let count_sql = format!(
        "SELECT COUNT(*) FROM leads l LEFT JOIN discovered_posts p ON p.id = l.post_id \
         WHERE {where_clause}"
    );

    let mut query = sqlx::query_as::<_, LeadRow>(&sql);
    for param in &params {
        query = match param {
            Param::Text(value) => query.bind(value),
            Param::Int(value) => query.bind(value),
        };
    }
    let rows = query.bind(limit).bind(offset).fetch_all(&db.reader).await?;

    let mut count_query = sqlx::query_scalar::<_, i64>(&count_sql);
    for param in &params {
        count_query = match param {
            Param::Text(value) => count_query.bind(value),
            Param::Int(value) => count_query.bind(value),
        };
    }
    let total = count_query.fetch_one(&db.reader).await?;
    Ok((rows, total))
}

pub async fn get_lead(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
) -> Result<Option<LeadRow>, sqlx::Error> {
    let sql = format!("{LEAD_SELECT} WHERE l.tenant_id = ? AND l.id = ? AND l.is_active = 1");
    sqlx::query_as::<_, LeadRow>(&sql)
        .bind(tenant_id)
        .bind(lead_id)
        .fetch_optional(&db.reader)
        .await
}

pub async fn get_lead_by_post(
    db: &Db,
    tenant_id: &str,
    post_id: &str,
) -> Result<Option<LeadRow>, sqlx::Error> {
    let sql = format!("{LEAD_SELECT} WHERE l.tenant_id = ? AND l.post_id = ?");
    sqlx::query_as::<_, LeadRow>(&sql)
        .bind(tenant_id)
        .bind(post_id)
        .fetch_optional(&db.reader)
        .await
}

pub struct LeadUpsert<'a> {
    pub tenant_id: &'a str,
    pub post_id: &'a str,
    pub title: Option<&'a str>,
    pub language: Option<&'a str>,
    pub source_id: Option<&'a str>,
    pub community: Option<&'a str>,
    pub source_url: Option<&'a str>,
    pub author: Option<&'a str>,
    pub published_at: Option<&'a str>,
    pub discovered_at: &'a str,
    pub vertical_primary: &'a str,
    pub vertical_secondary: &'a [String],
    pub intent: &'a str,
    pub customer_type: &'a str,
    pub buying_intent: &'a str,
    pub business_potential: &'a str,
    pub product_fit: &'a str,
    pub confidence: Option<f64>,
    pub evidence: &'a [Evidence],
    pub score: i64,
    pub score_category: &'a str,
    pub qualification_status: &'a str,
}

/// One active lead per tenant and post: the unique constraint keeps re-analysis from duplicating it.
pub async fn upsert_lead(db: &Db, input: &LeadUpsert<'_>) -> Result<String, sqlx::Error> {
    let now = now_iso();
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM leads WHERE tenant_id = ? AND post_id = ?")
            .bind(input.tenant_id)
            .bind(input.post_id)
            .fetch_optional(&db.reader)
            .await?;

    match existing {
        Some(id) => {
            sqlx::query(
                "UPDATE leads SET title = ?, language = ?, source_id = ?, community = ?, \
                 source_url = ?, author = ?, published_at = ?, vertical_primary = ?, \
                 vertical_secondary = ?, intent = ?, customer_type = ?, buying_intent = ?, \
                 business_potential = ?, product_fit = ?, confidence = ?, evidence = ?, score = ?, \
                 score_category = ?, qualification_status = ?, review_status = \
                 CASE WHEN review_status = 'none' THEN 'pending' ELSE review_status END, \
                 is_active = 1, analysis_count = analysis_count + 1, updated_at = ? \
                 WHERE tenant_id = ? AND id = ?",
            )
            .bind(input.title)
            .bind(input.language)
            .bind(input.source_id)
            .bind(input.community)
            .bind(input.source_url)
            .bind(input.author)
            .bind(input.published_at)
            .bind(input.vertical_primary)
            .bind(to_json(&input.vertical_secondary))
            .bind(input.intent)
            .bind(input.customer_type)
            .bind(input.buying_intent)
            .bind(input.business_potential)
            .bind(input.product_fit)
            .bind(input.confidence)
            .bind(to_json(&input.evidence))
            .bind(input.score)
            .bind(input.score_category)
            .bind(input.qualification_status)
            .bind(&now)
            .bind(input.tenant_id)
            .bind(&id)
            .execute(&db.writer)
            .await?;
            Ok(id)
        }
        None => {
            let id = new_id();
            sqlx::query(
                "INSERT INTO leads (id, tenant_id, post_id, title, language, source_id, community, \
                 source_url, author, published_at, discovered_at, vertical_primary, \
                 vertical_secondary, intent, customer_type, buying_intent, business_potential, \
                 product_fit, confidence, evidence, score, score_category, qualification_status, \
                 review_status, is_active, analysis_count, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                 'pending', 1, 1, ?, ?)",
            )
            .bind(&id)
            .bind(input.tenant_id)
            .bind(input.post_id)
            .bind(input.title)
            .bind(input.language)
            .bind(input.source_id)
            .bind(input.community)
            .bind(input.source_url)
            .bind(input.author)
            .bind(input.published_at)
            .bind(input.discovered_at)
            .bind(input.vertical_primary)
            .bind(to_json(&input.vertical_secondary))
            .bind(input.intent)
            .bind(input.customer_type)
            .bind(input.buying_intent)
            .bind(input.business_potential)
            .bind(input.product_fit)
            .bind(input.confidence)
            .bind(to_json(&input.evidence))
            .bind(input.score)
            .bind(input.score_category)
            .bind(input.qualification_status)
            .bind(&now)
            .bind(&now)
            .execute(&db.writer)
            .await?;
            Ok(id)
        }
    }
}

pub struct ScoreHistoryInsert<'a> {
    pub lead_id: &'a str,
    pub score_version: i64,
    pub score: i64,
    pub category: &'a str,
    pub components: &'a Value,
    pub inputs: &'a Value,
    pub decision_run_id: Option<&'a str>,
}

pub async fn insert_score_history(
    db: &Db,
    input: &ScoreHistoryInsert<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO lead_scores (id, lead_id, score_version, score, category, components, inputs, \
         decision_run_id, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(input.lead_id)
    .bind(input.score_version)
    .bind(input.score)
    .bind(input.category)
    .bind(to_json(input.components))
    .bind(to_json(input.inputs))
    .bind(input.decision_run_id)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, FromRow)]
pub struct ScoreHistoryRow {
    pub id: String,
    pub lead_id: String,
    pub score_version: i64,
    pub score: i64,
    pub category: String,
    pub components: String,
    pub inputs: String,
    pub decision_run_id: Option<String>,
    pub created_at: String,
}

pub async fn score_history(
    db: &Db,
    lead_id: &str,
    limit: i64,
) -> Result<Vec<ScoreHistoryRow>, sqlx::Error> {
    sqlx::query_as::<_, ScoreHistoryRow>(
        "SELECT * FROM lead_scores WHERE lead_id = ? ORDER BY created_at DESC LIMIT ?",
    )
    .bind(lead_id)
    .bind(limit)
    .fetch_all(&db.reader)
    .await
}

pub async fn set_review_status(
    db: &Db,
    tenant_id: &str,
    lead_id: &str,
    status: &str,
) -> Result<bool, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE leads SET review_status = ?, updated_at = ? WHERE tenant_id = ? AND id = ?",
    )
    .bind(status)
    .bind(now_iso())
    .bind(tenant_id)
    .bind(lead_id)
    .execute(&db.writer)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// Aggregate counts used by the dashboard and the inbox facets.
pub async fn lead_counts_by_vertical(
    db: &Db,
    tenant_id: &str,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT vertical_primary, COUNT(*) FROM leads WHERE tenant_id = ? AND is_active = 1 \
         GROUP BY vertical_primary ORDER BY COUNT(*) DESC",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await
}

pub async fn lead_counts_by_intent(
    db: &Db,
    tenant_id: &str,
) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT intent, COUNT(*) FROM leads WHERE tenant_id = ? AND is_active = 1 \
         GROUP BY intent ORDER BY COUNT(*) DESC",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RescoreOutcome {
    pub examined: usize,
    pub updated: usize,
    pub skipped: usize,
    pub version: i64,
}

/// Recompute score and qualification for every active lead of a tenant from the decisions already
/// stored, using the active scoring configuration. No provider is called: changing weights or
/// thresholds never requires paying for the same analysis twice.
pub async fn rescore_tenant(
    db: &Db,
    tenant_id: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<RescoreOutcome, sqlx::Error> {
    let config = active_scoring_config(db, tenant_id).await?;
    #[derive(sqlx::FromRow)]
    struct Row {
        id: String,
        vertical_primary: String,
        intent: String,
        customer_type: String,
        buying_intent: String,
        business_potential: String,
        product_fit: String,
        confidence: Option<f64>,
        evidence: String,
        published_at: Option<String>,
        score: Option<i64>,
        engagement_comments: Option<i64>,
        engagement_upvotes: Option<i64>,
        engagement_shares: Option<i64>,
        decision_run_id: Option<String>,
        relevance: Option<f64>,
        evidence_sufficient: Option<f64>,
        too_ambiguous: Option<f64>,
        needs_human_review: Option<f64>,
    }

    let rows = sqlx::query_as::<_, Row>(
        "WITH latest AS ( \
           SELECT r.id, r.post_id, \
                  ROW_NUMBER() OVER (PARTITION BY r.post_id ORDER BY r.created_at DESC) AS rn \
           FROM decision_runs r WHERE r.tenant_id = ? AND r.status = 'succeeded' \
         ) \
         SELECT l.id, l.vertical_primary, l.intent, l.customer_type, l.buying_intent, \
                l.business_potential, l.product_fit, l.confidence, l.evidence, l.published_at, l.score, \
                p.engagement_comments, p.engagement_upvotes, p.engagement_shares, \
                latest.id AS decision_run_id, \
                (SELECT a.noul FROM decision_answers a WHERE a.decision_run_id = latest.id \
                   AND a.question = 'relevance') AS relevance, \
                (SELECT a.noul FROM decision_answers a WHERE a.decision_run_id = latest.id \
                   AND a.question = 'evidence_sufficient') AS evidence_sufficient, \
                (SELECT a.noul FROM decision_answers a WHERE a.decision_run_id = latest.id \
                   AND a.question = 'too_ambiguous') AS too_ambiguous, \
                (SELECT a.noul FROM decision_answers a WHERE a.decision_run_id = latest.id \
                   AND a.question = 'needs_human_review') AS needs_human_review \
         FROM leads l \
         JOIN discovered_posts p ON p.id = l.post_id \
         LEFT JOIN latest ON latest.post_id = l.post_id AND latest.rn = 1 \
         WHERE l.tenant_id = ? AND l.is_active = 1 \
         ORDER BY l.id",
    )
    .bind(tenant_id)
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await?;

    let mut outcome = RescoreOutcome {
        version: config.version,
        ..Default::default()
    };
    for row in rows {
        outcome.examined += 1;
        let Some(relevance) = row.relevance else {
            // Without a stored decision there is nothing to recompute; the lead stays as it is.
            outcome.skipped += 1;
            continue;
        };
        let vertical = Vertical::parse(&row.vertical_primary).unwrap_or(Vertical::Other);
        let intent = Intent::parse(&row.intent).unwrap_or(Intent::Irrelevant);
        let customer_type =
            CustomerType::parse(&row.customer_type).unwrap_or(CustomerType::Unknown);
        let buying_intent = BuyingIntent::parse(&row.buying_intent).unwrap_or(BuyingIntent::None);
        let business_potential =
            BusinessPotential::parse(&row.business_potential).unwrap_or(BusinessPotential::Low);
        let product_fit = ProductFit::parse(&row.product_fit).unwrap_or(ProductFit::Poor);
        let evidence: Vec<crate::domain::evidence::Evidence> =
            from_json_or(&row.evidence, Vec::new());
        let engagement = match (
            row.engagement_comments,
            row.engagement_upvotes,
            row.engagement_shares,
        ) {
            (None, None, None) => None,
            (comments, upvotes, shares) => Some(crate::domain::scoring::EngagementCounts {
                comments: comments.unwrap_or(0),
                upvotes: upvotes.unwrap_or(0),
                shares: shares.unwrap_or(0),
            }),
        };
        let inputs = crate::domain::scoring::ScoreInputs {
            relevance_probability: relevance,
            buying_intent,
            business_potential,
            product_fit,
            published_at: row.published_at.clone(),
            engagement,
        };
        let scored = crate::domain::scoring::score(&config, &inputs, now);
        let confident = row.confidence.unwrap_or(0.0);
        let qualification = qualify(&crate::domain::qualification::QualificationInput {
            relevance,
            intent,
            customer_type,
            evidence_sufficient: row.evidence_sufficient.unwrap_or(0.0) > 0.5
                && !evidence.is_empty(),
            ambiguous: row.too_ambiguous.unwrap_or(0.0) > 0.5
                || row.needs_human_review.unwrap_or(0.0) > 0.5,
            confidence: confident,
            vertical,
            score: scored.score,
            thresholds: config.thresholds.clone(),
        });
        sqlx::query(
            "UPDATE leads SET score = ?, score_category = ?, qualification_status = ?, updated_at = ? \
             WHERE tenant_id = ? AND id = ?",
        )
        .bind(scored.score)
        .bind(scored.category.as_str())
        .bind(qualification.status().as_str())
        .bind(crate::domain::time::now_iso())
        .bind(tenant_id)
        .bind(&row.id)
        .execute(&db.writer)
        .await?;
        let components = serde_json::to_value(&scored.components).unwrap_or(Value::Null);
        let score_inputs = serde_json::to_value(&inputs).unwrap_or(Value::Null);
        insert_score_history(
            db,
            &ScoreHistoryInsert {
                lead_id: &row.id,
                score_version: config.version,
                score: scored.score,
                category: scored.category.as_str(),
                components: &components,
                inputs: &score_inputs,
                decision_run_id: row.decision_run_id.as_deref(),
            },
        )
        .await?;
        if row.score != Some(scored.score) {
            outcome.updated += 1;
        }
    }
    Ok(outcome)
}

#[derive(Debug, Clone, Default)]
pub struct LeadTotals {
    pub opportunities: i64,
    pub new_opportunities: i64,
    pub hot: i64,
    pub warm: i64,
    pub potential: i64,
    pub low: i64,
}

pub async fn lead_totals(db: &Db, tenant_id: &str, since: &str) -> Result<LeadTotals, sqlx::Error> {
    let opportunities: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM leads WHERE tenant_id = ? AND is_active = 1")
            .bind(tenant_id)
            .fetch_one(&db.reader)
            .await?;
    let new_opportunities: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM leads WHERE tenant_id = ? AND is_active = 1 AND created_at >= ?",
    )
    .bind(tenant_id)
    .bind(since)
    .fetch_one(&db.reader)
    .await?;
    let mut totals = LeadTotals {
        opportunities,
        new_opportunities,
        ..Default::default()
    };
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT COALESCE(score_category, 'low'), COUNT(*) FROM leads \
         WHERE tenant_id = ? AND is_active = 1 GROUP BY score_category",
    )
    .bind(tenant_id)
    .fetch_all(&db.reader)
    .await?;
    for (category, count) in rows {
        match category.as_str() {
            "hot" => totals.hot = count,
            "warm" => totals.warm = count,
            "potential" => totals.potential = count,
            _ => totals.low = count,
        }
    }
    Ok(totals)
}

/// Parse the stored JSON columns of a lead row for the API.
pub fn lead_evidence(row: &LeadRow) -> Vec<Evidence> {
    from_json_or(&row.evidence, Vec::new())
}

pub fn lead_secondary_verticals(row: &LeadRow) -> Vec<String> {
    from_json_or(&row.vertical_secondary, Vec::new())
}

/// Documented default configuration, exposed to settings endpoints.
pub fn default_config_json() -> Value {
    let config = ScoringConfig::default_v1();
    json!({
        "version": config.version,
        "weights": config.weights,
        "bands": config.bands,
        "thresholds": config.thresholds,
    })
}
