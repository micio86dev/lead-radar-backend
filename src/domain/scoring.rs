//! Deterministic lead scoring. No model controls this number: providers only supply the inputs.
//!
//! Weights, bands and vertical thresholds are versioned configuration (`ScoringConfig`). Missing
//! inputs are dropped from the denominator and reported in `ScoreResult::missing`; they are never
//! replaced by invented values. Each signal is counted exactly once, which is why buying intent and
//! business potential are separate dimensions rather than derived from each other.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::enums::{BusinessPotential, BuyingIntent, ProductFit, ScoreCategory, Vertical};
use crate::domain::time::hours_since;

pub const DIM_PROBLEM_RELEVANCE: &str = "problem_relevance";
pub const DIM_BUYING_INTENT: &str = "buying_intent";
pub const DIM_BUSINESS_POTENTIAL: &str = "business_potential";
pub const DIM_PRODUCT_FIT: &str = "product_fit";
pub const DIM_RECENCY: &str = "recency";
pub const DIM_ENGAGEMENT: &str = "engagement";

/// Recency decays linearly to zero after 30 days.
const RECENCY_HORIZON_HOURS: f64 = 720.0;
/// Engagement saturates logarithmically: 200 weighted interactions is "fully engaged".
const ENGAGEMENT_SATURATION: f64 = 200.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ScoringWeights {
    pub problem_relevance: f64,
    pub buying_intent: f64,
    pub business_potential: f64,
    pub product_fit: f64,
    pub recency: f64,
    pub engagement: f64,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            problem_relevance: 25.0,
            buying_intent: 25.0,
            business_potential: 20.0,
            product_fit: 15.0,
            recency: 10.0,
            engagement: 5.0,
        }
    }
}

impl ScoringWeights {
    pub fn total(&self) -> f64 {
        self.problem_relevance
            + self.buying_intent
            + self.business_potential
            + self.product_fit
            + self.recency
            + self.engagement
    }

