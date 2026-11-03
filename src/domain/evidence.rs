//! Evidence extraction: exact excerpts and their character offsets. Nothing is paraphrased and
//! nothing is invented; when no configured term occurs, the evidence list stays empty.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    pub excerpt: String,
    pub start_offset: usize,
    pub end_offset: usize,
}

const CONTEXT_CHARS: usize = 120;
const MAX_EXCERPTS: usize = 3;

/// Find the configured terms in the original text (case-insensitive) and return exact excerpts with
/// their offsets. Overlapping excerpts are merged by taking the first occurrence of each term.
pub fn extract(content: &str, terms: &[String]) -> Vec<Evidence> {
    let lowered: Vec<char> = content.to_lowercase().chars().collect();
    let original: Vec<char> = content.chars().collect();
    let mut evidence: Vec<Evidence> = Vec::new();

    for term in terms {
        let needle: Vec<char> = term.to_lowercase().chars().collect();
        if needle.is_empty() || needle.len() > lowered.len() {
            continue;
        }
        let mut found = None;
        for start in 0..=lowered.len() - needle.len() {
            if lowered[start..start + needle.len()] == needle[..] {
                found = Some(start);
                break;
            }
        }
        let Some(start) = found else { continue };
        let end = start + needle.len();
        let excerpt_start = start.saturating_sub(CONTEXT_CHARS);
        let excerpt_end = (end + CONTEXT_CHARS).min(original.len());
        let excerpt: String = original[excerpt_start..excerpt_end].iter().collect();
        // Character offsets, not byte offsets.
        let excerpt = excerpt.trim().to_string();
        if evidence.iter().any(|existing| {
            existing.start_offset == excerpt_start && existing.end_offset == excerpt_end
        }) {
            continue;
        }
        evidence.push(Evidence {
            excerpt,
            start_offset: excerpt_start,
            end_offset: excerpt_end,
        });
        if evidence.len() >= MAX_EXCERPTS {
            break;
        }
    }
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excerpts_are_verbatim_and_offset() {
        let content =
            "Our team struggles with international hiring because candidates speak Portuguese.";
        let evidence = extract(content, &["international hiring".to_string()]);
        assert_eq!(evidence.len(), 1);
        assert!(evidence[0].excerpt.contains("international hiring"));
        let start = evidence[0].start_offset;
        let end = evidence[0].end_offset;
        let slice: String = content.chars().skip(start).take(end - start).collect();
        assert_eq!(slice.trim(), evidence[0].excerpt);
    }

    #[test]
    fn no_terms_mean_no_evidence() {
        assert!(extract("Irrelevant text", &[]).is_empty());
        assert!(extract("Irrelevant text", &["absent term".to_string()]).is_empty());
    }

    #[test]
    fn at_most_three_excerpts() {
        let filler = "x".repeat(300);
        let content = format!("alpha {filler} beta {filler} gamma {filler} delta");
        let evidence = extract(
            &content,
            &[
                "alpha".to_string(),
                "beta".to_string(),
                "gamma".to_string(),
                "delta".to_string(),
            ],
        );
        assert_eq!(evidence.len(), 3);
        assert!(evidence[0].excerpt.contains("alpha"));
        assert!(evidence[1].excerpt.contains("beta"));
        assert!(evidence[2].excerpt.contains("gamma"));
    }
}
