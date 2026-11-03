//! Shared test harness: an isolated SQLite database, the real router, seeded data and offline
//! providers. No test ever reaches a paid service.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::Value;
use tower::ServiceExt;

use lead_radar::api;
use lead_radar::auth::hash_token;
use lead_radar::config::AppConfig;
use lead_radar::db::Db;
use lead_radar::domain::enums::Role;
use lead_radar::modules::tenancy;
use lead_radar::seed;
use lead_radar::state::{AppState, SharedState};

pub const DEV_TOKEN: &str = "test-dev-token";
pub const TENANT_A: &str = "00000000-0000-0000-0000-000000000001";
pub const TENANT_B: &str = "00000000-0000-0000-0000-0000000000b1";
pub const USER_B: &str = "00000000-0000-0000-0000-0000000000b2";
pub const TOKEN_B: &str = "tenant-b-token";

pub struct TestApp {
    pub state: SharedState,
    pub router: Router,
    _dir: tempfile::TempDir,
}

impl TestApp {
    pub fn db(&self) -> &Db {
        &self.state.db
    }

    pub fn tenant(&self) -> &str {
        TENANT_A
    }
}

pub async fn setup() -> TestApp {
    setup_with("mock", "mock", "mock").await
}

pub async fn setup_with(decision: &str, generative: &str, source: &str) -> TestApp {
    let dir = tempfile::tempdir().expect("tempdir");
    let database_url = format!("sqlite://{}/test.db?mode=rwc", dir.path().display());
    let mut vars: HashMap<String, String> = HashMap::new();
    let entries = [
        ("APP_ENV", "test"),
        ("APP_HOST", "127.0.0.1"),
        ("APP_PORT", "8080"),
        ("LOG_LEVEL", "warn"),
        ("DEFAULT_TENANT_ID", TENANT_A),
        ("DEV_API_TOKEN", DEV_TOKEN),
        ("DECISION_PROVIDER", decision),
        ("GENERATIVE_PROVIDER", generative),
        ("SOURCE_PROVIDER", source),
        ("WORKER_POLL_INTERVAL", "1"),
        ("APIFY_POLL_INTERVAL", "1"),
        ("JOB_RETRY_BACKOFF", "1"),
        ("CORS_ALLOWED_ORIGINS", "http://localhost:4321"),
    ];
    for (key, value) in entries {
        vars.insert(key.to_string(), value.to_string());
    }
    vars.insert("DATABASE_URL".to_string(), database_url);

    let config = AppConfig::from_map(&vars).expect("test configuration");
    let state = Arc::new(AppState::build(config).await.expect("state"));
    state.db.migrate().await.expect("migrations");
    let config = state.config.clone();
    seed::seed_all(&config, &state.db).await.expect("seed");
    seed_second_tenant(&state).await;

    let router = api::router(state.clone());
    TestApp {
        state,
        router,
        _dir: dir,
    }
}

/// A second tenant with its own token, used to prove isolation.
pub async fn seed_second_tenant(state: &AppState) {
    tenancy::ensure_tenant(&state.db, TENANT_B, "Tenant B", "tenant-b")
        .await
        .expect("tenant b");
    tenancy::ensure_user(&state.db, USER_B, "b@example.invalid", "Tenant B User")
        .await
        .expect("user b");
    tenancy::ensure_membership(&state.db, TENANT_B, USER_B, Role::Viewer)
        .await
        .expect("membership b");
    tenancy::ensure_api_token(
        &state.db,
        TENANT_B,
        USER_B,
        &hash_token(TOKEN_B),
        "tenant b token",
    )
    .await
    .expect("token b");
}

pub async fn call(
    app: &TestApp,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(body) => builder
            .body(Body::from(serde_json::to_vec(&body).expect("json body")))
            .expect("request"),
        None => builder.body(Body::empty()).expect("request"),
    };
    let response = app.router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()))
    };
    (status, value)
}

pub async fn get(app: &TestApp, path: &str) -> (StatusCode, Value) {
    call(app, "GET", path, Some(DEV_TOKEN), None).await
}

pub async fn get_as(app: &TestApp, path: &str, token: &str) -> (StatusCode, Value) {
    call(app, "GET", path, Some(token), None).await
}

pub async fn post(app: &TestApp, path: &str, body: Value) -> (StatusCode, Value) {
    call(app, "POST", path, Some(DEV_TOKEN), Some(body)).await
}

pub async fn patch(app: &TestApp, path: &str, body: Value) -> (StatusCode, Value) {
    call(app, "PATCH", path, Some(DEV_TOKEN), Some(body)).await
}

/// A post that passes the seeded keyword filter, the mock decision and the qualification gates.
pub fn relevant_post(published_at: &str) -> Value {
    serde_json::json!({
        "external_id": "manual-1",
        "title": "Multilingual interviews are blocking our hiring",
        "content": "Our recruiting team cannot interview candidates who only speak Spanish. \
                    We need real-time translation in the interviews, any suggestions? \
                    This is an international hiring problem we face every week.",
        "url": "https://example.invalid/discussion/1",
        "author": "hiring-lead",
        "community": "recruiting",
        "language": "en",
        "published_at": published_at,
        "engagement": {"comments": 3, "upvotes": 7, "shares": 0}
    })
}

pub fn now_minus_hours(hours: i64) -> String {
    let when = chrono::Utc::now() - chrono::Duration::hours(hours);
    lead_radar::domain::time::to_iso(when)
}

/// Import one relevant post and drive the pipeline to a lead.
pub async fn import_and_lead(app: &TestApp) -> String {
    let payload_post = relevant_post(&now_minus_hours(1));
    let (status, body) = post(
        app,
        "/api/v1/import/json",
        serde_json::json!({"source_name": "manual-import", "posts": [payload_post]}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["imported"], 1, "{body}");
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");
    let (status, body) = get(app, "/api/v1/opportunities").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().expect("items");
    assert!(!items.is_empty(), "a lead should exist: {body}");
    items[0]["id"].as_str().expect("lead id").to_string()
}
