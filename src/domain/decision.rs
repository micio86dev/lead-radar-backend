//! Typed decision contract, shared by every `DecisionProvider`.
//!
//! The wire protocol is the documented TypeSafe/Jev system-one contract: one `state`, up to 20
//! `questions`, `noul`/`choice`/`score` answers with probability distributions. Laya speaks the same
//! protocol, so one set of types covers both and results stay independently auditable.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::enums::Vertical;

/// Provider limit: at most 20 questions per request.
pub const MAX_QUESTIONS: usize = 20;
/// Provider limit: state size.
pub const MAX_STATE_CHARS: usize = 100_000;
/// Provider limit: one instruction, in characters.
pub const MAX_INSTRUCTIONS_CHARS: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionKind {
    Noul,
    Choice,
    Score,
}

impl QuestionKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Noul => "noul",
            Self::Choice => "choice",
            Self::Score => "score",
        }
    }
}

/// `choice` criteria are an object (label -> boundary description), `score` criteria are an array
/// ordered low to high. Swapping them is rejected by the provider, so the type enforces it here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestionCriteria {
    Choice(BTreeMap<String, String>),
    Score(Vec<String>),
}

impl QuestionCriteria {
    pub fn choice(labels: &[(&str, &str)]) -> Self {
        Self::Choice(
            labels
                .iter()
                .map(|(label, description)| (label.to_string(), description.to_string()))
                .collect(),
        )
    }

