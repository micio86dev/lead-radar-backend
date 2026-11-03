//! Apify REST adapter (https://docs.apify.com/api). Access is asynchronous: starting a run returns
//! immediately, the worker polls the run, then pages through the dataset. No request ever blocks on
//! a long-running Actor, and the token is only sent in headers.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use super::{
    ProviderError, ProviderHealth, ProviderMetadata, RunStatus, SourceProvider, SourceRun,
    SourceRunRequest,
};
use crate::config::ApifyConfig;

#[derive(Debug, Clone)]
pub struct ApifyProvider {
    base_url: String,
    api_token: Option<String>,
    client: reqwest::Client,
}

impl ApifyProvider {
    pub fn new(config: ApifyConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_token: config.api_token.clone(),
            client,
        })
    }

    fn configured(&self) -> bool {
        self.api_token
            .as_deref()
            .map(|token| !token.trim().is_empty())
            .unwrap_or(false)
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn token(&self) -> Result<&str, ProviderError> {
        self.api_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| ProviderError::Unconfigured("apify".to_string()))
    }

    async fn get_json(&self, path: &str) -> Result<Value, ProviderError> {
        let response = self
            .client
            .get(self.url(path))
            .bearer_auth(self.token()?)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        self.parse(response).await
    }

    async fn post_json(&self, path: &str, body: Value) -> Result<Value, ProviderError> {
        let response = self
            .client
            .post(self.url(path))
            .bearer_auth(self.token()?)
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        self.parse(response).await
    }

    async fn parse(&self, response: reqwest::Response) -> Result<Value, ProviderError> {
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| ProviderError::Transport(error.to_string()))?;
        if !status.is_success() {
            let detail = text.chars().take(300).collect::<String>();
            return Err(match status.as_u16() {
                401 | 403 => ProviderError::Unauthorized(format!("apify: {detail}")),
                404 => ProviderError::InvalidRequest(detail),
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
        serde_json::from_str(&text)
            .map_err(|error| ProviderError::InvalidResponse(format!("{error}")))
    }
}

fn map_status(raw: &str) -> RunStatus {
    match raw {
        "READY" | "READY_TO_RUN" => RunStatus::Queued,
        "RUNNING" | "ABORTING" | "TIMING-OUT" => RunStatus::Running,
        "SUCCEEDED" => RunStatus::Succeeded,
        "FAILED" => RunStatus::Failed,
        "ABORTED" => RunStatus::Aborted,
        "TIMED-OUT" => RunStatus::TimedOut,
        _ => RunStatus::Running,
    }
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

fn parse_run(value: &Value) -> SourceRun {
    let data = value.get("data").unwrap_or(value);
    let status = string_field(data, "status").unwrap_or_else(|| "RUNNING".to_string());
    let items_count = data
        .pointer("/stats/itemCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    SourceRun {
        provider_run_id: string_field(data, "id").unwrap_or_default(),
        status: map_status(&status),
        dataset_id: string_field(data, "defaultDatasetId"),
        items_count,
        cost_usd: data.get("usageTotalUsd").and_then(Value::as_f64),
        started_at: string_field(data, "startedAt"),
        finished_at: string_field(data, "finishedAt"),
        error: data
            .pointer("/meta/origin")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

#[async_trait]
impl SourceProvider for ApifyProvider {
    fn metadata(&self) -> ProviderMetadata {
        ProviderMetadata {
            provider: "apify".to_string(),
            kind: "source".to_string(),
            model: None,
            configured: self.configured(),
            enabled: self.configured(),
            base_url: Some(self.base_url.clone()),
        }
    }

    async fn health_check(&self) -> ProviderHealth {
        if !self.configured() {
            return ProviderHealth::unconfigured("apify");
        }
        let started = std::time::Instant::now();
        match self.get_json("/v2/users/me").await {
            Ok(_) => ProviderHealth::ok(
                "apify",
                started.elapsed().as_millis().min(i64::MAX as u128) as i64,
            ),
            Err(error) => ProviderHealth::error("apify", error.to_string()),
        }
    }

    async fn start_run(&self, request: &SourceRunRequest) -> Result<SourceRun, ProviderError> {
        if request.actor_id.trim().is_empty() {
            return Err(ProviderError::InvalidRequest(
                "actor_id must not be empty".to_string(),
            ));
        }
        let mut input = request.input.clone();
        if let Value::Object(map) = &mut input {
            map.entry("maxItems").or_insert(json!(request.max_items));
        }
        let path = format!("/v2/acts/{}/runs", request.actor_id);
        let mut body = json!({ "input": input, "maxItems": request.max_items });
        if let Some(cap) = request.max_total_charge_usd {
            // Enforced by Apify: a pay-per-event run cannot bill beyond this cap.
            body["maxTotalChargeUsd"] = json!(cap);
        }
        let value = self.post_json(&path, body).await?;
        Ok(parse_run(&value))
    }

    async fn get_run(&self, provider_run_id: &str) -> Result<SourceRun, ProviderError> {
        let value = self
            .get_json(&format!("/v2/actor-runs/{provider_run_id}"))
            .await?;
        Ok(parse_run(&value))
    }

    async fn fetch_items(
        &self,
        dataset_id: &str,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Value>, ProviderError> {
        let path = format!(
            "/v2/datasets/{dataset_id}/items?format=json&clean=true&offset={}&limit={}",
            offset.max(0),
            limit.clamp(1, 1000)
        );
        match self.get_json(&path).await? {
            Value::Array(items) => Ok(items),
            other => Err(ProviderError::InvalidResponse(format!(
                "dataset items must be an array, got {other}"
            ))),
        }
    }

    async fn cancel_run(&self, provider_run_id: &str) -> Result<(), ProviderError> {
        self.post_json(
            &format!("/v2/actor-runs/{provider_run_id}/abort"),
            json!({}),
        )
        .await
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn provider(server: &MockServer) -> ApifyProvider {
        ApifyProvider::new(ApifyConfig {
            api_token: Some("apify-token".to_string()),
            base_url: server.uri(),
            max_concurrent_runs: 1,
            max_items_per_run: 10,
            max_run_cost_eur: 0.5,
            max_total_charge_usd: 0.25,
            daily_budget_eur: 1.0,
            total_budget_eur: 25.0,
            kill_switch: false,
            run_timeout_seconds: 60,
            poll_interval_seconds: 1,
        })
        .expect("provider")
    }

    #[tokio::test]
    async fn starts_a_run_without_blocking() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/acts/test~actor/runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": {
                    "id": "run-1",
                    "status": "READY",
                    "defaultDatasetId": "dataset-1",
                    "usageTotalUsd": 0.0,
                    "startedAt": "2024-05-01T09:00:00Z"
                }
            })))
            .mount(&server)
            .await;
        let run = provider(&server)
            .start_run(&SourceRunRequest {
                actor_id: "test~actor".to_string(),
                input: json!({"query": "x"}),
                max_items: 5,
                max_total_charge_usd: Some(0.1),
            })
            .await
            .expect("start");
        assert_eq!(run.provider_run_id, "run-1");
        assert_eq!(run.status, RunStatus::Queued);
        assert_eq!(run.dataset_id.as_deref(), Some("dataset-1"));
    }

    #[tokio::test]
    async fn the_run_carries_the_provider_side_charge_cap() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v2/acts/test~actor/runs"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "data": {"id": "run-cap", "status": "READY", "defaultDatasetId": "dataset-cap"}
            })))
            .mount(&server)
            .await;
        provider(&server)
            .start_run(&SourceRunRequest {
                actor_id: "test~actor".to_string(),
                input: json!({"search": "multilingual interviews"}),
                max_items: 5,
                max_total_charge_usd: Some(0.1),
            })
            .await
            .expect("start");

        let received = server.received_requests().await.expect("requests");
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).expect("json body");
        assert_eq!(body["maxItems"], 5);
        assert_eq!(body["maxTotalChargeUsd"], 0.1);
        assert_eq!(body["input"]["search"], "multilingual interviews");
    }

    #[tokio::test]
    async fn maps_run_statuses_and_cost() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/actor-runs/run-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "id": "run-2",
                    "status": "SUCCEEDED",
                    "defaultDatasetId": "dataset-2",
                    "usageTotalUsd": 0.0312,
                    "finishedAt": "2024-05-01T09:10:00Z"
                }
            })))
            .mount(&server)
            .await;
        let run = provider(&server).get_run("run-2").await.expect("run");
        assert_eq!(run.status, RunStatus::Succeeded);
        assert_eq!(run.cost_usd, Some(0.0312));
        assert!(run.status.terminal());
    }

    #[tokio::test]
    async fn pages_dataset_items() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/datasets/dataset-1/items"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"id": "1", "text": "a"},
                {"id": "2", "text": "b"}
            ])))
            .mount(&server)
            .await;
        let items = provider(&server)
            .fetch_items("dataset-1", 0, 100)
            .await
            .expect("items");
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn unconfigured_provider_is_reported_not_faked() {
        let provider = ApifyProvider::new(ApifyConfig {
            api_token: None,
            base_url: "http://127.0.0.1:1".to_string(),
            max_concurrent_runs: 1,
            max_items_per_run: 10,
            max_run_cost_eur: 0.5,
            max_total_charge_usd: 0.25,
            daily_budget_eur: 1.0,
            total_budget_eur: 25.0,
            kill_switch: false,
            run_timeout_seconds: 60,
            poll_interval_seconds: 1,
        })
        .expect("provider");
        assert_eq!(provider.health_check().await.status, "unconfigured");
        let error = provider
            .start_run(&SourceRunRequest {
                actor_id: "a".to_string(),
                input: json!({}),
                max_items: 1,
                max_total_charge_usd: None,
            })
            .await
            .expect_err("must fail");
        assert!(matches!(error, ProviderError::Unconfigured(_)));
    }

    #[tokio::test]
    async fn upstream_errors_are_typed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/actor-runs/unknown"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;
        let error = provider(&server)
            .get_run("unknown")
            .await
            .expect_err("fail");
        assert!(matches!(error, ProviderError::InvalidRequest(_)));
        assert!(!error.is_retryable());
    }
}
