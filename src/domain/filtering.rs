//! Deterministic keyword filtering that runs before any paid call. Matching is case-insensitive on
//! normalized text with word boundaries, so "work" never matches "network" and a negative keyword
//! always wins over a positive one.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::domain::enums::Vertical;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordSet {
    /// vertical (as spelling) -> positive terms
    pub positive: BTreeMap<String, Vec<String>>,
    pub negative: Vec<String>,
}

impl KeywordSet {
    pub fn add_positive(&mut self, vertical: Vertical, term: &str) {
        let normalized = normalize(term);
        if normalized.is_empty() {
            return;
        }
        let entry = self
            .positive
            .entry(vertical.as_str().to_string())
            .or_default();
        if !entry.contains(&normalized) {
            entry.push(normalized);
        }
    }

    pub fn add_negative(&mut self, term: &str) {
        let normalized = normalize(term);
        if normalized.is_empty() {
            return;
        }
        if !self.negative.contains(&normalized) {
            self.negative.push(normalized);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.positive.values().all(|terms| terms.is_empty())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterOutcome {
    pub relevant: bool,
    pub vertical: Option<Vertical>,
    pub matched_positive: Vec<String>,
    pub matched_negative: Vec<String>,
}

/// Lowercase, replace anything that is not alphanumeric with a space, collapse whitespace.
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = true;
    for character in text.chars().flat_map(|character| {
        if character.is_alphanumeric() {
            character.to_lowercase().collect::<Vec<_>>()
        } else {
            vec![' ']
        }
    }) {
        if character == ' ' {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(character);
            last_space = false;
        }
    }
    out.trim().to_string()
}

/// Word-boundary substring match on already-normalized text.
pub fn matches_term(normalized_text: &str, normalized_term: &str) -> bool {
    if normalized_term.is_empty() {
        return false;
    }
    let haystack = format!(" {normalized_text} ");
    let needle = format!(" {normalized_term} ");
    haystack.contains(&needle)
}

pub fn evaluate(text: &str, keywords: &KeywordSet) -> FilterOutcome {
    let normalized = normalize(text);
    let mut matched_negative = Vec::new();
    for term in &keywords.negative {
        if matches_term(&normalized, term) {
            matched_negative.push(term.clone());
        }
    }

    let mut matched_positive = Vec::new();
    let mut votes: BTreeMap<String, usize> = BTreeMap::new();
    for vertical in Vertical::PRODUCT_VERTICALS
        .iter()
        .chain(std::iter::once(&Vertical::Other))
    {
        if let Some(terms) = keywords.positive.get(vertical.as_str()) {
            for term in terms {
                if matches_term(&normalized, term) {
                    matched_positive.push(term.clone());
                    *votes.entry(vertical.as_str().to_string()).or_default() += 1;
                }
            }
        }
    }

    if !matched_negative.is_empty() {
        return FilterOutcome {
            relevant: false,
            vertical: None,
            matched_positive,
            matched_negative,
        };
    }

    // Deterministic primary vertical: most matches; ties fall back to the product order.
    let mut vertical = None;
    let mut best = 0usize;
    for candidate in Vertical::PRODUCT_VERTICALS
        .iter()
        .chain(std::iter::once(&Vertical::Other))
    {
        let count = votes.get(candidate.as_str()).copied().unwrap_or(0);
        if count > best {
            best = count;
            vertical = Some(*candidate);
        }
    }

    FilterOutcome {
        relevant: !matched_positive.is_empty(),
        vertical,
        matched_positive,
        matched_negative,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keywords() -> KeywordSet {
        let mut set = KeywordSet::default();
        set.add_positive(Vertical::HrRecruiting, "international hiring");
        set.add_positive(Vertical::HrRecruiting, "multilingual interviews");
        set.add_positive(Vertical::InternationalMeetings, "multilingual meetings");
        set.add_negative("translation homework");
        set.add_negative("language learning exercises");
        set
    }

    #[test]
    fn normalization_removes_punctuation_and_case() {
        assert_eq!(normalize("Hello,   WORLD!!"), "hello world");
        assert_eq!(normalize("  "), "");
        assert_eq!(normalize("Naïve"), "naïve");
    }

    #[test]
    fn matches_require_word_boundaries() {
        let text = normalize("Our network is down");
        assert!(!matches_term(&text, "work"));
        assert!(matches_term(&text, "network"));
        let text = normalize("We are hiring multilingual people");
        assert!(matches_term(&text, "hiring"));
    }

    #[test]
    fn positive_keyword_sets_the_vertical() {
        let outcome = evaluate(
            "We Struggle with international hiring: interviews with candidates who do not share a language",
            &keywords(),
        );
        assert!(outcome.relevant);
        assert_eq!(outcome.vertical, Some(Vertical::HrRecruiting));
        assert_eq!(outcome.matched_positive, vec!["international hiring"]);
        assert!(outcome.matched_negative.is_empty());
    }

    #[test]
    fn strongest_vertical_wins_and_ties_are_deterministic() {
        let outcome = evaluate(
            "multilingual interviews and multilingual meetings are both hard",
            &keywords(),
        );
        assert_eq!(outcome.matched_positive.len(), 2);
        // One match each: product order decides, HR comes first.
        assert_eq!(outcome.vertical, Some(Vertical::HrRecruiting));

        let outcome = evaluate(
            "multilingual meetings, multilingual meetings again",
            &keywords(),
        );
        assert_eq!(outcome.vertical, Some(Vertical::InternationalMeetings));
    }

    #[test]
    fn negative_keywords_always_win() {
        let outcome = evaluate(
            "I need help with my translation homework about international hiring",
            &keywords(),
        );
        assert!(!outcome.relevant);
        assert_eq!(outcome.vertical, None);
        assert_eq!(outcome.matched_negative, vec!["translation homework"]);
    }

    #[test]
    fn no_match_is_not_relevant() {
        let outcome = evaluate("The weather is nice today", &keywords());
        assert!(!outcome.relevant);
        assert_eq!(outcome.vertical, None);
        assert!(outcome.matched_positive.is_empty());
    }
}
