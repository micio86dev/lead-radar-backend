//! Contract tests for the HTTP surface: auth, isolation, the pipeline, the review workflow, the spend
//! guard and provider visibility.

mod common;

use axum::http::StatusCode;
use serde_json::json;

use common::{
    call, get, get_as, import_and_lead, now_minus_hours, patch, post, relevant_post, setup,
    setup_with, TOKEN_B,
};
use lead_radar::domain::enums::Vertical;
use lead_radar::seed;

#[tokio::test]
async fn health_and_ready_are_unauthenticated() {
    let app = setup().await;
    let (status, body) = call(&app, "GET", "/health", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");

    let (status, body) = call(&app, "GET", "/ready", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["database"], "ok");
}

#[tokio::test]
async fn requests_without_a_valid_token_are_rejected() {
    let app = setup().await;
    let (status, body) = call(&app, "GET", "/api/v1/opportunities", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");

    let (status, _) = call(&app, "GET", "/api/v1/opportunities", Some("wrong"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn import_pipeline_creates_one_qualified_lead() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;

    let (status, detail) = get(&app, &format!("/api/v1/opportunities/{lead_id}")).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["qualification_status"], "qualified", "{detail}");
    assert!(detail["score"].as_i64().expect("score") >= 60, "{detail}");
    assert_eq!(detail["intent"], "explicit_solution_request");
    assert_eq!(detail["vertical_primary"], "hr_recruiting");
    assert!(!detail["evidence"].as_array().expect("evidence").is_empty());
    assert_eq!(detail["decision"]["provider"], "mock");
    assert!(
        detail["score_breakdown"]["components"]
            .as_array()
            .expect("components")
            .len()
            >= 4
    );
}

#[tokio::test]
async fn duplicate_imports_do_not_create_a_second_lead() {
    let app = setup().await;
    let first = import_and_lead(&app).await;
    let payload_post = relevant_post(&now_minus_hours(1));
    let (status, body) = post(
        &app,
        "/api/v1/import/json",
        json!({"posts": [payload_post]}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    assert_eq!(body["imported"], 0, "{body}");
    assert_eq!(body["duplicates"], 1, "{body}");
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    let (status, body) = get(&app, "/api/v1/opportunities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1, "{body}");
    assert_eq!(body["items"][0]["id"], first);
}

#[tokio::test]
async fn tenant_isolation_is_enforced_for_every_resource() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;

    let (status, body) = get_as(&app, "/api/v1/opportunities", TOKEN_B).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["total"], 0,
        "tenant B must not see tenant A leads: {body}"
    );

    let (status, _) = get_as(&app, &format!("/api/v1/opportunities/{lead_id}"), TOKEN_B).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Tenant B cannot reach tenant A comments either.
    let (status, body) = get_as(&app, "/api/v1/comments", TOKEN_B).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);

    let (status, _) = get_as(&app, "/api/v1/dashboard", TOKEN_B).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn comment_workflow_requires_explicit_human_actions() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;

    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/generate-comments"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    let (status, body) = get(&app, &format!("/api/v1/comments?lead_id={lead_id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let comments = body["items"].as_array().expect("comments");
    assert_eq!(comments.len(), 3, "three variants expected: {body}");
    for comment in comments {
        assert_eq!(comment["status"], "pending_review");
        assert_eq!(comment["language"], "en");
    }

    let helpful = comments
        .iter()
        .find(|comment| comment["variant"] == "helpful")
        .expect("helpful variant");
    let comment_id = helpful["id"].as_str().expect("id");

    // Publication is refused before approval.
    let (status, body) = post(
        &app,
        &format!("/api/v1/comments/{comment_id}/record-publication"),
        json!({"note": "posted by hand"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Editing bumps the version.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/comments/{comment_id}"),
        json!({"body": "A shared glossary of role-specific terms, agreed before the interview, removes most of the friction here."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], 2);

    let (status, body) = post(
        &app,
        &format!("/api/v1/comments/{comment_id}/approve"),
        json!({"note": "grounded and useful"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "approved");

    let (status, body) = post(
        &app,
        &format!("/api/v1/comments/{comment_id}/record-publication"),
        json!({"note": "posted by hand", "published_at": "2024-05-02T09:00:00Z"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "published_manually");

    // A published comment cannot be rejected afterwards.
    let (status, _) = post(
        &app,
        &format!("/api/v1/comments/{comment_id}/reject"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A rejected comment is terminal as well.
    let direct = comments
        .iter()
        .find(|comment| comment["variant"] == "direct_pitch")
        .expect("direct pitch");
    let direct_id = direct["id"].as_str().expect("id");
    let (status, body) = post(
        &app,
        &format!("/api/v1/comments/{direct_id}/reject"),
        json!({"note": "too salesy"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "rejected");
}

#[tokio::test]
async fn unconfigured_decision_provider_fails_visibly_and_creates_no_lead() {
    let app = setup_with("jev", "mock", "mock").await;
    let payload_post = relevant_post(&now_minus_hours(1));
    let (status, body) = post(
        &app,
        "/api/v1/import/json",
        json!({"posts": [payload_post]}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    let (status, body) = get(&app, "/api/v1/jobs?status=dead_letter").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["total"].as_i64().expect("total") >= 1, "{body}");
    let error = body["items"][0]["last_error"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(error.contains("not configured"), "{error}");

    let (status, body) = get(&app, "/api/v1/opportunities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0, "{body}");

    let (status, body) = get(&app, "/api/v1/providers").await;
    assert_eq!(status, StatusCode::OK);
    let jev = body
        .as_array()
        .expect("providers")
        .iter()
        .find(|provider| provider["provider"] == "jev")
        .expect("jev status");
    assert_eq!(jev["status"], "unconfigured");
    // A failure is not reported as stale while it is recent, and no success was ever recorded.
    assert_eq!(jev["stale"], false);
    assert_eq!(jev["last_success_at"], serde_json::Value::Null);
    assert!(jev.get("last_error_at").is_some());
}

#[tokio::test]
async fn apify_runs_pass_the_cost_guard_before_being_queued() {
    let app = setup().await;
    let (status, actor) = post(
        &app,
        "/api/v1/providers/apify/actors",
        json!({
            "actor_id": "operator/supplied-actor",
            "name": "Operator actor",
            "source_type": "community_feed",
            "max_items_per_run": 5
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{actor}");
    assert_eq!(actor["enabled"], false);
    assert_eq!(actor["validation_status"], "pending");
    let actor_id = actor["id"].as_str().expect("actor id");

    let (status, body) = post(
        &app,
        "/api/v1/providers/apify/runs",
        json!({"actor_registry_id": actor_id, "items_limit": 5}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "cost_guard_rejected");
    assert_eq!(body["error"]["details"]["reason"], "actor_not_enabled");

    // Enabling without validation is refused.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/providers/apify/actors/{actor_id}"),
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Validate and enable explicitly, then the same call is accepted.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/providers/apify/actors/{actor_id}"),
        json!({"validation_status": "validated", "enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["validation_status"], "validated");
    assert_eq!(body["enabled"], true);

    let (status, body) = post(
        &app,
        "/api/v1/providers/apify/runs",
        json!({"actor_registry_id": actor_id, "items_limit": 5}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], "queued");

    lead_radar::worker::drain(&app.state, 30)
        .await
        .expect("drain");
    let (status, body) = get(&app, "/api/v1/providers/apify/runs").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["total"].as_i64().expect("total") >= 1);

    // The run lifecycle is asynchronous: drain, then wait for the poll requeue to finish it.
    let mut run_status = String::new();
    let mut listing = serde_json::Value::Null;
    for _ in 0..20 {
        lead_radar::worker::drain(&app.state, 30)
            .await
            .expect("drain");
        let (_, current) = get(&app, "/api/v1/providers/apify/runs").await;
        listing = current;
        run_status = listing["items"][0]["status"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if run_status == "succeeded" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    assert_eq!(run_status, "succeeded", "{listing}");

    // Re-processing the collected dataset needs no new provider run.
    let succeeded = listing["items"]
        .as_array()
        .expect("runs")
        .iter()
        .find(|run| run["status"] == "succeeded")
        .unwrap_or_else(|| panic!("succeeded run missing in {listing}"));
    let run_id = succeeded["id"].as_str().expect("run id");
    let (status, body) = post(
        &app,
        &format!("/api/v1/providers/apify/runs/{run_id}/reingest"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert!(body["job_id"].is_string());

    // A zero daily ceiling blocks the next billable run.
    let (status, _) = patch(
        &app,
        "/api/v1/settings",
        json!({"budgets": {"apify_daily_budget_eur": 0.0}}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = post(
        &app,
        "/api/v1/providers/apify/runs",
        json!({"actor_registry_id": actor_id, "items_limit": 5}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["details"]["reason"], "daily_ceiling_reached");
}

#[tokio::test]
async fn communities_and_sources_cannot_be_enabled_without_validation() {
    let app = setup().await;
    let (status, body) = get(&app, "/api/v1/communities?validation_status=pending").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["total"].as_i64().expect("total") >= 24, "{body}");
    let community = &body["items"][0];
    assert_eq!(community["enabled"], false);
    let id = community["id"].as_str().expect("id");

    let (status, body) = patch(
        &app,
        &format!("/api/v1/communities/{id}"),
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Evidence is mandatory.
    let (status, body) = post(
        &app,
        &format!("/api/v1/communities/{id}/validate"),
        json!({"outcome": "validated", "evidence": "  "}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = post(
        &app,
        &format!("/api/v1/communities/{id}/validate"),
        json!({"outcome": "validated", "evidence": "public subreddit rules allow business participation"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["validation_status"], "validated");

    let (status, body) = patch(
        &app,
        &format!("/api/v1/communities/{id}"),
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);
}

#[tokio::test]
async fn seeds_activate_scoring_version_two() {
    let app = setup().await;
    let active: (i64, String, String) = sqlx::query_as(
        "SELECT version, weights, thresholds FROM lead_score_versions \
         WHERE tenant_id = ? AND active = 1",
    )
    .bind(app.tenant())
    .fetch_one(&app.db().reader)
    .await
    .expect("active scoring version");
    assert_eq!(active.0, 2);
    assert!(active.1.contains("\"product_fit\":25"));
    assert!(
        active.2.contains("\"conferences_webinars\":50"),
        "conference threshold lowered: {}",
        active.2
    );
}

#[tokio::test]
async fn communities_can_be_added_and_are_born_pending() {
    let app = setup().await;
    let (status, body) = post(
        &app,
        "/api/v1/communities",
        json!({"name": "CrossLanguageCalls", "vertical": "international_meetings", "notes": "on-topic for live calls across languages"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["validation_status"], "pending");
    assert_eq!(body["enabled"], false);
    assert_eq!(body["vertical"], "international_meetings");

    // The same name cannot be added twice.
    let (status, body) = post(
        &app,
        "/api/v1/communities",
        json!({"name": "CrossLanguageCalls", "vertical": "international_meetings"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // An unknown vertical is rejected.
    let (status, _) = post(
        &app,
        "/api/v1/communities",
        json!({"name": "something", "vertical": "not_a_vertical"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn sources_require_recorded_validation_before_enabling() {
    let app = setup().await;
    let (status, body) = get(&app, "/api/v1/sources").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let source = body["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|source| source["name"] == "apify-default")
        .expect("seeded apify source")
        .clone();
    let id = source["id"].as_str().expect("id").to_string();
    assert_eq!(source["enabled"], false);

    // Enabling without validation is refused.
    let (status, body) = patch(
        &app,
        &format!("/api/v1/sources/{id}"),
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or_default()
        .starts_with("source_not_validated:"));

    // Evidence is mandatory.
    let (status, body) = post(
        &app,
        &format!("/api/v1/sources/{id}/validate"),
        json!({"outcome": "validated", "evidence": "   "}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = post(
        &app,
        &format!("/api/v1/sources/{id}/validate"),
        json!({
            "outcome": "validated",
            "evidence": "operator relevance review; collection through an authorized Actor",
            "notes": "relevance review"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["validation_status"], "validated");
    assert!(body["notes"]
        .as_str()
        .unwrap_or_default()
        .contains("authorized Actor"));

    let (status, body) = patch(
        &app,
        &format!("/api/v1/sources/{id}"),
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], true);

    // The reachability probe reports the adapter state; it never fabricates a success.
    let (status, body) = post(&app, &format!("/api/v1/sources/{id}/test"), json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "ok");
}

#[tokio::test]
async fn search_profiles_are_created_and_filtered_with_configured_keywords() {
    let app = setup().await;
    let (status, body) = get(&app, "/api/v1/search-profiles").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 5, "five verticals: {body}");
    let hr = body["items"]
        .as_array()
        .expect("profiles")
        .iter()
        .find(|profile| profile["vertical"] == "hr_recruiting")
        .expect("hr profile");
    assert_eq!(hr["keywords"].as_array().expect("keywords").len(), 10);

    let (status, body) = post(
        &app,
        "/api/v1/search-profiles",
        json!({
            "name": "extra profile",
            "vertical": "customer_support",
            "keywords": ["multilingual call center"],
            "negative_keywords": ["dictionary questions"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().expect("id");

    let (status, body) = patch(
        &app,
        &format!("/api/v1/search-profiles/{id}"),
        json!({"keywords": ["live translation customer service"]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["keywords"],
        json!(["live translation customer service"])
    );

    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/search-profiles/{id}"),
        Some(common::DEV_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn dashboard_reports_real_counts_and_provider_state() {
    let app = setup().await;
    import_and_lead(&app).await;
    let (status, body) = get(&app, "/api/v1/dashboard").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["totals"]["opportunities"], 1);
    assert_eq!(
        body["totals"]["hot"].as_i64().unwrap_or(0) + body["totals"]["warm"].as_i64().unwrap_or(0),
        1
    );
    assert!(body["providers"].as_array().expect("providers").len() == 4);
    assert_eq!(body["apify"]["cost_source"], "unknown");
    assert!(body["costs"].as_array().expect("costs").len() == 4);
}

#[tokio::test]
async fn benchmark_runs_against_the_seeded_evaluation_split() {
    let app = setup().await;
    let (status, body) = post(
        &app,
        "/api/v1/providers/benchmark",
        json!({"dataset": "seed", "providers": ["jev"]}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["benchmark_run_id"].as_str().expect("run id");
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    let (status, body) = get(&app, &format!("/api/v1/providers/benchmark/{run_id}")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "succeeded", "{body}");
    assert_eq!(body["item_count"], 12);
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    let metrics = &results[0]["metrics"];
    assert_eq!(metrics["count"], 12);
    assert!(metrics["precision"].as_f64().expect("precision") >= 0.0);
    assert!(metrics["confusion_matrix"]["tp"].as_i64().expect("tp") > 0);
    assert_eq!(results[0]["cost_per_1000_usd"], serde_json::Value::Null);
}

#[tokio::test]
async fn seeds_are_idempotent() {
    let app = setup().await;
    let config = app.state.config.clone();
    let before = counts(&app).await;
    seed::seed_all(&config, app.db())
        .await
        .expect("second seed");
    let after = counts(&app).await;
    assert_eq!(before, after, "seed must not duplicate rows");
}

async fn counts(app: &common::TestApp) -> (i64, i64, i64, i64) {
    let tenants: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tenants")
        .fetch_one(&app.db().reader)
        .await
        .expect("tenants");
    let keywords: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM keywords")
        .fetch_one(&app.db().reader)
        .await
        .expect("keywords");
    let communities: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM communities")
        .fetch_one(&app.db().reader)
        .await
        .expect("communities");
    let benchmarks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM benchmark_items")
        .fetch_one(&app.db().reader)
        .await
        .expect("benchmark items");
    (tenants, keywords, communities, benchmarks)
}

#[tokio::test]
async fn jobs_can_be_retried_and_dead_letters_are_visible() {
    let app = setup_with("jev", "mock", "mock").await;
    let payload_post = relevant_post(&now_minus_hours(1));
    let (status, _) = post(
        &app,
        "/api/v1/import/json",
        json!({"posts": [payload_post]}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED);
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    let (status, body) = get(&app, "/api/v1/jobs?status=dead_letter").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let job_id = body["items"][0]["id"].as_str().expect("job id");

    let (status, body) = post(&app, &format!("/api/v1/jobs/{job_id}/retry"), json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], "queued");

    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");
    let (status, body) = get(&app, "/api/v1/jobs").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["total"].as_i64().expect("total") >= 2);
}

#[tokio::test]
async fn engagements_are_recorded_manually_and_scoped_to_the_tenant() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;
    let (status, body) = post(
        &app,
        "/api/v1/engagements",
        json!({"lead_id": lead_id, "kind": "reply", "note": "answered in the thread"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "reply");

    let (status, body) = get(&app, &format!("/api/v1/engagements?lead_id={lead_id}")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);

    let (status, _) = get_as(&app, "/api/v1/engagements", TOKEN_B).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = post(
        &app,
        "/api/v1/engagements",
        json!({"lead_id": "00000000-0000-0000-0000-00000000dead", "kind": "reply"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn content_removal_hides_the_opportunity_and_keeps_an_audit_trail() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;
    let (status, _) = call(
        &app,
        "DELETE",
        &format!("/api/v1/opportunities/{lead_id}"),
        Some(common::DEV_TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = get(&app, "/api/v1/opportunities").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 0);

    let audits: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_logs WHERE action = 'content_removed'")
            .fetch_one(&app.db().reader)
            .await
            .expect("audits");
    assert_eq!(audits, 1);
}

#[tokio::test]
async fn verticals_are_seeded_and_validated_on_input() {
    let app = setup().await;
    assert_eq!(Vertical::ALL.len(), 6);
    let (status, body) = get(&app, "/api/v1/opportunities?vertical=hr_recruiting").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = get(&app, "/api/v1/opportunities?vertical=hr").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = get(&app, "/api/v1/opportunities?published_after=yesterday").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}
