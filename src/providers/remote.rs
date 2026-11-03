//! Decision adapters for the TypeSafe/Jev system-one protocol.
//!
//! Verified against https://jev-ai.org/docs/decisions/ and https://jev-ai.org/docs/question-types/
//! and the Laya Studio OpenAPI document (https://laya.studio/v1/openapi), which states that Laya is
//! wire-compatible with the same protocol. Only documented fields are read or written.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{DecisionProvider, DecisionRequest, ProviderError, ProviderHealth, ProviderMetadata};
use crate::config::{JevConfig, LayaConfig};
use crate::domain::decision::{
    validate_request, DecisionAnswer, DecisionResult, DecisionUsage, QuestionCriteria, QuestionKind,
};

const PROVIDER_TIMEOUT_FLOOR_SECONDS: i64 = 5;

#[derive(Debug, Clone)]
pub struct JevCompatibleProvider {
    provider: String,
    endpoint: String,
    base_url: String,
    api_key: Option<String>,
    model: Option<String>,
    client: reqwest::Client,
}

impl JevCompatibleProvider {
    pub fn jev(config: &JevConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(
                config.timeout_seconds.max(PROVIDER_TIMEOUT_FLOOR_SECONDS) as u64,
            ))
            .build()?;
        Ok(Self {
            provider: "jev".to_string(),
            endpoint: config.endpoint(),
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            model: Some(config.model.clone()),
            client,
        })
    }

    pub fn laya(config: &LayaConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(
                config.timeout_seconds.max(PROVIDER_TIMEOUT_FLOOR_SECONDS) as u64,
            ))
            .build()?;
        Ok(Self {
            provider: "laya".to_string(),
            endpoint: config.endpoint(),
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            client,
        })
    }

    fn configured(&self) -> bool {
        self.api_key
            .as_deref()
            .map(|key| !key.trim().is_empty())
            .unwrap_or(false)
    }

    fn build_body<'a>(&'a self, request: &'a DecisionRequest) -> RemoteRequest<'a> {
        let questions = request
            .questions
            .iter()
            .map(|question| {
                (
                    question.id.as_str(),
                    RemoteQuestion {
                        kind: question.kind.as_str(),
                        instructions: question.instructions.as_str(),
                        criteria: question.criteria.as_ref(),
                    },
                )
            })
            .collect();
        RemoteRequest {
            model: self.model.as_deref(),
            state: request.state.as_str(),
            questions,
        }
    }
}

#[derive(Debug, Serialize)]
struct RemoteRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a str>,
    state: &'a str,
    questions: BTreeMap<&'a str, RemoteQuestion<'a>>,
}

#[derive(Debug, Serialize)]
struct RemoteQuestion<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    instructions: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    criteria: Option<&'a QuestionCriteria>,
}

#[derive(Debug, Deserialize)]
struct RemoteResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    model_version: Option<String>,
    answers: BTreeMap<String, RemoteAnswer>,
    #[serde(default)]
    usage: RemoteUsage,
    #[serde(default)]
    latency_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
struct RemoteUsage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    charged_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RemoteAnswer {
    Noul {
        noul: f64,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: Option<BTreeMap<String, f64>>,
        #[serde(default)]
        confidence: Option<f64>,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: Option<BTreeMap<String, String>>,
        #[serde(default)]
        probabilities: Option<BTreeMap<String, f64>>,
        #[serde(default)]
        confidence: Option<f64>,
    },
}

fn truncate(text: &str, limit: usize) -> String {
    let mut truncated: String = text.chars().take(limit).collect();
    if text.chars().count() > limit {
        truncated.push('…');
    }
    truncated
}

fn map_http_error(
    provider: &str,
    status: u16,
    retry_after_seconds: Option<i64>,
    body: &str,
) -> ProviderError {
    let detail = extract_error_message(body);
    match status {
        400 | 404 | 422 => ProviderError::InvalidRequest(detail),
        401 | 403 => ProviderError::Unauthorized(format!("{provider}: {detail}")),
        409 => ProviderError::Upstream {
            status,
            message: format!("idempotency conflict: {detail}"),
        },
        429 => ProviderError::RateLimited {
            retry_after_seconds,
        },
        500..=599 => ProviderError::Unavailable(detail),
        other => ProviderError::Upstream {
            status: other,
            message: detail,
        },
    }
}

/// Providers report errors as `{"detail": ...}` or `{"error": {"message": ...}}`; never surface the
/// raw body, which can contain a request echo.
fn extract_error_message(body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    if let Some(value) = parsed {
        for pointer in ["/detail", "/error/message", "/message", "/error"] {
            if let Some(found) = value.pointer(pointer) {
                if let Some(text) = found.as_str() {
                    return truncate(text, 300);
                }
            }
        }
    }
    truncate(body.trim(), 200)
}

