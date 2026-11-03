//! The vocabulary shared by the API, the database and the providers.
//!
//! Every value has exactly one serialized spelling, so the frontend contract, the stored rows and
//! the provider labels cannot drift apart.

use serde::{Deserialize, Serialize};

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(&self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }

            pub fn parse(raw: &str) -> Option<Self> {
                match raw.trim() {
                    $($text => Some($name::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            fn from_str(raw: &str) -> Result<Self, Self::Err> {
                Self::parse(raw).ok_or_else(|| {
                    format!("unknown {} value {:?}", stringify!($name), raw)
                })
            }
        }
    };
}

string_enum!(Vertical {
    HrRecruiting => "hr_recruiting",
    InternationalMeetings => "international_meetings",
    ConferencesWebinars => "conferences_webinars",
    CustomerSupport => "customer_support",
    TourismHospitality => "tourism_hospitality",
    Other => "other",
});

string_enum!(Intent {
    ExplicitSolutionRequest => "explicit_solution_request",
    ActiveProblem => "active_problem",
    LatentProblem => "latent_problem",
    ProductComparison => "product_comparison",
    GeneralDiscussion => "general_discussion",
    Irrelevant => "irrelevant",
});

string_enum!(CustomerType {
    B2b => "b2b",
    B2c => "b2c",
    Unknown => "unknown",
});

string_enum!(BuyingIntent {
    None => "none",
    Weak => "weak",
    Moderate => "moderate",
    Strong => "strong",
    Explicit => "explicit",
});

string_enum!(BusinessPotential {
    Low => "low",
    Moderate => "moderate",
    High => "high",
    VeryHigh => "very_high",
});

string_enum!(ProductFit {
    Poor => "poor",
    Partial => "partial",
    Good => "good",
    Excellent => "excellent",
});

string_enum!(ScoreCategory {
    Hot => "hot",
    Warm => "warm",
    Potential => "potential",
    Low => "low",
});

string_enum!(QualificationStatus {
    Pending => "pending",
    Qualified => "qualified",
    NeedsReview => "needs_review",
    Rejected => "rejected",
});

string_enum!(ReviewStatus {
    None => "none",
    Pending => "pending",
    InReview => "in_review",
    Resolved => "resolved",
});

string_enum!(CommentVariant {
    Helpful => "helpful",
    SoftPitch => "soft_pitch",
    DirectPitch => "direct_pitch",
});

string_enum!(CommentStatus {
    Draft => "draft",
    PendingReview => "pending_review",
    NeedsEdit => "needs_edit",
    Approved => "approved",
    Rejected => "rejected",
    PublishedManually => "published_manually",
});

string_enum!(ValidationStatus {
    Pending => "pending",
    Validated => "validated",
    Rejected => "rejected",
});

string_enum!(JobKind {
    Discovery => "discovery",
    ActorRun => "actor_run",
    DatasetIngestion => "dataset_ingestion",
    Normalization => "normalization",
    Deduplication => "deduplication",
    DecisionEvaluation => "decision_evaluation",
    GenerativeAnalysis => "generative_analysis",
    CommentGeneration => "comment_generation",
    BenchmarkExecution => "benchmark_execution",
});

string_enum!(JobStatus {
    Queued => "queued",
    Running => "running",
    Succeeded => "succeeded",
    Failed => "failed",
    DeadLetter => "dead_letter",
    Cancelled => "cancelled",
});

string_enum!(Role {
    Owner => "owner",
    Admin => "admin",
    Reviewer => "reviewer",
    Viewer => "viewer",
});

string_enum!(ProviderName {
    Jev => "jev",
    Laya => "laya",
    Qwen => "qwen",
    Apify => "apify",
});

impl Vertical {
    pub const PRODUCT_VERTICALS: &'static [Vertical] = &[
        Vertical::HrRecruiting,
        Vertical::InternationalMeetings,
        Vertical::ConferencesWebinars,
        Vertical::CustomerSupport,
        Vertical::TourismHospitality,
    ];
}

impl ScoreCategory {
    pub fn from_score(score: i64) -> Self {
        if score >= 80 {
            Self::Hot
        } else if score >= 60 {
            Self::Warm
        } else if score >= 40 {
            Self::Potential
        } else {
            Self::Low
        }
    }
}

impl BuyingIntent {
    /// Position on the 0..1 scale used by the deterministic score.
    pub fn weight(self) -> f64 {
        match self {
            Self::None => 0.0,
            Self::Weak => 0.25,
            Self::Moderate => 0.5,
            Self::Strong => 0.8,
            Self::Explicit => 1.0,
        }
    }
}

impl BusinessPotential {
    pub fn weight(self) -> f64 {
        match self {
            Self::Low => 0.25,
            Self::Moderate => 0.5,
            Self::High => 0.75,
            Self::VeryHigh => 1.0,
        }
    }
}

impl ProductFit {
    pub fn weight(self) -> f64 {
        match self {
            Self::Poor => 0.0,
            Self::Partial => 0.35,
            Self::Good => 0.7,
            Self::Excellent => 1.0,
        }
    }
}

impl Intent {
    /// Intent values that can indicate a commercial need on their own.
    pub fn is_commercial_signal(self) -> bool {
        matches!(
            self,
            Self::ExplicitSolutionRequest
                | Self::ActiveProblem
                | Self::LatentProblem
                | Self::ProductComparison
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serialized_spellings_match_the_api_contract() {
        assert_eq!(Vertical::HrRecruiting.as_str(), "hr_recruiting");
        assert_eq!(CustomerType::B2b.as_str(), "b2b");
        assert_eq!(BusinessPotential::VeryHigh.as_str(), "very_high");
        assert_eq!(
            CommentStatus::PublishedManually.as_str(),
            "published_manually"
        );
        assert_eq!(JobKind::DecisionEvaluation.as_str(), "decision_evaluation");
        assert_eq!(
            serde_json::to_string(&Vertical::CustomerSupport).expect("json"),
            "\"customer_support\""
        );
        assert_eq!(
            serde_json::from_str::<CustomerType>("\"b2c\"").expect("json"),
            CustomerType::B2c
        );
    }

    #[test]
    fn every_value_round_trips_through_its_spelling() {
        for vertical in Vertical::ALL {
            assert_eq!(Vertical::parse(vertical.as_str()), Some(*vertical));
        }
        for intent in Intent::ALL {
            assert_eq!(Intent::parse(intent.as_str()), Some(*intent));
        }
        for status in CommentStatus::ALL {
            assert_eq!(CommentStatus::parse(status.as_str()), Some(*status));
        }
        for kind in JobKind::ALL {
            assert_eq!(JobKind::parse(kind.as_str()), Some(*kind));
        }
        assert!(Vertical::parse("HR").is_none());
    }

    #[test]
    fn score_bands_follow_the_configured_thresholds() {
        assert_eq!(ScoreCategory::from_score(100), ScoreCategory::Hot);
        assert_eq!(ScoreCategory::from_score(80), ScoreCategory::Hot);
        assert_eq!(ScoreCategory::from_score(79), ScoreCategory::Warm);
        assert_eq!(ScoreCategory::from_score(60), ScoreCategory::Warm);
        assert_eq!(ScoreCategory::from_score(59), ScoreCategory::Potential);
        assert_eq!(ScoreCategory::from_score(40), ScoreCategory::Potential);
        assert_eq!(ScoreCategory::from_score(39), ScoreCategory::Low);
        assert_eq!(ScoreCategory::from_score(0), ScoreCategory::Low);
    }
}
