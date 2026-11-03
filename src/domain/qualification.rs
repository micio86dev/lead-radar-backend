//! Qualification gates. A discovered post is not a lead: relevance, a commercial signal, an
//! addressable customer type, enough score for the vertical and usable evidence all have to hold.
//! Anything uncertain is routed to human review instead of being silently accepted.

use serde::{Deserialize, Serialize};

use crate::domain::enums::{CustomerType, Intent, QualificationStatus, Vertical};
use crate::domain::scoring::QualificationThresholds;

pub const MIN_RELEVANCE: f64 = 0.35;
pub const MIN_CONFIDENCE: f64 = 0.55;
/// How far below the vertical threshold a score may sit and still be recoverable by a human.
pub const NEAR_THRESHOLD_MARGIN: i64 = 10;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualificationInput {
    pub relevance: f64,
    pub intent: Intent,
    pub customer_type: CustomerType,
    pub evidence_sufficient: bool,
    pub ambiguous: bool,
    pub confidence: f64,
    pub vertical: Vertical,
    pub score: i64,
    pub thresholds: QualificationThresholds,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QualificationOutcome {
    Qualified,
    NeedsReview { reasons: Vec<String> },
    Rejected { reasons: Vec<String> },
}

impl QualificationOutcome {
    pub fn status(&self) -> QualificationStatus {
        match self {
            Self::Qualified => QualificationStatus::Qualified,
            Self::NeedsReview { .. } => QualificationStatus::NeedsReview,
            Self::Rejected { .. } => QualificationStatus::Rejected,
        }
    }

    pub fn reasons(&self) -> &[String] {
        match self {
            Self::Qualified => &[],
            Self::NeedsReview { reasons } | Self::Rejected { reasons } => reasons,
        }
    }
}

pub fn qualify(input: &QualificationInput) -> QualificationOutcome {
    let mut rejections = Vec::new();

    if input.relevance < MIN_RELEVANCE {
        rejections.push("relevance_below_minimum".to_string());
    }
    if input.intent == Intent::Irrelevant {
        rejections.push("intent_irrelevant".to_string());
    } else if !input.intent.is_commercial_signal() {
        rejections.push("no_commercial_signal".to_string());
    }
    if input.customer_type == CustomerType::B2c {
        rejections.push("consumer_audience".to_string());
    }

    let threshold = input.thresholds.for_vertical(input.vertical);
    if input.score < threshold - NEAR_THRESHOLD_MARGIN {
        rejections.push("score_below_vertical_threshold".to_string());
    }

    if !rejections.is_empty() {
        return QualificationOutcome::Rejected {
            reasons: rejections,
        };
    }

    let mut review = Vec::new();
    if input.ambiguous {
        review.push("ambiguous_evidence".to_string());
    }
    if !input.evidence_sufficient {
        review.push("insufficient_evidence".to_string());
    }
    if input.confidence < MIN_CONFIDENCE {
        review.push("low_confidence".to_string());
    }
    if input.customer_type == CustomerType::Unknown {
        review.push("customer_type_unknown".to_string());
    }
    if input.intent == Intent::LatentProblem {
        review.push("latent_problem_requires_review".to_string());
    }
    if input.intent == Intent::ProductComparison {
        review.push("competitor_comparison".to_string());
    }
    if input.score < threshold {
        review.push("score_near_vertical_threshold".to_string());
    }

    if review.is_empty() {
        QualificationOutcome::Qualified
    } else {
        QualificationOutcome::NeedsReview { reasons: review }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> QualificationInput {
        QualificationInput {
            relevance: 0.9,
            intent: Intent::ActiveProblem,
            customer_type: CustomerType::B2b,
            evidence_sufficient: true,
            ambiguous: false,
            confidence: 0.82,
            vertical: Vertical::HrRecruiting,
            score: 78,
            thresholds: QualificationThresholds::default(),
        }
    }

    #[test]
    fn strong_b2b_signal_is_qualified() {
        assert_eq!(qualify(&input()), QualificationOutcome::Qualified);
    }

    #[test]
    fn score_above_the_vertical_threshold_is_required() {
        let mut candidate = input();
        candidate.vertical = Vertical::CustomerSupport;
        candidate.score = 66;
        assert_eq!(qualify(&candidate), QualificationOutcome::Qualified);

        candidate.score = 64;
        assert_eq!(
            qualify(&candidate),
            QualificationOutcome::NeedsReview {
                reasons: vec!["score_near_vertical_threshold".to_string()]
            }
        );

        candidate.score = 54;
        assert_eq!(
            qualify(&candidate),
            QualificationOutcome::Rejected {
                reasons: vec!["score_below_vertical_threshold".to_string()]
            }
        );
    }

    #[test]
    fn consumer_audience_and_noise_are_rejected() {
        let mut candidate = input();
        candidate.customer_type = CustomerType::B2c;
        assert_eq!(
            qualify(&candidate),
            QualificationOutcome::Rejected {
                reasons: vec!["consumer_audience".to_string()]
            }
        );

        let mut candidate = input();
        candidate.intent = Intent::Irrelevant;
        candidate.relevance = 0.1;
        assert_eq!(
            qualify(&candidate),
            QualificationOutcome::Rejected {
                reasons: vec![
                    "relevance_below_minimum".to_string(),
                    "intent_irrelevant".to_string()
                ]
            }
        );

        let mut candidate = input();
        candidate.intent = Intent::GeneralDiscussion;
        assert_eq!(
            qualify(&candidate),
            QualificationOutcome::Rejected {
                reasons: vec!["no_commercial_signal".to_string()]
            }
        );
    }

    #[test]
    fn uncertainty_is_routed_to_human_review() {
        let mut candidate = input();
        candidate.ambiguous = true;
        candidate.confidence = 0.4;
        candidate.customer_type = CustomerType::Unknown;
        candidate.evidence_sufficient = false;
        assert_eq!(
            qualify(&candidate),
            QualificationOutcome::NeedsReview {
                reasons: vec![
                    "ambiguous_evidence".to_string(),
                    "insufficient_evidence".to_string(),
                    "low_confidence".to_string(),
                    "customer_type_unknown".to_string(),
                ]
            }
        );
        assert_eq!(
            qualify(&candidate).status(),
            QualificationStatus::NeedsReview
        );
    }

    #[test]
    fn latent_problems_always_reach_a_human() {
        let mut candidate = input();
        candidate.intent = Intent::LatentProblem;
        assert_eq!(
            qualify(&candidate),
            QualificationOutcome::NeedsReview {
                reasons: vec!["latent_problem_requires_review".to_string()]
            }
        );
    }
}
