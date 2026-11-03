//! Normalization, deduplication and ingestion of posts.
//!
//! Deduplication is enforced by unique constraints (`tenant_id + content_hash`, and
//! `tenant_id + source_id + external_id`), not only by application logic. A re-import therefore
//! cannot create a second row, and one post keeps at most one active lead.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use validator::{Validate, ValidationError};

use crate::db::Db;
use crate::domain::enums::JobKind;
use crate::domain::language::detect;
use crate::domain::time::now_iso;
use crate::modules::{new_id, operations, to_json};

/// Raw item shape accepted by the manual JSON import and by the generic dataset mapping.
/// Validation is declared here so every entry point shares the same limits.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Validate)]
pub struct ImportPost {
    #[validate(length(max = 200))]
    #[serde(default)]
    pub external_id: Option<String>,
    #[validate(length(max = 300))]
    #[serde(default)]
    pub title: Option<String>,
    #[validate(length(min = 1, max = 20000))]
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub url: Option<String>,
    #[validate(length(max = 200))]
    #[serde(default)]
    pub author: Option<String>,
    #[validate(length(max = 200))]
    #[serde(default)]
    pub community: Option<String>,
    #[validate(length(max = 16))]
    #[serde(default)]
    pub language: Option<String>,
    #[validate(custom(function = "validate_optional_timestamp"))]
    #[serde(default)]
    pub published_at: Option<String>,
    #[validate(nested)]
    #[serde(default)]
    pub engagement: Option<Engagement>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, Validate)]
pub struct Engagement {
    #[validate(range(min = 0))]
    #[serde(default)]
    pub comments: i64,
    #[validate(range(min = 0))]
    #[serde(default)]
    pub upvotes: i64,
    #[validate(range(min = 0))]
    #[serde(default)]
    pub shares: i64,
}

