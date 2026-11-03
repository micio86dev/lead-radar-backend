//! Qwen generative adapter for an OpenAI-compatible chat-completions endpoint.
//!
//! Nothing here is hardcoded: base URL, model and limits come from `QWEN_*`. The adapter never
//! falls back to another paid provider, never retries on its own (the worker owns bounded retries)
//! and rejects non-object JSON so a malformed generation cannot reach a comment draft.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{
    GenerativeProvider, GenerativeResult, ProviderError, ProviderHealth, ProviderMetadata,
};
use crate::config::QwenConfig;

#[derive(Debug, Clone)]
pub struct QwenProvider {
    endpoint: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    max_tokens: i64,
    client: reqwest::Client,
}

impl QwenProvider {
    pub fn new(config: QwenConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_seconds.max(5) as u64))
            .build()?;
        Ok(Self {
            endpoint: config.endpoint(),
            base_url: config.base_url.clone(),
            api_key: config.api_key.clone(),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            client,
        })
    }

    fn configured(&self) -> bool {
        self.api_key
            .as_deref()
            .map(|key| !key.trim().is_empty())
            .unwrap_or(false)
            && self.endpoint.is_some()
            && self.model.is_some()
    }
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: [ChatMessage<'a>; 2],
    temperature: f64,
    max_tokens: i64,
    response_format: ChatResponseFormat,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct ChatResponseFormat {
    #[serde(rename = "type")]
    kind: &'static str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: ChatUsage,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Debug, Deserialize)]
struct ChatChoiceMessage {
    #[serde(default)]
    content: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: Option<i64>,
    #[serde(default)]
    completion_tokens: Option<i64>,
}

#[async_trait]
impl GenerativeProvider for QwenProvider {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            provider: "qwen".to_string(),
            kind: "generative".to_string(),
            model: self.model.clone(),
            configured: self.configured(),
            enabled: self.configured(),
            base_url: self.base_url.clone(),
        }
    }

    /// Billed minimal probe used only by the explicit provider test endpoint.
    async fn health_check(&self) -> ProviderHealth {
        if !self.configured() {
            return ProviderHealth::unconfigured("qwen");
        }
        let started = Instant::now();
        match self
            .generate_json("You answer with JSON only.", "Return {\"ok\": true}.", "ok")
            .await
        {
            Ok(_) => ProviderHealth::ok(
                "qwen",
                started.elapsed().as_millis().min(i64::MAX as u128) as i64,
            ),
            Err(error) => ProviderHealth::error("qwen", error.to_string()),
        }
    }

    async fn generate_json(
        &self,
        system: &str,
        user: &str,
        schema_hint: &str,
    ) -> Result<GenerativeResult, ProviderError> {
        let endpoint = self
            .endpoint
            .as_deref()
            .ok_or_else(|| ProviderError::Unconfigured("qwen".to_string()))?;
        let api_key = self
            .api_key
            .as_deref()
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| ProviderError::Unconfigured("qwen".to_string()))?;
        let model = self
            .model
            .as_deref()
            .ok_or_else(|| ProviderError::Unconfigured("qwen".to_string()))?;

        let schema_instruction = format!(
            "{user}\n\nRespond with a single JSON object matching: {schema_hint}. No prose, no markdown."
        );
        let body = ChatRequest {
            model,
            messages: [
                ChatMessage {
                    role: "system",
                    content: system,
                },
                ChatMessage {
                    role: "user",
                    content: &schema_instruction,
                },
            ],
            temperature: 0.2,
            max_tokens: self.max_tokens,
            response_format: ChatResponseFormat {
                kind: "json_object",
            },
            stream: false,
        };

        let started = Instant::now();
        let response = self
            .client
            .post(endpoint)
            .bearer_auth(api_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        if !status.is_success() {
            let detail = extract_message(&text);
            return Err(match status.as_u16() {
                401 | 403 => ProviderError::Unauthorized(format!("qwen: {detail}")),
                400 | 404 | 422 => ProviderError::InvalidRequest(detail),
                429 => ProviderError::RateLimited {
                    retry_after_seconds: None,
                },
                500..=599 => ProviderError::Unavailable(detail),
                other => ProviderError::Upstream {
                    status: other,
                    message: detail,
                },
            });
        }

        let parsed: ChatResponse = serde_json::from_str(&text)
            .map_err(|error| ProviderError::InvalidResponse(format!("{error}")))?;
        let choice = parsed
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::InvalidResponse("no choices returned".to_string()))?;
        let content = choice
            .message
            .content
            .ok_or_else(|| ProviderError::InvalidResponse("empty message content".to_string()))?;
        let value: serde_json::Value = serde_json::from_str(&content).map_err(|error| {
            ProviderError::InvalidResponse(format!("content is not JSON: {error}"))
        })?;
        if !value.is_object() {
            return Err(ProviderError::InvalidResponse(
                "content must be a JSON object".to_string(),
            ));
        }

        Ok(GenerativeResult {
            provider: "qwen".to_string(),
            model: parsed.model.unwrap_or_else(|| model.to_string()),
            request_id: parsed.id,
            content: value,
            input_tokens: parsed.usage.prompt_tokens,
            output_tokens: parsed.usage.completion_tokens,
            latency_ms: started.elapsed().as_millis().min(i64::MAX as u128) as i64,
        })
    }
}

