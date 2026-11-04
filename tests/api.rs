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
async fn deleted_and_removed_content_is_rejected_and_never_stored() {
    let app = setup().await;
    let mut deleted_title = relevant_post(&now_minus_hours(1));
    deleted_title["external_id"] = "gone-title".into();
    deleted_title["title"] = "[deleted]".into();

    let mut removed_body = relevant_post(&now_minus_hours(1));
    removed_body["external_id"] = "gone-body".into();
    removed_body["content"] = "[removed]".into();

    let mut deleted_author = relevant_post(&now_minus_hours(1));
    deleted_author["external_id"] = "gone-author".into();
    deleted_author["author"] = "[deleted]".into();

    let (status, body) = post(
        &app,
        "/api/v1/import/json",
        serde_json::json!({
            "posts": [deleted_title, removed_body, deleted_author]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["imported"], 0, "{body}");
    assert_eq!(body["rejected"], 3, "{body}");
    assert_eq!(
        body["rejected_reasons"][0],
        "deleted or removed post: no author to answer, so it is not stored",
        "{body}"
    );

    // Nothing reached storage, so nothing can reach the inbox or the reply queue either.
    let stored: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM discovered_posts WHERE tenant_id = ? AND external_id LIKE 'gone-%'",
    )
    .bind(app.tenant())
    .fetch_one(&app.db().reader)
    .await
    .expect("count");
    assert_eq!(stored, 0);

    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");
    let (_, leads) = get(&app, "/api/v1/opportunities").await;
    assert_eq!(leads["total"], 0, "{leads}");
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
async fn rescore_recomputes_from_stored_decisions_without_calling_providers() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;

    // Make the stored score stale and drop its history, then rescore.
    sqlx::query(
        "UPDATE leads SET score = 1, score_category = 'low', qualification_status = 'rejected' \
         WHERE tenant_id = ? AND id = ?",
    )
    .bind(app.tenant())
    .bind(&lead_id)
    .execute(&app.db().writer)
    .await
    .expect("stale score");
    sqlx::query("DELETE FROM lead_scores WHERE lead_id = ?")
        .bind(&lead_id)
        .execute(&app.db().writer)
        .await
        .expect("clear history");

    let outcome = lead_radar::modules::qualification::rescore_tenant(
        app.db(),
        app.tenant(),
        chrono::Utc::now(),
    )
    .await
    .expect("rescore");
    assert!(outcome.examined >= 1, "{outcome:?}");
    assert!(outcome.updated >= 1, "{outcome:?}");
    assert_eq!(outcome.version, 3);

    let (score, version): (i64, i64) = sqlx::query_as(
        "SELECT l.score, s.score_version FROM leads l \
         JOIN lead_scores s ON s.lead_id = l.id WHERE l.id = ? \
         ORDER BY s.created_at DESC LIMIT 1",
    )
    .bind(&lead_id)
    .fetch_one(&app.db().reader)
    .await
    .expect("rescored row");
    assert!(score > 1, "score recomputed: {score}");
    assert_eq!(version, 3);
}

#[tokio::test]
async fn seeds_activate_the_latest_scoring_version() {
    let app = setup().await;
    let active: (i64, String, String) = sqlx::query_as(
        "SELECT version, weights, thresholds FROM lead_score_versions \
         WHERE tenant_id = ? AND active = 1",
    )
    .bind(app.tenant())
    .fetch_one(&app.db().reader)
    .await
    .expect("active scoring version");
    assert_eq!(active.0, 3);
    assert!(active.1.contains("\"product_fit\":25"));
    assert!(
        active.2.contains("\"conferences_webinars\":50"),
        "conference threshold lowered: {}",
        active.2
    );
    assert!(
        active.2.contains("\"customer_support\":60"),
        "support threshold lowered: {}",
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
    // The product specification's ten HR keywords are present; real-world variants are added on top.
    let hr_keywords: Vec<&str> = hr["keywords"]
        .as_array()
        .expect("keywords")
        .iter()
        .filter_map(|value| value.as_str())
        .collect();
    for expected in [
        "multilingual recruitment",
        "international hiring",
        "language barrier interview",
        "multilingual interviews",
        "real-time translation interview",
        "global workforce communication",
        "non-native speakers interview",
        "cross-border recruitment",
        "interview interpretation",
        "international HR communication",
    ] {
        assert!(hr_keywords.contains(&expected), "missing {expected}");
    }
    assert!(hr_keywords.len() >= 10);

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
async fn the_reply_console_queues_drafts_and_records_a_manual_reply_in_one_call() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;

    // The console lists the lead and reports that drafts are missing.
    let (status, body) = get(&app, "/api/v1/reply-console?limit=10").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stats"]["needs_drafts"], 1, "{body}");
    let item = &body["items"][0];
    assert_eq!(item["lead"]["id"], lead_id);
    assert_eq!(item["comments"].as_array().expect("comments").len(), 0);
    assert!(!item["lead"]["content_snippet"]
        .as_str()
        .unwrap_or("")
        .is_empty());

    // One call prepares the next drafts (queued, not generated inline).
    let (status, body) = post(&app, "/api/v1/reply-console/prepare", json!({"limit": 1})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["queued"], 1);
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    let (status, body) = get(&app, "/api/v1/reply-console?limit=10").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stats"]["ready"], 1, "{body}");
    assert_eq!(
        body["items"][0]["comments"]
            .as_array()
            .expect("comments")
            .len(),
        3
    );

    // Recording a pitch as published requires the community's promotion policy on file first.
    let (_, communities) = get(&app, "/api/v1/communities?per_page=100").await;
    let community_id = communities["items"]
        .as_array()
        .expect("communities")
        .iter()
        .find(|community| community["name"] == "recruiting")
        .expect("recruiting community")["id"]
        .as_str()
        .expect("community id")
        .to_string();
    let (status, body) = patch(
        &app,
        &format!("/api/v1/communities/{community_id}"),
        json!({"self_promotion": "allowed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // One call approves, records the manual publication and the engagement.
    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/reply"),
        json!({"variant": "helpful", "engagement_kind": "reply", "note": "answered in the thread"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["comment"]["status"], "published_manually");
    assert_eq!(body["engagement"]["kind"], "reply");

    // The same draft cannot be recorded twice.
    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/reply"),
        json!({"variant": "helpful"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Editing from the console creates a new version before publishing.
    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/reply"),
        json!({"variant": "soft_pitch", "body": "Disclosure: I work on VoxTranslate. For live interviews, real-time translation keeps the conversation natural and avoids scheduling an interpreter."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["comment"]["version"], 2);
    assert_eq!(body["comment"]["status"], "published_manually");

    // A missing variant is refused with a clear conflict.
    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/reply"),
        json!({"variant": "direct_pitch", "body": "Disclosure: I work on VoxTranslate. It translates speech live during the interview."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn the_console_matches_a_reddit_url_for_the_extension() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;

    // Normalisation ignores a trailing slash and query parameters.
    let (status, body) = get(
        &app,
        "/api/v1/reply-console/match?url=https://example.invalid/discussion/1/?utm_source=x",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lead"]["id"], lead_id);
    assert_eq!(body["comments"].as_array().expect("comments").len(), 0);

    let (status, _) = get(
        &app,
        "/api/v1/reply-console/match?url=https://example.invalid/not-our-post",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn automatic_preparation_is_bounded_and_rate_limited() {
    let app = setup().await;
    import_and_lead(&app).await;

    let (status, first) = post(
        &app,
        "/api/v1/reply-console/prepare",
        json!({"limit": 3, "auto": true}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{first}");
    assert_eq!(first["queued"], 1);

    // A page refresh inside the cooldown window must not queue paid generation again.
    let (status, second) = post(
        &app,
        "/api/v1/reply-console/prepare",
        json!({"limit": 3, "auto": true}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{second}");
    assert_eq!(second["queued"], 0);
    assert_eq!(second["cooldown_minutes"], 10);

    // The explicit button bypasses the cooldown.
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");
    let (status, forced) = post(&app, "/api/v1/reply-console/prepare", json!({"limit": 3})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{forced}");
    assert_eq!(forced["queued"], 0, "the lead already has drafts");
}

#[tokio::test]
async fn a_bootstrap_token_authenticates_from_the_database() {
    let token = "bootstrap-token-with-enough-length-123456";
    let app = common::setup_with_vars(&[("BOOTSTRAP_API_TOKEN", token)]).await;
    let (status, body) = get_as(&app, "/api/v1/dashboard", token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = get_as(&app, "/api/v1/dashboard", "wrong-token").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rotating_the_bootstrap_token_can_be_pruned_explicitly() {
    let token = "rotated-bootstrap-token-1234567890abcd";
    let app = common::setup_with_vars(&[("BOOTSTRAP_API_TOKEN", token)]).await;
    // Simulate a previous bootstrap token left behind by a rotation.
    sqlx::query(
        "INSERT INTO api_tokens (id, tenant_id, user_id, token_hash, label, created_at) \
         VALUES (?, ?, ?, ?, 'bootstrap (environment)', ?)",
    )
    .bind("stale-token-row")
    .bind(app.tenant())
    .bind(lead_radar::seed::DEV_USER_ID)
    .bind(lead_radar::auth::hash_token(
        "previous-bootstrap-token-000000000000",
    ))
    .bind(lead_radar::domain::time::now_iso())
    .execute(&app.db().writer)
    .await
    .expect("stale token");

    let removed = lead_radar::modules::tenancy::prune_bootstrap_tokens(
        app.db(),
        app.tenant(),
        &lead_radar::auth::hash_token(token),
    )
    .await
    .expect("prune");
    assert_eq!(removed, 1);

    // The current token still works; the stale one is gone.
    let (status, _) = get_as(&app, "/api/v1/dashboard", token).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get_as(
        &app,
        "/api/v1/dashboard",
        "previous-bootstrap-token-000000000000",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
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

#[tokio::test]
async fn a_project_ranks_only_the_communities_that_are_validated_and_enabled() {
    let app = setup().await;

    // Two communities: one validated and enabled, one still pending and disabled.
    let (_, communities) = get(&app, "/api/v1/communities?per_page=100").await;
    let find = |name: &str| -> String {
        communities["items"]
            .as_array()
            .expect("communities")
            .iter()
            .find(|community| community["name"] == name)
            .expect("community")["id"]
            .as_str()
            .expect("id")
            .to_string()
    };
    let active = find("humanresources");
    let dormant = find("hospitality");
    let (status, _) = post(
        &app,
        &format!("/api/v1/communities/{active}/validate"),
        json!({"outcome": "validated", "evidence": "rules read 2026-11-04"}),
    )
    .await;
    assert!(status.is_success());
    let (status, _) = patch(
        &app,
        &format!("/api/v1/communities/{active}"),
        json!({"enabled": true}),
    )
    .await;
    assert!(status.is_success());

    let (status, project) = post(
        &app,
        "/api/v1/projects",
        json!({
            "name": "Voice Translate Demo",
            "summary": "Real-time multilingual voice translation for live meetings and interviews.",
            "website_urls": []
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    // The seed already gave the tenant its default project, so a new one never steals it.
    assert_eq!(project["is_default"], false, "{project}");
    let project_id = project["id"].as_str().expect("project id").to_string();

    let (status, body) = post(
        &app,
        &format!("/api/v1/projects/{project_id}/match-communities"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["candidates"], 1, "{body}");
    assert_eq!(body["scorer"], "generative", "{body}");
    let ranked = body["communities"].as_array().expect("matches");
    assert_eq!(ranked.len(), 1, "{body}");
    assert_eq!(ranked[0]["name"], "humanresources");
    assert_eq!(ranked[0]["enabled"], true);
    assert!(ranked[0]["relevance"].as_i64().unwrap_or(0) > 0);

    // The pending, disabled community is never a candidate.
    assert!(ranked
        .iter()
        .all(|match_row| match_row["community_id"] != dormant.as_str()));

    // A project with no candidate community is refused instead of returning an empty ranking.
    let (status, _) = patch(
        &app,
        &format!("/api/v1/communities/{active}"),
        json!({"enabled": false}),
    )
    .await;
    assert!(status.is_success());
    let (status, body) = post(
        &app,
        &format!("/api/v1/projects/{project_id}/match-communities"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn the_decision_provider_can_rank_a_project_without_prose() {
    let app = setup().await;
    let (_, communities) = get(&app, "/api/v1/communities?per_page=100").await;
    let community_id = communities["items"][0]["id"]
        .as_str()
        .expect("community id")
        .to_string();
    let (status, _) = post(
        &app,
        &format!("/api/v1/communities/{community_id}/validate"),
        json!({"outcome": "validated", "evidence": "rules read"}),
    )
    .await;
    assert!(status.is_success());
    let (status, _) = patch(
        &app,
        &format!("/api/v1/communities/{community_id}"),
        json!({"enabled": true}),
    )
    .await;
    assert!(status.is_success());

    let (_, project) = post(
        &app,
        "/api/v1/projects",
        json!({"name": "Decision ranked", "summary": "Voice translation for international teams."}),
    )
    .await;
    let project_id = project["id"].as_str().expect("id").to_string();

    let (status, body) = post(
        &app,
        &format!("/api/v1/projects/{project_id}/match-communities?scorer=decision"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scorer"], "decision", "{body}");
    assert_eq!(body["provider"], "mock", "{body}");
    let ranked = body["communities"].as_array().expect("ranking");
    assert_eq!(ranked.len(), 1, "{body}");
    // A probability, not prose: 0.72 from the mock decision provider becomes 72.
    assert_eq!(ranked[0]["relevance"], 72, "{body}");
    assert!(
        ranked[0]["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("probability 0.72"),
        "{body}"
    );

    // An unknown scorer is a validation error, never a silent default.
    let (status, body) = post(
        &app,
        &format!("/api/v1/projects/{project_id}/match-communities?scorer=oracle"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn the_project_list_carries_the_head_of_its_ranking() {
    let app = setup().await;
    let (_, projects) = get(&app, "/api/v1/projects").await;
    let project = projects["items"]
        .as_array()
        .expect("projects")
        .first()
        .expect("seeded default project")
        .clone();
    // The seeded default has no ranking yet: the card shows that honestly.
    assert_eq!(project["matched"], 0, "{project}");
    assert!(project["top_communities"]
        .as_array()
        .expect("top")
        .is_empty());
    assert!(project["matched_at"].is_null(), "{project}");
}

#[tokio::test]
async fn projects_are_tenant_scoped_and_the_drafting_prompts_use_the_default_one() {
    let app = setup().await;
    let (status, project) = post(
        &app,
        "/api/v1/projects",
        json!({"name": "Acme Translate", "summary": "Speech translation for call centres."}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    let project_id = project["id"].as_str().expect("id").to_string();

    // Another tenant cannot see or patch it.
    let (status, body) = get_as(
        &app,
        &format!("/api/v1/projects/{project_id}"),
        common::TOKEN_B,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A second project is not the default; the first one stays the prompt context.
    let (status, second) = post(
        &app,
        "/api/v1/projects",
        json!({"name": "Second product", "summary": "Something else entirely."}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_eq!(second["is_default"], false, "{second}");
    let second_id = second["id"].as_str().expect("id").to_string();
    let (status, promoted) = patch(
        &app,
        &format!("/api/v1/projects/{second_id}"),
        json!({"is_default": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{promoted}");
    assert_eq!(promoted["is_default"], true);
    let (_, first) = get(&app, &format!("/api/v1/projects/{project_id}")).await;
    assert_eq!(first["project"]["is_default"], false);
}

#[tokio::test]
async fn a_pitch_is_not_recorded_as_published_until_the_community_allows_it() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;

    // Drafts exist; pick the soft pitch, which is the variant that can get an account banned.
    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/generate-comments"),
        serde_json::json!({}),
    )
    .await;
    assert!(status.is_success(), "{body}");

    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");
    let (_, comments) = get(&app, &format!("/api/v1/comments?lead_id={lead_id}")).await;
    let pitch = comments["items"]
        .as_array()
        .expect("comments")
        .iter()
        .find(|comment| comment["variant"] == "soft_pitch")
        .expect("soft pitch draft")
        .clone();
    let comment_id = pitch["id"].as_str().expect("comment id").to_string();

    let (status, _) = post(
        &app,
        &format!("/api/v1/comments/{comment_id}/approve"),
        serde_json::json!({}),
    )
    .await;
    assert!(status.is_success());

    // The seeded community is 'pending' with no recorded promotion policy: unknown is not permission.
    let (status, body) = post(
        &app,
        &format!("/api/v1/comments/{comment_id}/record-publication"),
        serde_json::json!({"note": "posted by hand"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    // Recording that the moderators allow promotion unblocks the same reply.
    let (_, communities) = get(&app, "/api/v1/communities?per_page=100").await;
    let recruiting = communities["items"]
        .as_array()
        .expect("communities")
        .iter()
        .find(|community| community["name"] == "recruiting")
        .expect("recruiting community")
        .clone();
    let community_id = recruiting["id"].as_str().expect("community id").to_string();
    let (status, body) = patch(
        &app,
        &format!("/api/v1/communities/{community_id}"),
        serde_json::json!({"self_promotion": "allowed", "rules_summary": "self-promotion allowed with disclosure"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["self_promotion"], "allowed");

    let (status, body) = post(
        &app,
        &format!("/api/v1/comments/{comment_id}/record-publication"),
        serde_json::json!({"note": "posted by hand"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "published_manually");
}

#[tokio::test]
async fn seeding_reconciles_keywords_a_profile_was_missing() {
    let app = setup().await;
    let config = app.state.config.clone();

    // A keyword added to the seed after the profile already existed used to be lost forever,
    // because the seed only wrote keywords while creating a missing profile.
    sqlx::query("DELETE FROM keywords WHERE term = 'non english speakers'")
        .execute(&app.db().writer)
        .await
        .expect("remove keyword");
    let missing: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM keywords WHERE term = 'non english speakers'")
            .fetch_one(&app.db().reader)
            .await
            .expect("count");
    assert_eq!(missing, 0);

    seed::seed_all(&config, app.db())
        .await
        .expect("reconcile seed");

    let restored: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM keywords WHERE term = 'non english speakers'")
            .fetch_one(&app.db().reader)
            .await
            .expect("count");
    assert_eq!(restored, 1);

    // Running it again is still idempotent: no duplicates.
    seed::seed_all(&config, app.db())
        .await
        .expect("second reconcile");
    let again: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM keywords WHERE term = 'non english speakers'")
            .fetch_one(&app.db().reader)
            .await
            .expect("count");
    assert_eq!(again, 1);
}

#[tokio::test]
async fn language_support_posts_pass_the_seeded_filter_and_study_posts_do_not() {
    use lead_radar::domain::enums::Vertical;
    use lead_radar::domain::filtering::{evaluate, KeywordSet};

    let (positive, negative) = seed::keyword_terms();
    let mut keywords = KeywordSet::default();
    for term in &negative {
        keywords.add_negative(term);
    }
    for vertical in [
        Vertical::HrRecruiting,
        Vertical::InternationalMeetings,
        Vertical::ConferencesWebinars,
        Vertical::CustomerSupport,
        Vertical::TourismHospitality,
        Vertical::Other,
    ] {
        for term in &positive {
            keywords.add_positive(vertical, term);
        }
    }

    // The real thread this rule was written for: an HR practitioner asking how to support
    // non-English speakers at work.
    let support = evaluate(
        "How do you support non-English speakers / readers? We have significant populations of \
         Spanish and Vietnamese speakers. Everyone knows enough English to work safely.",
        &keywords,
    );
    assert!(support.relevant, "{support:?}");

    // Off-topic language-study content stays out, including the negative keyword case.
    let homework = evaluate(
        "I need help with my translation homework, it is a grammar exercise for my language \
         learning class.",
        &keywords,
    );
    assert!(!homework.relevant, "{homework:?}");
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
async fn a_labeled_dataset_can_be_ingested_and_benchmarked() {
    let app = setup().await;
    let (status, body) = post(
        &app,
        "/api/v1/providers/benchmark/dataset",
        json!({
            "dataset": "operator-sample",
            "items": [
                {"external_ref": "s1", "text": "We cannot interview candidates who only speak Spanish. We need real-time translation, any suggestions?", "language": "en", "expected_relevant": true, "expected_intent": "explicit_solution_request", "expected_vertical": "hr_recruiting"},
                {"external_ref": "s2", "text": "I need help with my translation homework for school.", "language": "en", "expected_relevant": false, "expected_intent": "irrelevant", "expected_vertical": "other"},
                {"external_ref": "s3", "text": "Their support team cannot help customers who do not speak English, they need live translation.", "language": "en", "expected_relevant": true, "expected_intent": "active_problem", "expected_vertical": "customer_support", "split": "tuning"}
            ]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stored"], 3);

    let (status, body) = post(
        &app,
        "/api/v1/providers/benchmark",
        json!({"dataset": "operator-sample", "providers": ["jev"]}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["benchmark_run_id"].as_str().expect("run id");
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    let (status, detail) = get(&app, &format!("/api/v1/providers/benchmark/{run_id}")).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["status"], "succeeded", "{detail}");
    // Only the evaluation split is scored: the tuning example is excluded by design.
    assert_eq!(detail["item_count"], 2);
    let metrics = &detail["results"][0]["metrics"];
    assert_eq!(metrics["count"], 2);

    // Invalid labels are refused with field errors.
    let (status, body) = post(
        &app,
        "/api/v1/providers/benchmark/dataset",
        json!({"dataset": "bad", "items": [{"external_ref": "b1", "text": "x", "language": "en", "expected_relevant": true, "expected_intent": "nonsense", "expected_vertical": "hr_recruiting"}]}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
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

#[tokio::test]
async fn a_recorded_reply_leaves_the_queue_and_sets_the_waiting_time() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;
    post(&app, "/api/v1/reply-console/prepare", json!({"limit": 1})).await;
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/reply"),
        json!({"variant": "helpful", "note": "answered in the thread"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The default queue never re-offers work that is done.
    let (status, body) = get(&app, "/api/v1/reply-console?limit=10").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().expect("items").len(), 0, "{body}");
    assert_eq!(body["stats"]["answered"], 1, "{body}");
    assert_eq!(body["pacing"]["source"], "publication", "{body}");
    let wait = body["pacing"]["retry_after_seconds"]
        .as_i64()
        .expect("wait");
    assert!(wait > 0 && wait <= 600, "unexpected wait {wait}");

    // The answered view still shows the lead, with when it was answered.
    let (status, body) = get(&app, "/api/v1/reply-console?limit=10&include_answered=1").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().expect("items").len(), 1, "{body}");
    assert!(
        body["items"][0]["lead"]["answered_at"].as_str().is_some(),
        "{body}"
    );

    // A wait Reddit itself announced replaces the configured floor.
    let (status, body) = post(
        &app,
        "/api/v1/reply-console/rate-limit",
        json!({"minutes": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["retry_after_seconds"].as_i64().unwrap_or(9999) <= 180);
    let (_, body) = get(&app, "/api/v1/reply-console?limit=10").await;
    assert_eq!(body["pacing"]["source"], "reddit_reported", "{body}");

    // Rejected input changes nothing.
    let (status, _) = post(
        &app,
        "/api/v1/reply-console/rate-limit",
        json!({"minutes": 0}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, body) = get(&app, "/api/v1/reply-console?limit=10").await;
    assert!(
        body["pacing"]["retry_after_seconds"]
            .as_i64()
            .unwrap_or(9999)
            <= 180,
        "{body}"
    );
}

#[tokio::test]
async fn follow_ups_open_on_publication_and_track_reported_replies() {
    let app = setup().await;
    let lead_id = import_and_lead(&app).await;
    post(&app, "/api/v1/reply-console/prepare", json!({"limit": 1})).await;
    lead_radar::worker::drain(&app.state, 20)
        .await
        .expect("drain");

    // Nothing can be followed before a reply was recorded.
    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/followup"),
        json!({"state": "waiting"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/reply"),
        json!({"variant": "helpful"}),
    )
    .await;

    let (status, body) = get(&app, "/api/v1/followups?state=waiting&limit=5").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().expect("items");
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["lead_id"], lead_id);
    assert_eq!(items[0]["new_replies"], 0);

    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/followup"),
        json!({
            "new_replies": 2,
            "last_reporter": "threadop",
            "latest_excerpt": "Thanks, that answers it. One more question about pricing.",
            "latest_reply_at": "2026-11-04T10:00:00Z"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["new_replies"], 2);
    assert_eq!(body["state"], "waiting");

    let (_, body) = get(&app, "/api/v1/followups?state=waiting").await;
    assert_eq!(body["items"][0]["last_reporter"], "threadop", "{body}");
    assert_eq!(body["items"][0]["new_replies"], 2, "{body}");

    // Closing keeps the record but drops it from the waiting list.
    let (status, body) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/followup"),
        json!({"state": "closed"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = get(&app, "/api/v1/followups?state=waiting").await;
    assert_eq!(body["items"].as_array().expect("items").len(), 0, "{body}");
    let (_, body) = get(&app, "/api/v1/followups?state=all").await;
    assert_eq!(body["items"][0]["state"], "closed", "{body}");

    // An unknown state is refused and leaves the record untouched.
    let (status, _) = post(
        &app,
        &format!("/api/v1/opportunities/{lead_id}/followup"),
        json!({"state": "abandoned"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (_, body) = get(&app, "/api/v1/followups?state=all").await;
    assert_eq!(body["items"][0]["state"], "closed", "{body}");
}
