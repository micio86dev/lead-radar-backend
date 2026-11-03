//! Deterministic offline providers for tests and local development.
//!
//! They are explicit opt-ins (`DECISION_PROVIDER=mock`, `GENERATIVE_PROVIDER=mock`,
//! `SOURCE_PROVIDER=mock`) and are rejected in production configuration. Their output is a pure
//! function of the input, so tests are reproducible and nothing is billed.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    DecisionProvider, DecisionRequest, GenerativeProvider, GenerativeResult, ProviderError,
    ProviderHealth, ProviderMetadata, RunStatus, SourceProvider, SourceRun, SourceRunRequest,
};
use crate::domain::decision::{
    validate_request, DecisionAnswer, DecisionResult, DecisionUsage, QuestionKind,
};
use crate::domain::filtering::normalize;
use crate::domain::language::detect;

fn metadata(provider: &str, kind: &str) -> ProviderMetadata {
    ProviderMetadata {
        provider: provider.to_string(),
        kind: kind.to_string(),
        model: Some("mock".to_string()),
        configured: true,
        enabled: true,
        base_url: None,
    }
}

fn noul(question: &str, probability: f64) -> DecisionAnswer {
    DecisionAnswer {
        question: question.to_string(),
        kind: QuestionKind::Noul,
        noul: Some(probability.clamp(0.0, 1.0)),
        choice: None,
        score: None,
        legend: None,
        probabilities: None,
        confidence: None,
    }
}

