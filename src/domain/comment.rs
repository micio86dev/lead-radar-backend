//! Comment policy. Generated text is untrusted until these rules pass: it must be in the original
//! language, disclose affiliation when pitching, avoid invented experience or claims, and never be
//! a generic template. Nothing here publishes anything.

use serde::{Deserialize, Serialize};

use crate::domain::enums::{CommentStatus, CommentVariant};
use crate::domain::language::{detect, primary_subtag};

pub const MIN_BODY_CHARS: usize = 40;
pub const MAX_BODY_CHARS: usize = 1200;

/// Claims a model must never make on the user's behalf.
pub const FORBIDDEN_PHRASES: [&str; 9] = [
    "i personally",
    "in my experience",
    "we guarantee",
    "100% accurate",
    "best in the world",
    "as an ai",
    "our customers say",
    "testimonial",
    "no other product",
];

/// Sales language that does not belong in the "helpful" variant.
pub const PROMOTIONAL_PHRASES: [&str; 6] = [
    "sign up",
    "book a demo",
    "start your free trial",
    "buy now",
    "discount",
    "pricing plans",
];

/// Affiliation can be disclosed in the discussion's language; these are the accepted forms.
pub const DISCLOSURE_MARKERS: [&str; 15] = [
    "i work on",
    "i work at",
    "i build",
    "we build",
    "we make",
    "full disclosure",
    "disclosure:",
    "i am affiliated",
    "trabajo en",
    "aviso:",
    "trabalho na",
    "lavoro su",
    "je travaille sur",
    "ich arbeite",
    "hinweis:",
];

/// One disclosure sentence per supported language. The generator prompt quotes these examples, and a
/// test proves each of them satisfies the validator, so prompt and policy cannot drift apart.
pub const DISCLOSURE_EXAMPLES: [(&str, &str); 6] = [
    ("en", "Disclosure: I work on VoxTranslate."),
    ("es", "Aviso: trabajo en VoxTranslate."),
    ("pt", "Aviso: trabalho na VoxTranslate."),
    ("it", "Avviso: lavoro su VoxTranslate."),
    ("fr", "Avis : je travaille sur VoxTranslate."),
    ("de", "Hinweis: ich arbeite an VoxTranslate."),
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentDraft {
    pub variant: CommentVariant,
    pub language: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "issue", rename_all = "snake_case")]
pub enum CommentIssue {
    TooShort { min: usize },
    TooLong { max: usize },
    MissingAffiliationDisclosure,
    PromotionalLanguageInHelpful { phrase: String },
    ForbiddenClaim { phrase: String },
    LanguageMismatch { expected: String, detected: String },
}