fn extract_message(body: &str) -> String {
    let parsed: Option<serde_json::Value> = serde_json::from_str(body).ok();
    parsed
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/error"))
                .and_then(|found| found.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| body.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer, key: Option<&str>) -> QwenProvider {
        QwenProvider::new(QwenConfig {
            api_key: key.map(str::to_string),
            base_url: Some(server.uri()),
            model: Some("qwen-test".to_string()),
            timeout_seconds: 5,
            max_tokens: 256,
        })
        .expect("provider")
    }

    #[tokio::test]
    async fn sends_the_documented_chat_request_and_parses_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer secret"))
            .and(body_partial_json(json!({
                "model": "qwen-test",
                "response_format": {"type": "json_object"},
                "max_tokens": 256
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-1",
                "model": "qwen-test",
                "choices": [{"message": {"content": "{\"summary\": \"grounded\"}"}}],
                "usage": {"prompt_tokens": 120, "completion_tokens": 20}
            })))
            .mount(&server)
            .await;

        let result = provider(&server, Some("secret"))
            .generate_json("system", "user", "summary")
            .await
            .expect("generation");
        assert_eq!(result.content["summary"], "grounded");
        assert_eq!(result.input_tokens, Some(120));
        assert_eq!(result.output_tokens, Some(20));
        assert_eq!(result.request_id.as_deref(), Some("chatcmpl-1"));
    }

    #[tokio::test]
    async fn non_json_content_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "sure! here is the JSON: {oops"}}]
            })))
            .mount(&server)
            .await;
        let error = provider(&server, Some("secret"))
            .generate_json("s", "u", "hint")
            .await
            .expect_err("must fail");
        assert!(matches!(error, ProviderError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn non_object_json_is_rejected() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{"message": {"content": "[1,2,3]"}}]
            })))
            .mount(&server)
            .await;
        let error = provider(&server, Some("secret"))
            .generate_json("s", "u", "hint")
            .await
            .expect_err("must fail");
        assert!(matches!(error, ProviderError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn rate_limits_and_outages_are_typed_and_retryable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let qwen = provider(&server, Some("secret"));
        let first = qwen
            .generate_json("s", "u", "hint")
            .await
            .expect_err("rate limited");
        let second = qwen
            .generate_json("s", "u", "hint")
            .await
            .expect_err("unavailable");
        assert!(first.is_retryable());
        assert!(second.is_retryable());
    }

    #[tokio::test]
    async fn unconfigured_provider_never_calls_out() {
        let qwen = QwenProvider::new(QwenConfig {
            api_key: None,
            base_url: None,
            model: None,
            timeout_seconds: 5,
            max_tokens: 100,
        })
        .expect("provider");
        assert_eq!(qwen.health_check().await.status, "unconfigured");
        let error = qwen
            .generate_json("s", "u", "hint")
            .await
            .expect_err("must fail");
        assert!(matches!(error, ProviderError::Unconfigured(_)));
    }
}
