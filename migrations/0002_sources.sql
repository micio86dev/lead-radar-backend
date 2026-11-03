-- Sources, provider registry, Actor registry, communities, search profiles, keywords.
CREATE TABLE source_providers (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('source', 'decision', 'generative')),
    enabled INTEGER NOT NULL DEFAULT 0,
    config TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE sources (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    provider TEXT NOT NULL CHECK (provider IN ('apify', 'manual_import', 'official_api', 'licensed_feed')),
    source_type TEXT NOT NULL CHECK (source_type IN ('community_feed', 'manual_json', 'webhook')),
    enabled INTEGER NOT NULL DEFAULT 0,
    validation_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (validation_status IN ('pending', 'validated', 'rejected')),
    config TEXT NOT NULL DEFAULT '{}',
    last_run_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, name)
);

CREATE INDEX idx_sources_tenant ON sources(tenant_id);

-- Actor identifiers are operator-supplied configuration. Nothing here is inferred.
CREATE TABLE actor_registry (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    provider TEXT NOT NULL DEFAULT 'apify',
    actor_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    source_type TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 0,
    validation_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (validation_status IN ('pending', 'validated', 'rejected')),
    input_template TEXT NOT NULL DEFAULT '{}',
    max_items_per_run INTEGER NOT NULL DEFAULT 25 CHECK (max_items_per_run > 0),
    cost_per_run_estimate REAL CHECK (cost_per_run_estimate IS NULL OR cost_per_run_estimate >= 0),
    last_success_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, provider, actor_id)
);

CREATE INDEX idx_actor_registry_tenant ON actor_registry(tenant_id);

CREATE TABLE communities (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    vertical TEXT NOT NULL,
    validation_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (validation_status IN ('pending', 'validated', 'rejected')),
    enabled INTEGER NOT NULL DEFAULT 0,
    notes TEXT,
    last_checked_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, name)
);

CREATE INDEX idx_communities_tenant ON communities(tenant_id, vertical);

CREATE TABLE search_profiles (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    vertical TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, name)
);

CREATE INDEX idx_search_profiles_tenant ON search_profiles(tenant_id, vertical);

-- vertical = 'all' holds negative keywords that apply to every vertical.
CREATE TABLE keywords (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    vertical TEXT NOT NULL,
    term TEXT NOT NULL,
    polarity TEXT NOT NULL CHECK (polarity IN ('positive', 'negative')),
    created_at TEXT NOT NULL,
    UNIQUE (tenant_id, vertical, term, polarity)
);

CREATE INDEX idx_keywords_lookup ON keywords(tenant_id, polarity);