#[async_trait::async_trait]
impl DecisionProvider for JevCompatibleProvider {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            provider: self.provider.clone(),
            kind: "decision".to_string(),
            model: self.model.clone(),
            configured: self.configured(),
            enabled: self.configured(),
            base_url: Some(self.base_url.clone()),
        }
    }

    /// A real, billed probe with a single `noul` question. Only the explicit
    /// `POST /api/v1/providers/:provider/test` endpoint calls it; the dashboard reads recorded
    /// usage instead so a page refresh can never spend money.
    async fn health_check(&self) -> ProviderHealth {
        if !self.configured() {
            return ProviderHealth::unconfigured(&self.provider);
        }
        let request = DecisionRequest {
            state: "Health check: is this sentence in English?".to_string(),
            questions: vec![crate::domain::decision::DecisionQuestion::noul(
                "english",
                "Is this text in English?",
            )],
            idempotency_key: Some(format!("health-{}", uuid::Uuid::now_v7())),
        };
        let started = Instant::now();
        match self.evaluate(&request).await {
            Ok(_) => ProviderHealth::ok(
                &self.provider,
                started.elapsed().as_millis().min(i64::MAX as u128) as i64,
            ),
            Err(error) => ProviderHealth::error(&self.provider, error.to_string()),
        }
    }

    async fn evaluate(&self, request: &DecisionRequest) -> Result<DecisionResult, ProviderError> {
        let api_key = self
            .api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| ProviderError::Unconfigured(self.provider.clone()))?;
        validate_request(&request.state, &request.questions)
            .map_err(ProviderError::InvalidRequest)?;

        let body = self.build_body(request);
        let mut builder = self
            .client
            .post(&self.endpoint)
            .bearer_auth(api_key)
            .json(&body);
        if let Some(key) = &request.idempotency_key {
            builder = builder.header("Idempotency-Key", key);
        }

        let started = Instant::now();
        let response = builder
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok());
        let text = response
            .text()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let wall_latency = started.elapsed().as_millis().min(i64::MAX as u128) as i64;

        if !status.is_success() {
            return Err(map_http_error(
                &self.provider,
                status.as_u16(),
                retry_after,
                &text,
            ));
        }

        let parsed: RemoteResponse = serde_json::from_str(&text).map_err(|error| {
            ProviderError::InvalidResponse(format!("{error}: {}", truncate(&text, 200)))
        })?;

        let mut answers = Vec::with_capacity(request.questions.len());
        for question in &request.questions {
            let answer = parsed.answers.get(&question.id).ok_or_else(|| {
                ProviderError::InvalidResponse(format!(
                    "answer for question {} is missing",
                    question.id
                ))
            })?;
            let mapped = match answer {
                RemoteAnswer::Noul { noul, confidence } => DecisionAnswer {
                    question: question.id.clone(),
                    kind: QuestionKind::Noul,
                    noul: Some(noul.clamp(0.0, 1.0)),
                    choice: None,
                    score: None,
                    legend: None,
                    probabilities: None,
                    confidence: *confidence,
                },
                RemoteAnswer::Choice {
                    choice,
                    probabilities,
                    confidence,
                } => DecisionAnswer {
                    question: question.id.clone(),
                    kind: QuestionKind::Choice,
                    noul: None,
                    choice: Some(choice.clone()),
                    score: None,
                    legend: None,
                    probabilities: probabilities.clone(),
                    confidence: *confidence,
                },
                RemoteAnswer::Score {
                    score,
                    legend,
                    probabilities,
                    confidence,
                } => DecisionAnswer {
                    question: question.id.clone(),
                    kind: QuestionKind::Score,
                    noul: None,
                    choice: None,
                    score: Some(*score),
                    legend: legend.clone(),
                    probabilities: probabilities.clone(),
                    confidence: *confidence,
                },
            };
            answers.push(mapped);
        }

        Ok(DecisionResult {
            provider: self.provider.clone(),
            model: parsed.model.unwrap_or_else(|| {
                self.model
                    .clone()
                    .unwrap_or_else(|| "unspecified".to_string())
            }),
            model_version: parsed.model_version,
            provider_request_id: parsed.id,
            answers,
            usage: DecisionUsage {
                input_tokens: parsed.usage.input_tokens,
                output_tokens: parsed.usage.output_tokens,
                charged_tokens: parsed.usage.charged_tokens,
            },
            latency_ms: parsed.latency_ms.unwrap_or(wall_latency),
        })
    }
}