/// Timestamps must be RFC 3339 when present; an unparseable value is rejected instead of guessed.
pub fn validate_optional_timestamp(value: &str) -> Result<(), ValidationError> {
    if crate::domain::time::parse_iso(value).is_some() {
        Ok(())
    } else {
        Err(ValidationError::new("timestamp").with_message("must be an RFC 3339 timestamp".into()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Post {
    pub id: String,
    pub tenant_id: String,
    pub source_id: Option<String>,
    pub source_name: Option<String>,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub content: String,
    pub language: Option<String>,
    pub author: Option<String>,
    pub source_url: Option<String>,
    pub community: Option<String>,
    pub published_at: Option<String>,
    pub discovered_at: String,
    /// `None` when the source supplied no engagement data at all.
    pub engagement: Option<Engagement>,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ImportOutcome {
    pub ingestion_run_id: String,
    pub imported: i64,
    pub duplicates: i64,
    pub rejected: i64,
    pub post_ids: Vec<String>,
    pub rejected_reasons: Vec<String>,
    /// Normalization job created for this import, when at least one post was stored.
    pub normalization_job_id: Option<String>,
}

/// Stable content hash over normalized title and content.
pub fn content_hash(title: Option<&str>, content: &str) -> String {
    let mut hasher = Sha256::new();
    let normalized_title = title
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let normalized_content = content
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    hasher.update(normalized_title.as_bytes());
    hasher.update(b"\n");
    hasher.update(normalized_content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn normalize_language(language: Option<&str>, content: &str) -> String {
    match language {
        Some(value) if !value.trim().is_empty() => crate::domain::language::primary_subtag(value),
        _ => detect(content),
    }
}

/// Ensure a source row exists, returning its id. Used to attach provenance to ingested posts.
pub async fn ensure_source(
    db: &Db,
    tenant_id: &str,
    name: &str,
    provider: &str,
    source_type: &str,
    note: &str,
) -> Result<String, sqlx::Error> {
    if let Some(source) = crate::modules::sources::find_source_by_name(db, tenant_id, name).await? {
        return Ok(source.id);
    }
    let source = crate::modules::sources::create_source(
        db,
        tenant_id,
        &crate::modules::sources::NewSource {
            name,
            provider,
            source_type,
            config: &serde_json::json!({ "note": note }),
        },
    )
    .await?;
    Ok(source.id)
}

/// Ensure the default manual import source exists and is usable.
pub async fn ensure_manual_source(db: &Db, tenant_id: &str) -> Result<String, sqlx::Error> {
    let id = ensure_source(
        db,
        tenant_id,
        "manual-import",
        "manual_import",
        "manual_json",
        "operator supplied JSON",
    )
    .await?;
    crate::modules::sources::patch_source(
        db,
        tenant_id,
        &id,
        &crate::modules::sources::SourcePatch {
            enabled: Some(true),
            config: None,
        },
    )
    .await?;
    Ok(id)
}

/// Import posts and enqueue the pipeline. Duplicates are counted, not stored twice.
#[allow(clippy::too_many_arguments)]
pub async fn import_posts(
    db: &Db,
    tenant_id: &str,
    source_id: Option<&str>,
    provider: &str,
    posts: &[ImportPost],
    requested_by: &str,
    max_attempts: i64,
    kind: &str,
) -> Result<ImportOutcome, sqlx::Error> {
    let run_id = new_id();
    let now = now_iso();
    sqlx::query(
        "INSERT INTO ingestion_runs (id, tenant_id, source_id, kind, status, requested_by, stats, \
         started_at) VALUES (?, ?, ?, ?, 'running', ?, '{}', ?)",
    )
    .bind(&run_id)
    .bind(tenant_id)
    .bind(source_id)
    .bind(kind)
    .bind(requested_by)
    .bind(&now)
    .execute(&db.writer)
    .await?;

    let mut imported = 0i64;
    let mut duplicates = 0i64;
    let mut rejected = 0i64;
    let mut post_ids = Vec::new();
    let mut rejected_reasons = Vec::new();

    for post in posts {
        let content = post.content.trim();
        if content.is_empty() {
            rejected += 1;
            rejected_reasons.push("content must not be empty".to_string());
            insert_item(db, &run_id, None, "rejected", Some("empty content")).await?;
            continue;
        }
        let hash = content_hash(post.title.as_deref(), content);
        let language = normalize_language(post.language.as_deref(), content);
        let post_id = new_id();
        let engagement = post.engagement.clone();
        let inserted = sqlx::query(
            "INSERT INTO discovered_posts (id, tenant_id, source_id, external_id, content_hash, \
             title, content, language, author, source_url, community, published_at, discovered_at, \
             engagement_comments, engagement_upvotes, engagement_shares, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(tenant_id, content_hash) DO NOTHING",
        )
        .bind(&post_id)
        .bind(tenant_id)
        .bind(source_id)
        .bind(&post.external_id)
        .bind(&hash)
        .bind(&post.title)
        .bind(content)
        .bind(&language)
        .bind(&post.author)
        .bind(&post.url)
        .bind(&post.community)
        .bind(&post.published_at)
        .bind(now_iso())
        .bind(engagement.as_ref().map(|value| value.comments))
        .bind(engagement.as_ref().map(|value| value.upvotes))
        .bind(engagement.as_ref().map(|value| value.shares))
        .bind(&now)
        .bind(&now)
        .execute(&db.writer)
        .await?;

        if inserted.rows_affected() == 0 {
            duplicates += 1;
            let existing: Option<String> = sqlx::query_scalar(
                "SELECT id FROM discovered_posts WHERE tenant_id = ? AND content_hash = ?",
            )
            .bind(tenant_id)
            .bind(&hash)
            .fetch_optional(&db.reader)
            .await?;
            if let Some(existing) = &existing {
                // A later payload may carry metadata the first import lacked (community, URL, author,
                // engagement). Fill only what is missing; never overwrite collected values.
                sqlx::query(
                    "UPDATE discovered_posts SET community = COALESCE(community, ?), \
                     source_url = COALESCE(source_url, ?), author = COALESCE(author, ?), \
                     language = COALESCE(language, ?), published_at = COALESCE(published_at, ?), \
                     source_id = COALESCE(source_id, ?), external_id = COALESCE(external_id, ?), \
                     engagement_comments = COALESCE(engagement_comments, ?), \
                     engagement_upvotes = COALESCE(engagement_upvotes, ?), \
                     engagement_shares = COALESCE(engagement_shares, ?), updated_at = ? \
                     WHERE tenant_id = ? AND id = ?",
                )
                .bind(&post.community)
                .bind(&post.url)
                .bind(&post.author)
                .bind(&language)
                .bind(&post.published_at)
                .bind(source_id)
                .bind(&post.external_id)
                .bind(engagement.as_ref().map(|value| value.comments))
                .bind(engagement.as_ref().map(|value| value.upvotes))
                .bind(engagement.as_ref().map(|value| value.shares))
                .bind(now_iso())
                .bind(tenant_id)
                .bind(existing)
                .execute(&db.writer)
                .await?;
                // Keep the lead's copy of the same provenance in sync.
                sqlx::query(
                    "UPDATE leads SET community = COALESCE(community, ?), \
                     source_url = COALESCE(source_url, ?), author = COALESCE(author, ?), \
                     language = COALESCE(language, ?), published_at = COALESCE(published_at, ?), \
                     source_id = COALESCE(source_id, ?), updated_at = ? \
                     WHERE tenant_id = ? AND post_id = ?",
                )
                .bind(&post.community)
                .bind(&post.url)
                .bind(&post.author)
                .bind(&language)
                .bind(&post.published_at)
                .bind(source_id)
                .bind(now_iso())
                .bind(tenant_id)
                .bind(existing)
                .execute(&db.writer)
                .await?;
            }
            insert_item(
                db,
                &run_id,
                existing.as_deref(),
                "duplicate",
                Some("content_hash"),
            )
            .await?;
            continue;
        }

        imported += 1;
        post_ids.push(post_id.clone());
        if let Some(source_id) = source_id {
            sqlx::query(
                "INSERT INTO post_sources (post_id, source_id, provider, first_seen_at) \
                 VALUES (?, ?, ?, ?) ON CONFLICT DO NOTHING",
            )
            .bind(&post_id)
            .bind(source_id)
            .bind(provider)
            .bind(&now)
            .execute(&db.writer)
            .await?;
        }
        insert_item(db, &run_id, Some(&post_id), "imported", None).await?;
    }

    let status = if rejected > 0 && imported == 0 {
        "failed"
    } else if rejected > 0 {
        "partial"
    } else {
        "succeeded"
    };
    let stats = serde_json::json!({
        "imported": imported,
        "duplicates": duplicates,
        "rejected": rejected,
    });
    sqlx::query("UPDATE ingestion_runs SET status = ?, stats = ?, finished_at = ? WHERE id = ?")
        .bind(status)
        .bind(to_json(&stats))
        .bind(now_iso())
        .bind(&run_id)
        .execute(&db.writer)
        .await?;

    let mut normalization_job_id = None;
    // Duplicates still reference posts worth (re)filtering: re-processing a paid dataset after a
    // keyword change must not require a new Apify run.
    if imported + duplicates > 0 {
        let (job_id, _) = operations::enqueue(
            db,
            Some(tenant_id),
            JobKind::Normalization,
            &serde_json::json!({"ingestion_run_id": run_id}),
            Some(&format!("normalization:{run_id}")),
            max_attempts,
        )
        .await?;
        normalization_job_id = Some(job_id);
    }

    Ok(ImportOutcome {
        ingestion_run_id: run_id,
        imported,
        duplicates,
        rejected,
        post_ids,
        rejected_reasons,
        normalization_job_id,
    })
}

async fn insert_item(
    db: &Db,
    run_id: &str,
    post_id: Option<&str>,
    status: &str,
    reason: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO ingestion_items (id, ingestion_run_id, post_id, status, reason, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(new_id())
    .bind(run_id)
    .bind(post_id)
    .bind(status)
    .bind(reason)
    .bind(now_iso())
    .execute(&db.writer)
    .await?;
    Ok(())
}

const POST_SELECT: &str = "SELECT p.id, p.tenant_id, p.source_id, s.name AS source_name, \
    p.external_id, p.title, p.content, p.language, p.author, p.source_url, p.community, \
    p.published_at, p.discovered_at, p.engagement_comments, p.engagement_upvotes, \
    p.engagement_shares, p.deleted_at FROM discovered_posts p \
    LEFT JOIN sources s ON s.id = p.source_id";

#[derive(sqlx::FromRow)]
struct PostRow {
    id: String,
    tenant_id: String,
    source_id: Option<String>,
    source_name: Option<String>,
    external_id: Option<String>,
    title: Option<String>,
    content: String,
    language: Option<String>,
    author: Option<String>,
    source_url: Option<String>,
    community: Option<String>,
    published_at: Option<String>,
    discovered_at: String,
    engagement_comments: Option<i64>,
    engagement_upvotes: Option<i64>,
    engagement_shares: Option<i64>,
    deleted_at: Option<String>,
}

impl From<PostRow> for Post {
    fn from(row: PostRow) -> Self {
        Self {
            id: row.id,
            tenant_id: row.tenant_id,
            source_id: row.source_id,
            source_name: row.source_name,
            external_id: row.external_id,
            title: row.title,
            content: row.content,
            language: row.language,
            author: row.author,
            source_url: row.source_url,
            community: row.community,
            published_at: row.published_at,
            discovered_at: row.discovered_at,
            engagement: match (
                row.engagement_comments,
                row.engagement_upvotes,
                row.engagement_shares,
            ) {
                (None, None, None) => None,
                (comments, upvotes, shares) => Some(Engagement {
                    comments: comments.unwrap_or(0),
                    upvotes: upvotes.unwrap_or(0),
                    shares: shares.unwrap_or(0),
                }),
            },
            deleted_at: row.deleted_at,
        }
    }
}

pub async fn get_post(
    db: &Db,
    tenant_id: &str,
    post_id: &str,
) -> Result<Option<Post>, sqlx::Error> {
    let sql = format!("{POST_SELECT} WHERE p.tenant_id = ? AND p.id = ?");
    let row = sqlx::query_as::<_, PostRow>(&sql)
        .bind(tenant_id)
        .bind(post_id)
        .fetch_optional(&db.reader)
        .await?;
    Ok(row.map(Post::from))
}

/// Post ids imported by one ingestion run (used by the normalization job).
pub async fn run_post_ids(
    db: &Db,
    tenant_id: &str,
    ingestion_run_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT i.post_id FROM ingestion_items i \
         JOIN ingestion_runs r ON r.id = i.ingestion_run_id \
         WHERE r.tenant_id = ? AND i.ingestion_run_id = ? \
         AND i.status IN ('imported', 'duplicate') \
         AND i.post_id IS NOT NULL ORDER BY i.created_at",
    )
    .bind(tenant_id)
    .bind(ingestion_run_id)
    .fetch_all(&db.reader)
    .await
}

/// Resolve the item URL. Dataset items often carry a relative permalink (`/r/…`); it is joined to the
/// canonical host when the item also identifies a subreddit/community, and left empty rather than
/// guessed for an unknown provider.
pub fn absolute_url(item: &Value) -> Option<String> {
    let raw = ["postUrl", "contentUrl", "url", "permalink", "link"]
        .iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))?
        .trim();
    if raw.is_empty() {
        return None;
    }
    if raw.starts_with("http://") || raw.starts_with("https://") {
        return Some(raw.to_string());
    }
    if raw.starts_with('/')
        && (item.get("subreddit").is_some()
            || item.get("community").is_some()
            || item.get("forum").is_some())
    {
        return Some(format!(
            "https://www.reddit.com{}",
            raw.trim_end_matches('/')
        ));
    }
    None
}

/// Community/subreddit name, normalised: `r/recruiting` and `recruiting` both become `recruiting`.
pub fn community_name(item: &Value) -> Option<String> {
    let raw = [
        "parsedCommunityName",
        "communityName",
        "community",
        "subreddit",
        "forum",
        "channel",
    ]
    .iter()
    .find_map(|key| item.get(*key).and_then(Value::as_str))?
    .trim();
    if raw.is_empty() {
        return None;
    }
    Some(raw.trim_start_matches("r/").to_string())
}

/// Generic dataset mapping: documented aliases only, no Actor-specific assumptions.
pub fn map_dataset_item(item: &Value) -> Option<ImportPost> {
    let content = ["content", "text", "body", "selftext", "description"]
        .iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))
        .map(str::to_string)?;
    let string = |keys: &[&str]| -> Option<String> {
        keys.iter()
            .find_map(|key| item.get(*key).and_then(Value::as_str))
            .map(str::to_string)
    };
    Some(ImportPost {
        external_id: string(&["id", "parsedId", "external_id"]),
        title: string(&["title", "headline"]),
        content,
        url: absolute_url(item),
        author: string(&["author", "authorName", "username", "user"]),
        community: community_name(item),
        language: string(&["language", "lang", "detectedLanguage"]),
        published_at: string(&[
            "published_at",
            "publishedAt",
            "createdAt",
            "created_at",
            "date",
            "timestamp",
        ]),
        engagement: Some(Engagement {
            comments: item
                .pointer("/engagement/comments")
                .and_then(Value::as_i64)
                .or_else(|| item.get("commentsCount").and_then(Value::as_i64))
                .or_else(|| item.get("num_comments").and_then(Value::as_i64))
                .unwrap_or(0),
            upvotes: item
                .pointer("/engagement/upvotes")
                .and_then(Value::as_i64)
                .or_else(|| item.get("upVotes").and_then(Value::as_i64))
                .or_else(|| item.get("score").and_then(Value::as_i64))
                .unwrap_or(0),
            shares: item
                .pointer("/engagement/shares")
                .and_then(Value::as_i64)
                .unwrap_or(0),
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_ignores_case_and_whitespace() {
        let first = content_hash(Some("Hello  World"), "We  need   translation");
        let second = content_hash(Some("hello world"), "we need translation");
        assert_eq!(first, second);
        assert_ne!(first, content_hash(Some("hello world"), "different"));
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn dataset_mapping_uses_documented_aliases_only() {
        let item = serde_json::json!({
            "id": "abc",
            "title": "Interview problem",
            "text": "We cannot interview candidates who speak Portuguese.",
            "url": "https://example.invalid/p/1",
            "author": "someone",
            "subreddit": "recruiting",
            "createdAt": "2024-05-01T09:00:00Z",
            "score": 7,
            "num_comments": 3
        });
        let post = map_dataset_item(&item).expect("mapped");
        assert_eq!(post.external_id.as_deref(), Some("abc"));
        assert_eq!(post.community.as_deref(), Some("recruiting"));
        assert_eq!(post.engagement.as_ref().expect("engagement").upvotes, 7);
        assert!(map_dataset_item(&serde_json::json!({"title": "no body"})).is_none());
    }

    #[test]
    fn relative_permalinks_are_resolved_and_unknown_hosts_are_not_invented() {
        let reddit = serde_json::json!({
            "text": "body",
            "permalink": "/r/recruiting/comments/abc/multilingual_interviews/",
            "subreddit": "recruiting"
        });
        assert_eq!(
            map_dataset_item(&reddit).expect("mapped").url.as_deref(),
            Some("https://www.reddit.com/r/recruiting/comments/abc/multilingual_interviews")
        );

        let absolute = serde_json::json!({"text": "body", "url": "https://example.invalid/p/1"});
        assert_eq!(
            map_dataset_item(&absolute).expect("mapped").url.as_deref(),
            Some("https://example.invalid/p/1")
        );

        // A relative path with no host hint stays empty instead of being guessed.
        let unknown = serde_json::json!({"text": "body", "url": "/p/1"});
        assert_eq!(map_dataset_item(&unknown).expect("mapped").url, None);
    }

    #[test]
    fn maps_the_second_actor_payload_shape() {
        let item = serde_json::json!({
            "id": "t3_1piafgn",
            "parsedId": "1piafgn",
            "title": "Experienced Recruiters",
            "body": "Text of the post about multilingual interviews.",
            "postUrl": "https://www.reddit.com/r/recruiting/comments/1piafgn/x/",
            "communityName": "r/recruiting",
            "parsedCommunityName": "recruiting",
            "authorName": "someone",
            "createdAt": "2025-12-09T15:33:25.000Z",
            "upVotes": 3,
            "commentsCount": 5
        });
        let post = map_dataset_item(&item).expect("mapped");
        assert_eq!(post.external_id.as_deref(), Some("t3_1piafgn"));
        assert_eq!(post.community.as_deref(), Some("recruiting"));
        assert_eq!(
            post.url.as_deref(),
            Some("https://www.reddit.com/r/recruiting/comments/1piafgn/x/")
        );
        assert_eq!(post.author.as_deref(), Some("someone"));
        assert_eq!(
            post.published_at.as_deref(),
            Some("2025-12-09T15:33:25.000Z")
        );
        let engagement = post.engagement.expect("engagement");
        assert_eq!(engagement.upvotes, 3);
        assert_eq!(engagement.comments, 5);
    }

    #[test]
    fn language_detection_is_used_when_the_item_has_none() {
        let post = ImportPost {
            content: "Necesitamos traducir las reuniones con nuestro equipo".to_string(),
            ..Default::default()
        };
        assert_eq!(
            normalize_language(post.language.as_deref(), &post.content),
            "es"
        );
    }
}