    pub fn score(labels: &[&str]) -> Self {
        Self::Score(labels.iter().map(|label| label.to_string()).collect())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionQuestion {
    pub id: String,
    pub kind: QuestionKind,
    pub instructions: String,
    pub criteria: Option<QuestionCriteria>,
}

impl DecisionQuestion {
    pub fn noul(id: &str, instructions: &str) -> Self {
        Self {
            id: id.to_string(),
            kind: QuestionKind::Noul,
            instructions: instructions.to_string(),
            criteria: None,
        }
    }

    pub fn choice(id: &str, instructions: &str, criteria: QuestionCriteria) -> Self {
        Self {
            id: id.to_string(),
            kind: QuestionKind::Choice,
            instructions: instructions.to_string(),
            criteria: Some(criteria),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.is_empty() || self.id.len() > 64 {
            return Err("question id must be 1..=64 characters".to_string());
        }
        if self.instructions.is_empty()
            || self.instructions.chars().count() > MAX_INSTRUCTIONS_CHARS
        {
            return Err(format!(
                "question {} instructions must be 1..={MAX_INSTRUCTIONS_CHARS} characters",
                self.id
            ));
        }
        match (self.kind, &self.criteria) {
            (QuestionKind::Noul, None) => Ok(()),
            (QuestionKind::Choice, Some(QuestionCriteria::Choice(labels))) => {
                let descriptions_ok = labels
                    .values()
                    .all(|description| description.chars().count() <= 400);
                if (2..=24).contains(&labels.len()) && descriptions_ok {
                    Ok(())
                } else {
                    Err(format!(
                        "question {} choice criteria must be 2..=24 labels with descriptions",
                        self.id
                    ))
                }
            }
            (QuestionKind::Score, Some(QuestionCriteria::Score(labels))) => {
                if (2..=10).contains(&labels.len()) {
                    Ok(())
                } else {
                    Err(format!(
                        "question {} score criteria must be 2..=10 tier labels",
                        self.id
                    ))
                }
            }
            _ => Err(format!(
                "question {} criteria do not match its type",
                self.id
            )),
        }
    }
}

/// Validate a request before it leaves the process.
pub fn validate_request(state: &str, questions: &[DecisionQuestion]) -> Result<(), String> {
    if state.trim().is_empty() {
        return Err("state must not be empty".to_string());
    }
    if state.chars().count() > MAX_STATE_CHARS {
        return Err(format!("state exceeds {MAX_STATE_CHARS} characters"));
    }
    if questions.is_empty() || questions.len() > MAX_QUESTIONS {
        return Err(format!(
            "between 1 and {MAX_QUESTIONS} questions are allowed per request"
        ));
    }
    for question in questions {
        question.validate()?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionAnswer {
    pub question: String,
    pub kind: QuestionKind,
    /// `noul` probability.
    pub noul: Option<f64>,
    /// `choice` label.
    pub choice: Option<String>,
    /// `score` expected value (a decimal, never rounded before thresholding).
    pub score: Option<f64>,
    pub legend: Option<BTreeMap<String, String>>,
    pub probabilities: Option<BTreeMap<String, f64>>,
    pub confidence: Option<f64>,
}

impl DecisionAnswer {
    /// Confidence for a `noul` answer: distance from a coin flip, scaled to 0..1.
    pub fn probability_confidence(probability: f64) -> f64 {
        ((probability - 0.5).abs() * 2.0).clamp(0.0, 1.0)
    }

    /// Effective confidence, using the provider value or the `noul` distance.
    pub fn effective_confidence(&self) -> Option<f64> {
        self.confidence
            .or_else(|| self.noul.map(Self::probability_confidence))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub charged_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResult {
    pub provider: String,
    pub model: String,
    pub model_version: Option<String>,
    pub provider_request_id: Option<String>,
    pub answers: Vec<DecisionAnswer>,
    pub usage: DecisionUsage,
    pub latency_ms: i64,
}

impl DecisionResult {
    pub fn answer(&self, question: &str) -> Option<&DecisionAnswer> {
        self.answers
            .iter()
            .find(|answer| answer.question == question)
    }

    pub fn noul(&self, question: &str) -> Option<f64> {
        self.answer(question).and_then(|answer| answer.noul)
    }

    pub fn choice(&self, question: &str) -> Option<&str> {
        self.answer(question)
            .and_then(|answer| answer.choice.as_deref())
    }

    pub fn score(&self, question: &str) -> Option<f64> {
        self.answer(question).and_then(|answer| answer.score)
    }

    /// Mean confidence over the answers that carry one.
    pub fn mean_confidence(&self) -> Option<f64> {
        let values: Vec<f64> = self
            .answers
            .iter()
            .filter_map(|answer| answer.effective_confidence())
            .collect();
        if values.is_empty() {
            None
        } else {
            Some(values.iter().sum::<f64>() / values.len() as f64)
        }
    }
}

/// The questions asked for one discovered post. Thirteen questions stay under the provider limit of
/// twenty and read the state once, which is billed once.
pub fn lead_questions() -> Vec<DecisionQuestion> {
    let vertical_labels: Vec<(&str, &str)> = vec![
        (
            "hr_recruiting",
            "Hiring, recruiting, interviews, onboarding or workforce communication across languages.",
        ),
        (
            "international_meetings",
            "Meetings, calls or stand-ups where participants do not share a language.",
        ),
        (
            "conferences_webinars",
            "Events, conferences, webinars or live streams that need interpretation or captions.",
        ),
        (
            "customer_support",
            "Support, call centres or service conversations across languages.",
        ),
        (
            "tourism_hospitality",
            "Hotels, tours, travel and guests who speak another language.",
        ),
        ("other", "A real business problem outside the five target verticals."),
        ("irrelevant", "No business problem, or nothing a translation product could help with."),
    ];
    let intent_labels: Vec<(&str, &str)> = vec![
        (
            "explicit_solution_request",
            "The author asks for a tool, vendor or recommendation.",
        ),
        (
            "active_problem",
            "The author describes a current problem they are actively dealing with.",
        ),
        (
            "latent_problem",
            "The problem is implied by the situation but not stated as a need.",
        ),
        (
            "product_comparison",
            "The author compares products or approaches for this problem.",
        ),
        (
            "general_discussion",
            "General discussion, opinion or news without a concrete problem.",
        ),
        ("irrelevant", "Unrelated content."),
    ];
    let customer_labels: Vec<(&str, &str)> = vec![
        (
            "b2b",
            "The author speaks for a company, team or business process.",
        ),
        ("b2c", "A private individual's personal situation."),
        ("unknown", "Not enough evidence to decide."),
    ];
    let buying_labels: Vec<(&str, &str)> = vec![
        ("none", "No sign of wanting to solve it now."),
        ("weak", "Frustration without evaluation or action."),
        (
            "moderate",
            "Comparing options or asking how others solve it.",
        ),
        (
            "strong",
            "Actively evaluating solutions or asking for vendors.",
        ),
        ("explicit", "Explicitly asks to buy, trial or be contacted."),
    ];
    let potential_labels: Vec<(&str, &str)> = vec![
        ("low", "Small or one-off situation."),
        ("moderate", "Recurring but limited scope."),
        ("high", "Recurring, team-level or customer-facing cost."),
        (
            "very_high",
            "Strategic, organisation-wide or revenue-impacting.",
        ),
    ];
    let fit_labels: Vec<(&str, &str)> = vec![
        ("poor", "Real-time voice translation would not help."),
        ("partial", "Only part of the need is addressable."),
        (
            "good",
            "Real-time multilingual voice translation addresses the core need.",
        ),
        (
            "excellent",
            "The need is exactly live multilingual voice translation.",
        ),
    ];
    let language_labels: Vec<(&str, &str)> = vec![
        ("en", "English."),
        ("es", "Spanish."),
        ("pt", "Portuguese."),
        ("it", "Italian."),
        ("fr", "French."),
        ("de", "German."),
        ("nl", "Dutch."),
        ("pl", "Polish."),
        ("ru", "Russian."),
        ("tr", "Turkish."),
        ("ar", "Arabic."),
        ("hi", "Hindi."),
        ("zh", "Chinese."),
        ("ja", "Japanese."),
        ("ko", "Korean."),
        ("other", "A language outside this list."),
    ];

    vec![
        DecisionQuestion::noul(
            "relevance",
            "Is this content about a problem that real-time multilingual voice translation could plausibly help with?",
        ),
        DecisionQuestion::noul(
            "business_problem",
            "Does the content describe a concrete problem in a workplace, business or organisation?",
        ),
        DecisionQuestion::noul(
            "irrelevant_noise",
            "Is this content noise for a B2B opportunity search, such as homework, language-learning exercises, dictionary questions or personal translation requests?",
        ),
        DecisionQuestion::choice(
            "vertical",
            "Which target vertical does this content belong to?",
            QuestionCriteria::choice(&vertical_labels),
        ),
        DecisionQuestion::choice(
            "intent",
            "What is the author's intent regarding the problem?",
            QuestionCriteria::choice(&intent_labels),
        ),
        DecisionQuestion::choice(
            "customer_type",
            "Is the author a business buyer or a private consumer?",
            QuestionCriteria::choice(&customer_labels),
        ),
        DecisionQuestion::choice(
            "buying_intent",
            "How strong is the buying intent?",
            QuestionCriteria::choice(&buying_labels),
        ),
        DecisionQuestion::choice(
            "business_potential",
            "How large is the business potential if this need is real?",
            QuestionCriteria::choice(&potential_labels),
        ),
        DecisionQuestion::choice(
            "product_fit",
            "How well does real-time multilingual voice translation fit the stated need?",
            QuestionCriteria::choice(&fit_labels),
        ),
        DecisionQuestion::noul(
            "needs_human_review",
            "Does this opportunity require additional human review before any engagement?",
        ),
        DecisionQuestion::noul(
            "evidence_sufficient",
            "Is the quoted evidence sufficient to qualify the lead without guessing?",
        ),
        DecisionQuestion::noul(
            "too_ambiguous",
            "Is the content too ambiguous to make a reliable decision?",
        ),
        DecisionQuestion::choice(
            "language",
            "Which language is the content written in? Answer from the original text; do not translate it.",
            QuestionCriteria::choice(&language_labels),
        ),
    ]
}

/// Labels that the ingestion layer can map back to product enums.
pub fn vertical_from_label(label: &str) -> Option<Vertical> {
    Vertical::parse(label).filter(|vertical| *vertical != Vertical::Other)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::enums::{BusinessPotential, BuyingIntent, CustomerType, Intent, ProductFit};

    #[test]
    fn question_set_stays_within_provider_limits() {
        let questions = lead_questions();
        assert!(questions.len() <= MAX_QUESTIONS);
        for question in &questions {
            question.validate().expect("valid question");
        }
        assert!(validate_request(&"state ".repeat(10), &questions).is_ok());
    }

    #[test]
    fn criteria_shapes_are_type_safe() {
        let questions = lead_questions();
        let vertical = questions
            .iter()
            .find(|question| question.id == "vertical")
            .expect("vertical");
        assert!(matches!(
            vertical.criteria,
            Some(QuestionCriteria::Choice(_))
        ));
        let mut swapped = vertical.clone();
        swapped.criteria = Some(QuestionCriteria::score(&["a", "b"]));
        assert!(swapped.validate().is_err());
    }

    #[test]
    fn invalid_requests_are_rejected_before_the_call() {
        let questions = lead_questions();
        assert!(validate_request("", &questions).is_err());
        assert!(validate_request(&"x".repeat(MAX_STATE_CHARS + 1), &questions).is_err());
        let too_many: Vec<DecisionQuestion> =
            std::iter::repeat_n(questions[0].clone(), 21).collect();
        assert!(validate_request("state", &too_many).is_err());
    }

    #[test]
    fn every_label_maps_back_to_an_enum() {
        let questions = lead_questions();
        let expected = [
            ("vertical", Vertical::ALL.len() + 1),
            ("intent", Intent::ALL.len()),
            ("customer_type", CustomerType::ALL.len()),
            ("buying_intent", BuyingIntent::ALL.len()),
            ("business_potential", BusinessPotential::ALL.len()),
            ("product_fit", ProductFit::ALL.len()),
        ];
        for (id, _count) in expected {
            let question = questions
                .iter()
                .find(|question| question.id == id)
                .expect("question");
            if let Some(QuestionCriteria::Choice(labels)) = &question.criteria {
                for label in labels.keys() {
                    if id == "vertical" {
                        // "irrelevant" is a routing sentinel, not a target vertical.
                        if label == "irrelevant" {
                            continue;
                        }
                        assert!(
                            Vertical::parse(label).is_some(),
                            "vertical label {label} must parse"
                        );
                    }
                    if id == "intent" {
                        assert!(Intent::parse(label).is_some(), "intent label {label}");
                    }
                    if id == "customer_type" {
                        assert!(CustomerType::parse(label).is_some());
                    }
                    if id == "buying_intent" {
                        assert!(BuyingIntent::parse(label).is_some());
                    }
                    if id == "business_potential" {
                        assert!(BusinessPotential::parse(label).is_some());
                    }
                    if id == "product_fit" {
                        assert!(ProductFit::parse(label).is_some());
                    }
                }
            }
        }
    }

    #[test]
    fn noul_confidence_is_distance_from_coin_flip() {
        assert_eq!(DecisionAnswer::probability_confidence(0.5), 0.0);
        assert_eq!(DecisionAnswer::probability_confidence(1.0), 1.0);
        assert_eq!(DecisionAnswer::probability_confidence(0.0), 1.0);
        assert!((DecisionAnswer::probability_confidence(0.75) - 0.5).abs() < 1e-9);
    }
}