/// Jev is the production decision provider.
pub struct JevProvider {
    inner: JevCompatibleProvider,
}

impl JevProvider {
    pub fn new(config: JevConfig) -> anyhow::Result<Self> {
        Ok(Self {
            inner: JevCompatibleProvider::jev(&config)?,
        })
    }
}

#[async_trait::async_trait]
impl DecisionProvider for JevProvider {
    fn metadata(&self) -> ProviderMetadata {
        self.inner.metadata()
    }

    async fn health_check(&self) -> ProviderHealth {
        self.inner.health_check().await
    }

    async fn evaluate(&self, request: &DecisionRequest) -> Result<DecisionResult, ProviderError> {
        self.inner.evaluate(request).await
    }
}

/// Laya is the benchmark-only second decision provider, disabled by default.
pub struct LayaProvider {
    inner: JevCompatibleProvider,
}

impl LayaProvider {
    pub fn new(config: LayaConfig) -> anyhow::Result<Self> {
        Ok(Self {
            inner: JevCompatibleProvider::laya(&config)?,
        })
    }
}

#[async_trait::async_trait]
impl DecisionProvider for LayaProvider {
    fn metadata(&self) -> ProviderMetadata {
        self.inner.metadata()
    }

    async fn health_check(&self) -> ProviderHealth {
        self.inner.health_check().await
    }