    pub fn as_map(&self) -> BTreeMap<String, f64> {
        BTreeMap::from([
            (DIM_PROBLEM_RELEVANCE.to_string(), self.problem_relevance),
            (DIM_BUYING_INTENT.to_string(), self.buying_intent),
            (DIM_BUSINESS_POTENTIAL.to_string(), self.business_potential),
            (DIM_PRODUCT_FIT.to_string(), self.product_fit),
            (DIM_RECENCY.to_string(), self.recency),
            (DIM_ENGAGEMENT.to_string(), self.engagement),
        ])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoreBands {
    pub hot_min: i64,
    pub warm_min: i64,
    pub potential_min: i64,
}

impl Default for ScoreBands {
    fn default() -> Self {
        Self {
            hot_min: 80,
            warm_min: 60,
            potential_min: 40,
        }
    }
}

impl ScoreBands {
    pub fn category(&self, score: i64) -> ScoreCategory {
        if score >= self.hot_min {
            ScoreCategory::Hot
        } else if score >= self.warm_min {
            ScoreCategory::Warm
        } else if score >= self.potential_min {
            ScoreCategory::Potential
        } else {
            ScoreCategory::Low
        }
    }
}

/// Minimum score per vertical before a lead can be qualified. `other` is not a target vertical and
/// uses the lowest configured threshold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualificationThresholds {
    pub per_vertical: BTreeMap<String, i64>,
}

impl Default for QualificationThresholds {
    fn default() -> Self {
        Self {
            per_vertical: BTreeMap::from([
                (Vertical::HrRecruiting.as_str().to_string(), 65),
                (Vertical::InternationalMeetings.as_str().to_string(), 60),
                (Vertical::ConferencesWebinars.as_str().to_string(), 60),
                (Vertical::CustomerSupport.as_str().to_string(), 65),
                (Vertical::TourismHospitality.as_str().to_string(), 65),
                (Vertical::Other.as_str().to_string(), 60),
            ]),
        }
    }
}

impl QualificationThresholds {
    pub fn for_vertical(&self, vertical: Vertical) -> i64 {
        self.per_vertical
            .get(vertical.as_str())
            .copied()
            .unwrap_or(60)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoringConfig {
    pub version: i64,
    pub weights: ScoringWeights,
    pub bands: ScoreBands,
    pub thresholds: QualificationThresholds,
}

impl Default for ScoringConfig {
    fn default() -> Self {
        Self::default_v1()
    }
}

impl ScoringConfig {
    pub fn default_v1() -> Self {
        Self {
            version: 1,
            weights: ScoringWeights::default(),
            bands: ScoreBands::default(),
            thresholds: QualificationThresholds::default(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.version < 1 {
            return Err("score version must be >= 1".to_string());
        }
        let weights = self.weights.as_map();
        for (dimension, weight) in &weights {
            if !weight.is_finite() || *weight < 0.0 {
                return Err(format!("{dimension} weight must be a non-negative number"));
            }
        }
        if (self.weights.total() - 100.0).abs() > 0.001 {
            return Err(format!(
                "weights must total 100 (got {})",
                self.weights.total()
            ));
        }
        let bands = self.bands;
        if !(bands.hot_min > bands.warm_min
            && bands.warm_min > bands.potential_min
            && bands.potential_min > 0
            && bands.hot_min <= 100)
        {
            return Err("score bands must be strictly descending within 0..100".to_string());
        }
        for (vertical, threshold) in &self.thresholds.per_vertical {
            if !(0..=100).contains(threshold) {
                return Err(format!("{vertical} threshold must be within 0..100"));
            }
        }
        Ok(())
    }

    pub fn threshold_for(&self, vertical: Vertical) -> i64 {
        self.thresholds.for_vertical(vertical)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EngagementCounts {
    pub comments: i64,
    pub upvotes: i64,
    pub shares: i64,
}

impl EngagementCounts {
    /// Weighted, monotonically increasing engagement. Shares count more than upvotes; comments are
    /// the strongest signal that a conversation is live.
    pub fn weighted(&self) -> i64 {
        self.comments.saturating_mul(2) + self.upvotes + self.shares.saturating_mul(3)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreInputs {
    pub relevance_probability: f64,
    pub buying_intent: BuyingIntent,
    pub business_potential: BusinessPotential,
    pub product_fit: ProductFit,
    pub published_at: Option<String>,
    pub engagement: Option<EngagementCounts>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreComponent {
    pub dimension: String,
    pub weight: f64,
    pub raw: Option<f64>,
    pub normalized: Option<f64>,
    pub contribution: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScoreResult {
    pub score: i64,
    pub category: ScoreCategory,
    pub components: Vec<ScoreComponent>,
    pub missing: Vec<String>,
    pub used_weight: f64,
}

/// Compute the deterministic score. The result is always within `0..=100`.
pub fn score(config: &ScoringConfig, inputs: &ScoreInputs, now: DateTime<Utc>) -> ScoreResult {
    let weights = config.weights;
    let relevance = inputs.relevance_probability.clamp(0.0, 1.0);
    let recency_hours = hours_since(inputs.published_at.as_deref(), now);
    let recency = recency_hours.map(|hours| (1.0 - hours / RECENCY_HORIZON_HOURS).clamp(0.0, 1.0));
    let engagement = inputs.engagement.map(|counts| {
        ((counts.weighted() as f64 + 1.0).ln() / (ENGAGEMENT_SATURATION + 1.0).ln()).clamp(0.0, 1.0)
    });

    let mut components = Vec::with_capacity(6);
    let mut missing = Vec::new();
    let mut used_weight = 0.0;
    let mut total = 0.0;

    let mut push = |dimension: &str,
                    weight: f64,
                    raw: Option<f64>,
                    normalized: Option<f64>,
                    missing_list: &mut Vec<String>,
                    total: &mut f64,
                    used_weight: &mut f64| {
        let contribution = normalized.map(|value| value * weight).unwrap_or(0.0);
        if normalized.is_some() {
            *total += contribution;
            *used_weight += weight;
        } else {
            missing_list.push(dimension.to_string());
        }
        components.push(ScoreComponent {
            dimension: dimension.to_string(),
            weight,
            raw,
            normalized,
            contribution: if normalized.is_some() {
                contribution
            } else {
                0.0
            },
        });
    };

    push(
        DIM_PROBLEM_RELEVANCE,
        weights.problem_relevance,
        Some(relevance),
        Some(relevance),
        &mut missing,
        &mut total,
        &mut used_weight,
    );
    push(
        DIM_BUYING_INTENT,
        weights.buying_intent,
        Some(inputs.buying_intent.weight()),
        Some(inputs.buying_intent.weight()),
        &mut missing,
        &mut total,
        &mut used_weight,
    );
    push(
        DIM_BUSINESS_POTENTIAL,
        weights.business_potential,
        Some(inputs.business_potential.weight()),
        Some(inputs.business_potential.weight()),
        &mut missing,
        &mut total,
        &mut used_weight,
    );
    push(
        DIM_PRODUCT_FIT,
        weights.product_fit,
        Some(inputs.product_fit.weight()),
        Some(inputs.product_fit.weight()),
        &mut missing,
        &mut total,
        &mut used_weight,
    );
    push(
        DIM_RECENCY,
        weights.recency,
        recency_hours,
        recency,
        &mut missing,
        &mut total,
        &mut used_weight,
    );
    push(
        DIM_ENGAGEMENT,
        weights.engagement,
        inputs.engagement.map(|counts| counts.weighted() as f64),
        engagement,
        &mut missing,
        &mut total,
        &mut used_weight,
    );

    let score = if used_weight > 0.0 {
        ((total / used_weight) * 100.0).round().clamp(0.0, 100.0) as i64
    } else {
        0
    };

    ScoreResult {
        score,
        category: config.bands.category(score),
        components,
        missing,
        used_weight,
    }
}

/// Score a category from a stored configuration; used when re-reading history.
pub fn category_for(config: &ScoringConfig, score: i64) -> ScoreCategory {
    config.bands.category(score)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::time::parse_iso;

    fn at(reference: &str) -> DateTime<Utc> {
        parse_iso(reference).expect("valid timestamp")
    }

    fn inputs() -> ScoreInputs {
        ScoreInputs {
            relevance_probability: 1.0,
            buying_intent: BuyingIntent::Explicit,
            business_potential: BusinessPotential::VeryHigh,
            product_fit: ProductFit::Excellent,
            published_at: Some("2024-05-01T10:00:00Z".to_string()),
            engagement: Some(EngagementCounts {
                comments: 100,
                upvotes: 600,
                shares: 200,
            }),
        }
    }

    #[test]
    fn default_configuration_matches_the_product_spec() {
        let config = ScoringConfig::default_v1();
        config.validate().expect("valid");
        assert_eq!(config.weights.problem_relevance, 25.0);
        assert_eq!(config.weights.buying_intent, 25.0);
        assert_eq!(config.weights.business_potential, 20.0);
        assert_eq!(config.weights.product_fit, 15.0);
        assert_eq!(config.weights.recency, 10.0);
        assert_eq!(config.weights.engagement, 5.0);
        assert_eq!(config.weights.total(), 100.0);
        assert_eq!(config.threshold_for(Vertical::InternationalMeetings), 60);
        assert_eq!(config.threshold_for(Vertical::ConferencesWebinars), 60);
        assert_eq!(config.threshold_for(Vertical::HrRecruiting), 65);
        assert_eq!(config.threshold_for(Vertical::CustomerSupport), 65);
        assert_eq!(config.threshold_for(Vertical::TourismHospitality), 65);
    }

    #[test]
    fn strongest_possible_signal_scores_100_hot() {
        let config = ScoringConfig::default_v1();
        let result = score(&config, &inputs(), at("2024-05-01T10:00:00Z"));
        assert_eq!(result.score, 100);
        assert_eq!(result.category, ScoreCategory::Hot);
        assert!(result.missing.is_empty());
        assert_eq!(result.used_weight, 100.0);
    }

    #[test]
    fn weakest_signal_scores_only_the_low_potential_weight() {
        let config = ScoringConfig::default_v1();
        let result = score(
            &config,
            &ScoreInputs {
                relevance_probability: 0.0,
                buying_intent: BuyingIntent::None,
                business_potential: BusinessPotential::Low,
                product_fit: ProductFit::Poor,
                published_at: Some("2024-01-01T10:00:00Z".to_string()),
                engagement: Some(EngagementCounts::default()),
            },
            at("2024-05-01T10:00:00Z"),
        );
        // business_potential:low = 0.25 * 20 = 5 points out of 100.
        assert_eq!(result.score, 5);
        assert_eq!(result.category, ScoreCategory::Low);
    }

    #[test]
    fn missing_engagement_is_reported_not_invented() {
        let config = ScoringConfig::default_v1();
        let mut input = inputs();
        input.engagement = None;
        input.relevance_probability = 1.0;
        input.buying_intent = BuyingIntent::None;
        input.business_potential = BusinessPotential::Low;
        input.product_fit = ProductFit::Poor;
        input.published_at = Some("2024-01-01T10:00:00Z".to_string());
        let result = score(&config, &input, at("2024-05-01T10:00:00Z"));
        assert!(result.missing.contains(&DIM_ENGAGEMENT.to_string()));
        assert_eq!(result.used_weight, 95.0);
        // (25 * 1 + 20 * 0.25) / 95 * 100 = 31.57 -> 32
        assert_eq!(result.score, 32);
        assert_eq!(
            result
                .components
                .iter()
                .find(|component| component.dimension == DIM_ENGAGEMENT)
                .map(|component| component.contribution),
            Some(0.0)
        );
    }

    #[test]
    fn missing_publication_time_drops_recency_instead_of_guessing() {
        let config = ScoringConfig::default_v1();
        let mut input = inputs();
        input.published_at = None;
        let result = score(&config, &input, at("2024-05-01T10:00:00Z"));
        assert!(result.missing.contains(&DIM_RECENCY.to_string()));
        assert_eq!(result.used_weight, 90.0);
        assert!(result.score <= 100);
    }

    #[test]
    fn recency_decays_over_thirty_days() {
        let config = ScoringConfig::default_v1();
        let mut input = inputs();
        input.engagement = None;
        input.published_at = Some("2024-04-01T10:00:00Z".to_string());
        let fresh = score(&config, &input, at("2024-04-01T10:00:00Z"));
        let stale = score(&config, &input, at("2024-04-16T10:00:00Z"));
        let expired = score(&config, &input, at("2024-07-01T10:00:00Z"));
        assert!(fresh.score > stale.score);
        assert!(stale.score > expired.score);
        let recency = |result: &ScoreResult| {
            result
                .components
                .iter()
                .find(|component| component.dimension == DIM_RECENCY)
                .and_then(|component| component.normalized)
                .expect("recency")
        };
        assert_eq!(recency(&expired), 0.0);
        assert!((recency(&stale) - 0.5).abs() < 0.01);
    }

    #[test]
    fn engagement_saturates_instead_of_scaling_without_bound() {
        let config = ScoringConfig::default_v1();
        let mut input = inputs();
        input.published_at = None;
        let none = score(
            &config,
            &ScoreInputs {
                engagement: Some(EngagementCounts::default()),
                ..input.clone()
            },
            at("2024-05-01T10:00:00Z"),
        );
        let some = score(
            &config,
            &ScoreInputs {
                engagement: Some(EngagementCounts {
                    comments: 5,
                    upvotes: 10,
                    shares: 1,
                }),
                ..input.clone()
            },
            at("2024-05-01T10:00:00Z"),
        );
        let huge = score(
            &config,
            &ScoreInputs {
                engagement: Some(EngagementCounts {
                    comments: 1_000_000,
                    upvotes: 1_000_000,
                    shares: 1_000_000,
                }),
                ..input
            },
            at("2024-05-01T10:00:00Z"),
        );
        assert!(some.score > none.score);
        assert!(huge.score > some.score);
        assert!(huge.score <= 100);
    }

    #[test]
    fn configuration_validation_rejects_nonsense() {
        let mut config = ScoringConfig::default_v1();
        config.weights.problem_relevance = 40.0;
        assert!(config.validate().is_err());

        let mut config = ScoringConfig::default_v1();
        config.bands = ScoreBands {
            hot_min: 60,
            warm_min: 80,
            potential_min: 40,
        };
        assert!(config.validate().is_err());

        let mut config = ScoringConfig::default_v1();
        config
            .thresholds
            .per_vertical
            .insert("other".to_string(), 101);
        assert!(config.validate().is_err());
    }
}
