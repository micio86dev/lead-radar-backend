-- Discovery: ingestion runs, normalized posts, provenance and per-item outcomes.
CREATE TABLE ingestion_runs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source_id TEXT REFERENCES sources(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK (kind IN ('manual_json', 'apify_actor', 'api_feed')),
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'partial')),
    requested_by TEXT NOT NULL,
    stats TEXT NOT NULL DEFAULT '{}',
    error TEXT,
    started_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE INDEX idx_ingestion_runs_tenant ON ingestion_runs(tenant_id, started_at DESC);

CREATE TABLE discovered_posts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source_id TEXT REFERENCES sources(id) ON DELETE SET NULL,
    external_id TEXT,
    content_hash TEXT NOT NULL,
    title TEXT,
    content TEXT NOT NULL,
    language TEXT,
    author TEXT,
    source_url TEXT,
    community TEXT,
    published_at TEXT,
    discovered_at TEXT NOT NULL,
    engagement_comments INTEGER,
    engagement_upvotes INTEGER,
    engagement_shares INTEGER,
    raw TEXT NOT NULL DEFAULT '{}',
    deleted_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, content_hash)
);

CREATE UNIQUE INDEX idx_posts_external
    ON discovered_posts(tenant_id, source_id, external_id)
    WHERE external_id IS NOT NULL;
CREATE INDEX idx_posts_published ON discovered_posts(tenant_id, published_at DESC);
CREATE INDEX idx_posts_community ON discovered_posts(tenant_id, community);

CREATE TABLE post_sources (
    post_id TEXT NOT NULL REFERENCES discovered_posts(id) ON DELETE CASCADE,
    source_id TEXT REFERENCES sources(id) ON DELETE SET NULL,
    provider TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    PRIMARY KEY (post_id, provider, source_id)
);

CREATE TABLE ingestion_items (
    id TEXT PRIMARY KEY,
    ingestion_run_id TEXT NOT NULL REFERENCES ingestion_runs(id) ON DELETE CASCADE,
    post_id TEXT REFERENCES discovered_posts(id) ON DELETE SET NULL,
    status TEXT NOT NULL CHECK (status IN ('imported', 'duplicate', 'rejected')),
    reason TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_ingestion_items_run ON ingestion_items(ingestion_run_id, status);