    async fn evaluate(&self, request: &DecisionRequest) -> Result<DecisionResult, ProviderError> {
        self.inner.evaluate(&request.clone()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::decision::lead_questions;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer, model: Option<&str>) -> JevCompatibleProvider {
        let config = LayaConfig {
            enabled: true,
            api_key: Some("test-key".to_string()),
            base_url: server.uri(),
            model: model.map(|model| model.to_string()),
            timeout_seconds: 5,
        };
        JevCompatibleProvider::laya(&config).expect("provider")
    }

    fn request() -> DecisionRequest {
        DecisionRequest {
            state: "We cannot interview candidates who do not speak our language.".to_string(),
            questions: lead_questions(),
            idempotency_key: Some("decision-run-1".to_string()),
        }
    }

    fn answer_body() -> serde_json::Value {
        serde_json::json!({
            "id": "dec_test",
            "model": "jev-1.13",
            "model_version": "jev-1.13-20260917",
            "provider": "jev-ai.org",
            "answers": {
                "relevance": {"type": "noul", "noul": 0.93},
                "business_problem": {"type": "noul", "noul": 0.88},
                "irrelevant_noise": {"type": "noul", "noul": 0.04},
                "vertical": {"type": "choice", "choice": "hr_recruiting",
                             "probabilities": {"hr_recruiting": 0.9, "other": 0.1}, "confidence": 0.9},
                "intent": {"type": "choice", "choice": "active_problem",
                           "probabilities": {"active_problem": 0.8}, "confidence": 0.8},
                "customer_type": {"type": "choice", "choice": "b2b",
                                  "probabilities": {"b2b": 0.7}, "confidence": 0.7},
                "buying_intent": {"type": "choice", "choice": "moderate",
                                  "probabilities": {"moderate": 0.6}, "confidence": 0.6},
                "business_potential": {"type": "choice", "choice": "high",
                                       "probabilities": {"high": 0.7}, "confidence": 0.7},
                "product_fit": {"type": "choice", "choice": "good",
                                "probabilities": {"good": 0.75}, "confidence": 0.75},
                "needs_human_review": {"type": "noul", "noul": 0.2},
                "evidence_sufficient": {"type": "noul", "noul": 0.85},
                "too_ambiguous": {"type": "noul", "noul": 0.1},
                "language": {"type": "choice", "choice": "en",
                             "probabilities": {"en": 0.95}, "confidence": 0.95}
            },
            "usage": {"input_tokens": 640, "output_tokens": 70, "charged_tokens": 640},
            "latency_ms": 940
        })
    }

    #[tokio::test]
    async fn parses_a_documented_response_and_sends_documented_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .and(header("authorization", "Bearer test-key"))
            .and(header("idempotency-key", "decision-run-1"))
            .and(body_partial_json(serde_json::json!({
                "state": "We cannot interview candidates who do not speak our language.",
                "model": "multilingual"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer_body()))
            .expect(1)
            .mount(&server)
            .await;

        let provider = provider(&server, Some("multilingual"));
        let result = provider.evaluate(&request()).await.expect("decision");
        assert_eq!(result.provider, "laya");
        assert_eq!(result.model_version.as_deref(), Some("jev-1.13-20260917"));
        assert_eq!(result.provider_request_id.as_deref(), Some("dec_test"));
        assert_eq!(result.noul("relevance"), Some(0.93));
        assert_eq!(result.choice("vertical"), Some("hr_recruiting"));
        assert_eq!(result.score("relevance"), None);
        assert_eq!(result.usage.input_tokens, Some(640));
        assert_eq!(result.latency_ms, 940);
        assert!(result.mean_confidence().expect("confidence") > 0.5);
    }

    #[test]
    fn request_body_matches_the_documented_question_shape() {
        let config = LayaConfig {
            enabled: true,
            api_key: Some("test-key".to_string()),
            base_url: "http://127.0.0.1:1".to_string(),
            model: None,
            timeout_seconds: 5,
        };
        let provider = JevCompatibleProvider::laya(&config).expect("provider");
        let request = request();
        let body = provider.build_body(&request);
        let value = serde_json::to_value(&body).expect("json");

        let vertical = &value["questions"]["vertical"];
        assert_eq!(vertical["type"], "choice");
        let criteria = vertical["criteria"].as_object().expect("criteria object");
        assert!(
            criteria.contains_key("hr_recruiting"),
            "criteria: {criteria:?}"
        );
        assert!(
            !criteria.contains_key("Choice"),
            "criteria must not be wrapped in a variant name: {criteria:?}"
        );

        let relevance = &value["questions"]["relevance"];
        assert_eq!(relevance["type"], "noul");
        assert!(relevance.get("criteria").is_none(), "noul has no criteria");
    }

    #[tokio::test]
    async fn omits_the_model_when_none_is_configured() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer_body()))
            .mount(&server)
            .await;
        let provider = provider(&server, None);
        provider.evaluate(&request()).await.expect("decision");
        let received = server.received_requests().await.expect("requests");
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
        assert!(body.get("model").is_none());
    }

    #[tokio::test]
    async fn missing_answer_is_an_invalid_response() {
        let server = MockServer::start().await;
        let mut body = answer_body();
        body["answers"]
            .as_object_mut()
            .expect("answers")
            .remove("relevance");
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&server)
            .await;
        let provider = provider(&server, None);
        let error = provider.evaluate(&request()).await.expect_err("must fail");
        assert!(matches!(error, ProviderError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn malformed_json_is_an_invalid_response() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/systemone"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{\"answers\": 12"))
            .mount(&server)
            .await;
        let provider = provider(&server, None);
        let error = provider.evaluate(&request()).await.expect_err("must fail");
        assert!(matches!(error, ProviderError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn maps_http_failures_to_typed_errors() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("retry-after", "7")
                    .set_body_json(serde_json::json!({"detail": "slow down"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(serde_json::json!({"detail": "bad key"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"detail": "Invalid input: expected record"})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let provider = provider(&server, None);
        let mut errors = Vec::new();
        for _ in 0..4 {
            errors.push(provider.evaluate(&request()).await.expect_err("error"));
        }
        let rate_limited = errors.iter().find(|error| {
            matches!(
                error,
                ProviderError::RateLimited {
                    retry_after_seconds: Some(7)
                }
            )
        });
        assert!(rate_limited.is_some(), "{errors:?}");
        assert!(errors
            .iter()
            .any(|error| matches!(error, ProviderError::Unauthorized(_))));
        let invalid = errors
            .iter()
            .find(|error| matches!(error, ProviderError::InvalidRequest(_)))
            .expect("invalid request");
        assert!(!invalid.is_retryable());
        assert!(errors
            .iter()
            .any(|error| matches!(error, ProviderError::Unavailable(_))));
    }

    #[tokio::test]
    async fn unconfigured_provider_never_calls_out() {
        let config = LayaConfig {
            enabled: true,
            api_key: None,
            base_url: "http://127.0.0.1:1".to_string(),
            model: None,
            timeout_seconds: 5,
        };
        let provider = JevCompatibleProvider::laya(&config).expect("provider");
        let error = provider.evaluate(&request()).await.expect_err("must fail");
        assert!(matches!(error, ProviderError::Unconfigured(_)));
        assert_eq!(provider.health_check().await.status, "unconfigured");
    }

    #[tokio::test]
    async fn invalid_request_is_rejected_before_any_http_call() {
        let server = MockServer::start().await;
        let provider = provider(&server, None);
        let bad = DecisionRequest {
            state: "".to_string(),
            questions: lead_questions(),
            idempotency_key: None,
        };
        let error = provider.evaluate(&bad).await.expect_err("must fail");
        assert!(matches!(error, ProviderError::InvalidRequest(_)));
        assert!(server
            .received_requests()
            .await
            .expect("requests")
            .is_empty());
    }
}