fn choice(question: &str, label: &str, confidence: f64) -> DecisionAnswer {
    let probabilities = BTreeMap::from([(label.to_string(), confidence)]);
    DecisionAnswer {
        question: question.to_string(),
        kind: QuestionKind::Choice,
        noul: None,
        choice: Some(label.to_string()),
        score: None,
        legend: None,
        probabilities: Some(probabilities),
        confidence: Some(confidence),
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

/// Heuristics over the normalized state. Deterministic and free.
pub fn classify(state: &str) -> Vec<DecisionAnswer> {
    let text = normalize(state);
    let translation_signal = contains_any(
        &text,
        &[
            "translat",
            "interpret",
            "language barrier",
            "multilingual",
            "another language",
            "foreign language",
            "subtitl",
            "traduc",
            "traduz",
            "traduire",
            "traduzione",
            "übersetz",
            "idioma",
            "langue",
            "sprache",
            "lingua",
            "barriere linguistique",
            "sprachbarriere",
        ],
    );
    let business_signal = contains_any(
        &text,
        &[
            "team",
            "company",
            "our ",
            "client",
            "customer",
            "hire",
            "hiring",
            "recruit",
            "interview",
            "meeting",
            "webinar",
            "conference",
            "support",
            "hotel",
            "guest",
            "call centre",
            "call center",
            "equipo",
            "equipe",
            "empresa",
            "equipe",
            "reuni",
            "reunión",
            "reunion",
            "riunion",
            "riunioni",
            "réunion",
            "entrevista",
            "entretien",
            "colloquio",
            "candidat",
            "cliente",
            "clienti",
            "kunde",
            "gast",
            "conferencia",
            "conférence",
            "konferenz",
        ],
    );
    let noise = contains_any(
        &text,
        &[
            "homework",
            "assignment",
            "dictionary",
            "vocabulary",
            "exercise",
            "grammar",
            "duolingo",
            "personal translation",
        ],
    );
    let explicit = contains_any(
        &text,
        &[
            "recommend",
            "looking for",
            "any tool",
            "any suggestions",
            "suggestions",
            "which tool",
            "vendors",
            "buy",
        ],
    );
    let active = contains_any(
        &text,
        &[
            "struggl",
            "problem",
            "difficult",
            "hard to",
            "cannot",
            "can t",
            "we need",
            "issue",
            "pain",
        ],
    );
    let b2b = contains_any(
        &text,
        &[
            "team",
            "company",
            "our ",
            "client",
            "customer",
            "hire",
            "hiring",
            "recruit",
            "business",
            "equipo",
            "empresa",
            "equipe",
            "reuni",
            "riunion",
            "réunion",
            "entrevista",
            "candidat",
            "cliente",
            "kunde",
            "unternehmen",
        ],
    );

    let vertical = if contains_any(
        &text,
        &[
            "recruit",
            "hir",
            "interview",
            "candidate",
            "hr ",
            "entrevista",
            "candidat",
            "colloquio",
            "entretien",
        ],
    ) {
        "hr_recruiting"
    } else if contains_any(&text, &["conference", "webinar", "event", "summit"]) {
        "conferences_webinars"
    } else if contains_any(&text, &["support", "call centre", "call center", "ticket"]) {
        "customer_support"
    } else if contains_any(&text, &["hotel", "guest", "tour", "travel", "hospitality"]) {
        "tourism_hospitality"
    } else if contains_any(&text, &["meeting", "zoom", "teams", "standup", "remote"]) {
        "international_meetings"
    } else if business_signal {
        "other"
    } else {
        "irrelevant"
    };

    let intent = if noise || vertical == "irrelevant" {
        "irrelevant"
    } else if explicit {
        "explicit_solution_request"
    } else if active {
        "active_problem"
    } else if business_signal && translation_signal {
        "latent_problem"
    } else if contains_any(&text, &["vs ", "alternative", "compare"]) {
        "product_comparison"
    } else {
        "general_discussion"
    };

    let relevance = if noise {
        0.08
    } else if translation_signal && business_signal {
        0.92
    } else if translation_signal || business_signal {
        0.58
    } else {
        0.12
    };

    let buying_intent = match intent {
        "explicit_solution_request" => "explicit",
        "product_comparison" => "strong",
        "active_problem" => "moderate",
        "latent_problem" => "weak",
        _ => "none",
    };
    let business_potential = if business_signal && translation_signal {
        "high"
    } else if business_signal {
        "moderate"
    } else {
        "low"
    };
    let product_fit = if translation_signal && business_signal {
        "good"
    } else if translation_signal {
        "partial"
    } else {
        "poor"
    };
    let customer_type = if b2b {
        "b2b"
    } else if contains_any(&text, &["i ", "me ", "my "]) {
        "b2c"
    } else {
        "unknown"
    };
    let language = detect(state);
    let language_label = if ["en", "es", "pt", "it", "fr", "de"].contains(&language.as_str()) {
        language.as_str()
    } else {
        "other"
    };
    let too_ambiguous = text.split(' ').count() < 8;

    vec![
        noul("relevance", relevance),
        noul("business_problem", if business_signal { 0.85 } else { 0.2 }),
        noul("irrelevant_noise", if noise { 0.92 } else { 0.05 }),
        choice("vertical", vertical, 0.8),
        choice("intent", intent, 0.78),
        choice("customer_type", customer_type, 0.7),
        choice("buying_intent", buying_intent, 0.72),
        choice("business_potential", business_potential, 0.7),
        choice("product_fit", product_fit, 0.74),
        noul(
            "needs_human_review",
            if too_ambiguous || intent == "latent_problem" {
                0.75
            } else {
                0.2
            },
        ),
        noul(
            "evidence_sufficient",
            if text.split(' ').count() >= 12 {
                0.85
            } else {
                0.35
            },
        ),
        noul("too_ambiguous", if too_ambiguous { 0.7 } else { 0.1 }),
        choice("language", language_label, 0.85),
    ]
}

pub struct MockDecisionProvider;

impl MockDecisionProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockDecisionProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DecisionProvider for MockDecisionProvider {
    fn metadata(&self) -> ProviderMetadata {
        metadata("mock", "decision")
    }

    async fn health_check(&self) -> ProviderHealth {
        ProviderHealth::ok("mock", 0)
    }

    async fn evaluate(&self, request: &DecisionRequest) -> Result<DecisionResult, ProviderError> {
        validate_request(&request.state, &request.questions)
            .map_err(ProviderError::InvalidRequest)?;
        let answers = classify(&request.state)
            .into_iter()
            .filter(|answer| {
                request
                    .questions
                    .iter()
                    .any(|question| question.id == answer.question)
            })
            .collect();
        Ok(DecisionResult {
            provider: "mock".to_string(),
            model: "mock".to_string(),
            model_version: Some("mock-1".to_string()),
            provider_request_id: Some(format!(
                "mock-{}",
                request.idempotency_key.as_deref().unwrap_or("no-key")
            )),
            answers,
            usage: DecisionUsage {
                input_tokens: Some((request.state.chars().count() / 4) as i64),
                output_tokens: Some(0),
                charged_tokens: Some(0),
            },
            latency_ms: 1,
        })
    }
}

fn comment_body(variant: &str, language: &str) -> &'static str {
    match (variant, language) {
        ("helpful", "es") => "Es una situación habitual cuando el equipo trabaja en varios idiomas. Compartir un glosario de términos del puesto y hacer una breve puesta en común antes de la conversación suele eliminar casi toda la fricción.",
        ("helpful", "pt") => "É uma situação comum quando a equipe trabalha em vários idiomas. Compartilhar um glossário com termos da função e fazer um alinhamento rápido antes da conversa costuma eliminar quase todo o atrito.",
        ("helpful", "it") => "È una situazione comune quando il team lavora in più lingue. Condividere un glossario dei termini del ruolo e fare un breve allineamento prima della conversazione di solito elimina quasi tutto l'attrito.",
        ("helpful", "fr") => "C'est une situation courante quand l'équipe travaille en plusieurs langues. Partager un glossaire des termes du poste et faire un bref alignement avant la conversation supprime généralement presque toute la friction.",
        ("helpful", "de") => "Das ist eine häufige Situation, wenn das Team in mehreren Sprachen arbeitet. Ein gemeinsames Glossar der rollenspezifischen Begriffe und ein kurzes Briefing vor dem Gespräch beseitigen meist den größten Teil der Reibung.",
        ("helpful", _) => "This is a common situation when a team works across languages. Giving everyone a shared glossary of role-specific terms and a short briefing before the conversation usually removes most of the friction.",
        ("soft_pitch", "es") => "Aviso: trabajo en VoxTranslate. Es justo el problema que resuelve: se suma a la conversación y traduce la voz en vivo, así cada persona sigue hablando su idioma. Un glosario compartido sigue ayudando con nombres y términos del producto.",
        ("soft_pitch", "pt") => "Aviso: trabalho na VoxTranslate. É exatamente o problema que ela resolve: entra na conversa e traduz a voz ao vivo, assim cada pessoa continua falando o próprio idioma. Um glossário compartilhado ainda ajuda com nomes e termos do produto.",
        ("soft_pitch", "it") => "Avviso: lavoro su VoxTranslate. È esattamente il problema che risolve: entra nella conversazione e traduce la voce in tempo reale, così ognuno continua a parlare la propria lingua. Un glossario condiviso aiuta ancora con nomi e termini di prodotto.",
        ("soft_pitch", "fr") => "Avis : je travaille sur VoxTranslate. C'est exactement le problème qu'il traite : il rejoint la conversation et traduit la voix en direct, chacun continue donc de parler sa langue. Un glossaire partagé reste utile pour les noms et les termes produit.",
        ("soft_pitch", "de") => "Hinweis: ich arbeite an VoxTranslate. Genau das ist das Problem, das es löst: es nimmt am Gespräch teil und übersetzt Sprache live, sodass jede Person weiter in ihrer Sprache sprechen kann. Ein gemeinsames Glossar hilft weiterhin bei Namen und Produktbegriffen.",
        ("soft_pitch", _) => "Disclosure: I work on VoxTranslate. This is exactly the problem it targets: it joins the conversation and translates speech live, so people can keep speaking their own language. A shared glossary still helps for names and product terms.",
        ("direct_pitch", "es") => "Aviso: trabajo en VoxTranslate. Si el objetivo es dejar de perder candidatos por el idioma, ofrece traducción de voz en tiempo real en la propia entrevista, sin coordinar un intérprete. Puedo responder dudas aquí y no por mensaje privado.",
        ("direct_pitch", "pt") => "Aviso: trabalho na VoxTranslate. Se o objetivo é parar de perder candidatos por causa do idioma, ela oferece tradução de voz em tempo real na própria entrevista, sem agendar intérprete. Posso responder dúvidas aqui, e não por mensagem privada.",
        ("direct_pitch", "it") => "Avviso: lavoro su VoxTranslate. Se l'obiettivo è smettere di perdere candidati per la lingua, offre traduzione vocale in tempo reale durante il colloquio, senza organizzare un interprete. Rispondo volentieri qui, non in privato.",
        ("direct_pitch", "fr") => "Avis : je travaille sur VoxTranslate. Si l'objectif est d'arrêter de perdre des candidats à cause de la langue, il traduit la voix en direct pendant l'entretien, sans organiser d'interprète. Je réponds volontiers ici, pas en message privé.",
        ("direct_pitch", "de") => "Hinweis: ich arbeite an VoxTranslate. Wenn das Ziel ist, keine Kandidaten mehr wegen der Sprache zu verlieren, übersetzt es Sprache live im Gespräch, ohne einen Dolmetscher zu organisieren. Ich antworte gern hier und nicht per Direktnachricht.",
        ("direct_pitch", _) => "Disclosure: I work on VoxTranslate. If the goal is to stop losing candidates because of the language gap, it provides real-time speech translation in the interview itself, without scheduling an interpreter. Happy to answer specifics here rather than in DMs.",
        _ => "This situation usually improves with a shared glossary and a short briefing before the conversation starts.",
    }
}

fn extract_marker(user: &str, marker: &str) -> Option<String> {
    user.lines().find_map(|line| {
        line.trim()
            .strip_prefix(marker)
            .map(|value| value.trim().to_string())
    })
}

pub struct MockGenerativeProvider;

impl MockGenerativeProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for MockGenerativeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl GenerativeProvider for MockGenerativeProvider {
    fn metadata(&self) -> ProviderMetadata {
        metadata("mock", "generative")
    }

    async fn health_check(&self) -> ProviderHealth {
        ProviderHealth::ok("mock", 0)
    }

    async fn generate_json(
        &self,
        _system: &str,
        user: &str,
        schema_hint: &str,
    ) -> Result<GenerativeResult, ProviderError> {
        let language = extract_marker(user, "language:").unwrap_or_else(|| "en".to_string());
        let language = language
            .split('-')
            .next()
            .unwrap_or("en")
            .to_ascii_lowercase();
        let content = if schema_hint.contains("comment_variants") {
            json!({
                "variants": [
                    {"variant": "helpful", "body": comment_body("helpful", &language)},
                    {"variant": "soft_pitch", "body": comment_body("soft_pitch", &language)},
                    {"variant": "direct_pitch", "body": comment_body("direct_pitch", &language)}
                ]
            })
        } else {
            json!({
                "summary": "The author describes a multilingual communication problem in a business context.",
                "evidence": ["multilingual communication problem"],
                "suggested_approach": "Offer a concrete operational tip first; mention the product only if the author asks."
            })
        };
        Ok(GenerativeResult {
            provider: "mock".to_string(),
            model: "mock".to_string(),
            request_id: Some("mock-generative".to_string()),
            content,
            input_tokens: Some((user.chars().count() / 4) as i64),
            output_tokens: Some(0),
            latency_ms: 1,
        })
    }
}

/// In-memory Apify stand-in. A run starts `running` and finishes `succeeded` with two deterministic
/// dataset items on the next poll, which exercises the whole asynchronous lifecycle offline.
#[derive(Default)]
pub struct MockSourceProvider {
    runs: Mutex<BTreeMap<String, (RunStatus, String)>>,
}

impl MockSourceProvider {
    pub fn new() -> Self {
        Self::default()
    }

    fn item(index: usize) -> Value {
        let content = match index {
            0 => "Our recruiting team cannot interview candidates who only speak Spanish. We need real-time translation in the interviews, any suggestions?",
            _ => "We run weekly multilingual meetings with our international remote teams and the language barrier at work is slowing everything down.",
        };
        json!({
            "id": format!("mock-post-{index}"),
            "title": format!("Mock conversation {index}"),
            "content": content,
            "url": format!("https://example.invalid/mock/{index}"),
            "author": "mock-user",
            "community": "recruiting",
            "language": "en",
            "published_at": "2024-05-01T09:00:00Z",
            "engagement": {"comments": 3, "upvotes": 7, "shares": 0}
        })
    }
}

#[async_trait]
impl SourceProvider for MockSourceProvider {
    fn metadata(&self) -> ProviderMetadata {
        metadata("mock", "source")
    }

    async fn health_check(&self) -> ProviderHealth {
        ProviderHealth::ok("mock", 0)
    }

    async fn start_run(&self, request: &SourceRunRequest) -> Result<SourceRun, ProviderError> {
        if request.actor_id.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "actor_id must not be empty".to_string(),
            ));
        }
        let run_id = format!("mock-run-{}", request.actor_id);
        self.runs.lock().expect("mock runs lock").insert(
            run_id.clone(),
            (RunStatus::Running, format!("dataset-{run_id}")),
        );
        Ok(SourceRun {
            provider_run_id: run_id,
            status: RunStatus::Running,
            dataset_id: Some("dataset-mock".to_string()),
            items_count: 0,
            cost_usd: None,
            started_at: Some(crate::domain::time::now_iso()),
            finished_at: None,
            error: None,
        })
    }

    async fn get_run(&self, provider_run_id: &str) -> Result<SourceRun, ProviderError> {
        let mut runs = self.runs.lock().expect("mock runs lock");
        let (status, dataset) = runs.get_mut(provider_run_id).ok_or_else(|| {
            ProviderError::InvalidRequest(format!("unknown run {provider_run_id}"))
        })?;
        *status = RunStatus::Succeeded;
        Ok(SourceRun {
            provider_run_id: provider_run_id.to_string(),
            status: RunStatus::Succeeded,
            dataset_id: Some(dataset.clone()),
            items_count: 2,
            cost_usd: Some(0.0),
            started_at: Some(crate::domain::time::now_iso()),
            finished_at: Some(crate::domain::time::now_iso()),
            error: None,
        })
    }

    async fn fetch_items(
        &self,
        _dataset_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Value>, ProviderError> {
        let total = 2i64;
        let start = offset.max(0);
        let end = (start + limit.max(0)).min(total);
        Ok((start..end)
            .map(|index| Self::item(index as usize))
            .collect())
    }

    async fn cancel_run(&self, provider_run_id: &str) -> Result<(), ProviderError> {
        self.runs.lock().expect("mock runs lock").insert(
            provider_run_id.to_string(),
            (RunStatus::Aborted, "dataset-mock".to_string()),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::decision::lead_questions;

    fn request(text: &str) -> DecisionRequest {
        DecisionRequest {
            state: text.to_string(),
            questions: lead_questions(),
            idempotency_key: Some("k1".to_string()),
        }
    }

    #[tokio::test]
    async fn classification_is_deterministic_and_typed() {
        let provider = MockDecisionProvider::new();
        let text = "Our recruiting team cannot interview candidates who only speak Spanish. We need real-time translation, any suggestions?";
        let first = provider.evaluate(&request(text)).await.expect("decision");
        let second = provider.evaluate(&request(text)).await.expect("decision");
        assert_eq!(first.answers, second.answers);
        assert_eq!(first.choice("vertical"), Some("hr_recruiting"));
        assert_eq!(first.choice("intent"), Some("explicit_solution_request"));
        assert_eq!(first.choice("customer_type"), Some("b2b"));
        assert!(first.noul("relevance").expect("relevance") > 0.8);
        assert_eq!(first.choice("language"), Some("en"));
        assert!(first.mean_confidence().is_some());
    }

    #[tokio::test]
    async fn homework_is_noise() {
        let provider = MockDecisionProvider::new();
        let result = provider
            .evaluate(&request(
                "I need help with my translation homework, it is a grammar exercise for school.",
            ))
            .await
            .expect("decision");
        assert!(result.noul("irrelevant_noise").expect("noise") > 0.8);
        assert_eq!(result.choice("intent"), Some("irrelevant"));
    }

    #[tokio::test]
    async fn spanish_content_is_detected_without_translation() {
        let provider = MockDecisionProvider::new();
        let result = provider
            .evaluate(&request(
                "Necesitamos traducir las reuniones con nuestro equipo internacional, el problema es el idioma.",
            ))
            .await
            .expect("decision");
        assert_eq!(result.choice("language"), Some("es"));
    }

    #[tokio::test]
    async fn generative_mock_produces_three_valid_variants() {
        let provider = MockGenerativeProvider::new();
        let result = provider
            .generate_json("system", "language: en", "comment_variants")
            .await
            .expect("generation");
        let variants = result.content["variants"].as_array().expect("variants");
        assert_eq!(variants.len(), 3);
        for expected in ["helpful", "soft_pitch", "direct_pitch"] {
            assert!(variants
                .iter()
                .any(|variant| variant["variant"] == expected));
        }
    }

    #[tokio::test]
    async fn source_run_completes_and_returns_items() {
        let provider = MockSourceProvider::new();
        let run = provider
            .start_run(&SourceRunRequest {
                actor_id: "mock-actor".to_string(),
                input: json!({}),
                max_items: 10,
                max_total_charge_usd: None,
            })
            .await
            .expect("start");
        assert_eq!(run.status, RunStatus::Running);
        let finished = provider.get_run(&run.provider_run_id).await.expect("get");
        assert_eq!(finished.status, RunStatus::Succeeded);
        let items = provider
            .fetch_items(finished.dataset_id.as_deref().expect("dataset"), 0, 10)
            .await
            .expect("items");
        assert_eq!(items.len(), 2);
    }
}
