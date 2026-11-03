//! Language detection used for two narrow purposes: preserving the original language of evidence
//! and checking that a generated comment was written in that language. It is a stopword heuristic,
//! not a translation step, and it never rewrites the source text.

/// Supported detection languages (ISO 639-1). "und" means undetermined.
pub const LANGUAGES: [&str; 6] = ["en", "es", "pt", "it", "fr", "de"];

const STOPWORDS: [(&str, &[&str]); 6] = [
    (
        "en",
        &[
            "the", "and", "with", "for", "our", "is", "are", "this", "that", "we", "you", "have",
            "not", "but", "from", "would", "should", "need",
        ],
    ),
    (
        "es",
        &[
            "el",
            "la",
            "los",
            "las",
            "que",
            "con",
            "para",
            "por",
            "una",
            "nuestro",
            "es",
            "son",
            "esto",
            "pero",
            "como",
            "más",
            "necesitamos",
            "trabajo",
        ],
    ),
    (
        "pt",
        &[
            "o",
            "a",
            "os",
            "as",
            "que",
            "com",
            "para",
            "por",
            "uma",
            "nosso",
            "é",
            "são",
            "isto",
            "mas",
            "como",
            "mais",
            "precisamos",
            "trabalho",
        ],
    ),
    (
        "it",
        &[
            "il", "lo", "la", "gli", "le", "che", "con", "per", "una", "nostro", "è", "sono",
            "questo", "ma", "come", "più", "abbiamo", "lavoro",
        ],
    ),
    (
        "fr",
        &[
            "le", "la", "les", "que", "avec", "pour", "par", "une", "notre", "est", "sont", "ceci",
            "mais", "comme", "plus", "nous", "besoin", "travail",
        ],
    ),
    (
        "de",
        &[
            "der", "die", "das", "und", "mit", "für", "von", "eine", "unser", "ist", "sind",
            "dies", "aber", "wie", "mehr", "wir", "brauchen", "arbeit",
        ],
    ),
];

/// Detect the dominant language from stopword counts. Returns `und` when there is no evidence.
pub fn detect(text: &str) -> String {
    let normalized = super::filtering::normalize(text);
    if normalized.is_empty() {
        return "und".to_string();
    }
    let words: Vec<&str> = normalized.split(' ').collect();
    let mut best = ("und", 0usize);
    for (language, stopwords) in STOPWORDS {
        let hits = words.iter().filter(|word| stopwords.contains(word)).count();
        if hits > best.1 {
            best = (language, hits);
        }
    }
    best.0.to_string()
}

/// Two-letter primary subtag, lowercased ("en-US" -> "en").
pub fn primary_subtag(language: &str) -> String {
    language
        .split(['-', '_'])
        .next()
        .unwrap_or(language)
        .trim()
        .to_ascii_lowercase()
}

/// True when the text plausibly is in `expected`. Undetermined detection never fails the check.
pub fn matches_language(text: &str, expected: &str) -> bool {
    let expected = primary_subtag(expected);
    if expected.is_empty() {
        return true;
    }
    let detected = detect(text);
    detected == "und" || detected == expected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_common_languages() {
        assert_eq!(
            detect("We need to translate our international meetings with the team"),
            "en"
        );
        assert_eq!(
            detect("Necesitamos traducir las reuniones con nuestro equipo"),
            "es"
        );
        assert_eq!(
            detect("Precisamos traduzir as reuniões com nosso time para o trabalho"),
            "pt"
        );
        assert_eq!(
            detect("Il nostro team ha bisogno di tradurre le riunioni con i clienti"),
            "it"
        );
        assert_eq!(
            detect("Nous avons besoin de traduire les réunions avec notre équipe"),
            "fr"
        );
        assert_eq!(
            detect("Wir brauchen eine Übersetzung für die Arbeit mit dem Team"),
            "de"
        );
    }

    #[test]
    fn unknown_text_is_undetermined() {
        assert_eq!(detect(""), "und");
        assert_eq!(detect("ARGHHH 123"), "und");
    }

    #[test]
    fn language_matching_never_fails_on_undetermined_text() {
        assert!(matches_language("123 456", "es"));
        assert!(matches_language("We need this today", "en-US"));
        assert!(!matches_language("We need this today", "de"));
        assert_eq!(primary_subtag("pt-BR"), "pt");
    }
}