/// Validate one generated draft. Empty result means the draft may be stored (still as a draft).
pub fn validate(draft: &CommentDraft) -> Vec<CommentIssue> {
    let mut issues = Vec::new();
    let body = draft.body.trim();
    let characters = body.chars().count();
    if characters < MIN_BODY_CHARS {
        issues.push(CommentIssue::TooShort {
            min: MIN_BODY_CHARS,
        });
    }
    if characters > MAX_BODY_CHARS {
        issues.push(CommentIssue::TooLong {
            max: MAX_BODY_CHARS,
        });
    }

    let lowered = body.to_lowercase();
    for phrase in FORBIDDEN_PHRASES {
        if lowered.contains(phrase) {
            issues.push(CommentIssue::ForbiddenClaim {
                phrase: phrase.to_string(),
            });
        }
    }

    if draft.variant == CommentVariant::Helpful {
        for phrase in PROMOTIONAL_PHRASES {
            if lowered.contains(phrase) {
                issues.push(CommentIssue::PromotionalLanguageInHelpful {
                    phrase: phrase.to_string(),
                });
            }
        }
    }

    if matches!(
        draft.variant,
        CommentVariant::SoftPitch | CommentVariant::DirectPitch
    ) {
        let discloses = lowered.contains("voxtranslate")
            && DISCLOSURE_MARKERS
                .iter()
                .any(|marker| lowered.contains(marker));
        if !discloses {
            issues.push(CommentIssue::MissingAffiliationDisclosure);
        }
    }

    let detected = detect(body);
    let expected = primary_subtag(&draft.language);
    if detected != "und" && !expected.is_empty() && detected != expected {
        issues.push(CommentIssue::LanguageMismatch {
            expected: expected.clone(),
            detected,
        });
    }

    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(variant: CommentVariant, language: &str, body: &str) -> CommentDraft {
        CommentDraft {
            variant,
            language: language.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn a_grounded_helpful_answer_passes() {
        let issues = validate(&draft(
            CommentVariant::Helpful,
            "en",
            "For interviews across languages, a shared glossary of role-specific terms and a short pre-brief with the candidate usually remove most of the friction.",
        ));
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_pitch_without_disclosure_is_rejected() {
        let issues = validate(&draft(
            CommentVariant::SoftPitch,
            "en",
            "For this problem, a live translation tool such as VoxTranslate can join the call and translate speech in real time.",
        ));
        assert!(issues.contains(&CommentIssue::MissingAffiliationDisclosure));
    }

    #[test]
    fn a_disclosed_pitch_passes() {
        let issues = validate(&draft(
            CommentVariant::SoftPitch,
            "en",
            "Disclosure: I work on VoxTranslate. For meetings where some people speak another language, joining with real-time translation is usually easier than scheduling interpreters.",
        ));
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn invented_experience_and_claims_are_rejected() {
        let issues = validate(&draft(
            CommentVariant::Helpful,
            "en",
            "I personally used this for years and we guarantee it is 100% accurate for every language pair you can imagine.",
        ));
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, CommentIssue::ForbiddenClaim { .. })));
    }

    #[test]
    fn helpful_variant_must_not_sell() {
        let issues = validate(&draft(
            CommentVariant::Helpful,
            "en",
            "You should sign up today and book a demo with the team to fix this meeting problem.",
        ));
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, CommentIssue::PromotionalLanguageInHelpful { .. })));
    }

    #[test]
    fn length_and_language_are_enforced() {
        let issues = validate(&draft(CommentVariant::Helpful, "en", "Too short."));
        assert!(issues.contains(&CommentIssue::TooShort {
            min: MIN_BODY_CHARS
        }));

        let issues = validate(&draft(
            CommentVariant::Helpful,
            "es",
            "For interviews across languages, a shared glossary of role-specific terms usually removes most of the friction from the call.",
        ));
        assert!(issues.iter().any(|issue| matches!(
            issue,
            CommentIssue::LanguageMismatch { expected, detected }
                if expected == "es" && detected == "en"
        )));
    }

    #[test]
    fn every_documented_disclosure_example_is_accepted_by_the_policy() {
        for (language, sentence) in DISCLOSURE_EXAMPLES {
            let lowered = sentence.to_lowercase();
            assert!(
                lowered.contains("voxtranslate"),
                "example for {language} must name the product"
            );
            assert!(
                DISCLOSURE_MARKERS
                    .iter()
                    .any(|marker| lowered.contains(marker)),
                "example for {language} must contain an accepted disclosure marker: {sentence}"
            );
        }
    }

    #[test]
    fn long_bodies_are_rejected() {
        let issues = validate(&draft(CommentVariant::Helpful, "en", &"a ".repeat(2000)));
        assert!(issues
            .iter()
            .any(|issue| matches!(issue, CommentIssue::TooLong { .. })));
    }
}

/// A body/status edit may only move a comment through the drafting statuses. Approval, rejection and
/// manual publication each have their own endpoint and their own event.
pub fn patch_target_allowed(current: CommentStatus, target: CommentStatus) -> bool {
    let editable = matches!(
        current,
        CommentStatus::Draft | CommentStatus::PendingReview | CommentStatus::NeedsEdit
    );
    let draft_only = matches!(
        target,
        CommentStatus::Draft | CommentStatus::PendingReview | CommentStatus::NeedsEdit
    );
    editable && draft_only
}

pub fn approval_allowed(current: CommentStatus) -> bool {
    matches!(
        current,
        CommentStatus::Draft | CommentStatus::PendingReview | CommentStatus::NeedsEdit
    )
}

pub fn rejection_allowed(current: CommentStatus) -> bool {
    !matches!(
        current,
        CommentStatus::PublishedManually | CommentStatus::Rejected
    )
}

/// Only an approved comment may be recorded as manually published. This is a recording endpoint:
/// nothing is ever sent to a third party.
pub fn publication_allowed(current: CommentStatus) -> bool {
    current == CommentStatus::Approved
}

#[cfg(test)]
mod transition_tests {
    use super::*;

    #[test]
    fn draft_statuses_are_editable_but_approval_is_not() {
        assert!(patch_target_allowed(
            CommentStatus::Draft,
            CommentStatus::NeedsEdit
        ));
        assert!(!patch_target_allowed(
            CommentStatus::Approved,
            CommentStatus::Draft
        ));
        assert!(!patch_target_allowed(
            CommentStatus::Draft,
            CommentStatus::Approved
        ));
        assert!(approval_allowed(CommentStatus::PendingReview));
        assert!(!approval_allowed(CommentStatus::PublishedManually));
    }

    #[test]
    fn publication_requires_approval_first() {
        assert!(!publication_allowed(CommentStatus::PendingReview));
        assert!(publication_allowed(CommentStatus::Approved));
        assert!(!rejection_allowed(CommentStatus::PublishedManually));
    }
}
