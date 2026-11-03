-- Durable job queue, attempts, provider usage and cost ledger, audit log, Apify runs.
CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN (
        'discovery', 'actor_run', 'dataset_ingestion', 'normalization', 'deduplication',
        'decision_evaluation', 'generative_analysis', 'comment_generation', 'benchmark_execution'
    )),
    status TEXT NOT NULL DEFAULT 'queued'
        CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'dead_letter', 'cancelled')),
    idempotency_key TEXT UNIQUE,
    payload TEXT NOT NULL DEFAULT '{}',
    priority INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    max_attempts INTEGER NOT NULL DEFAULT 5,
    run_at TEXT NOT NULL,
    lease_owner TEXT,
    lease_expires_at TEXT,
    last_error TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE INDEX idx_jobs_claim ON jobs(status, run_at);
CREATE INDEX idx_jobs_tenant ON jobs(tenant_id, status);

CREATE TABLE job_attempts (
    id TEXT PRIMARY KEY,
    job_id TEXT NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('succeeded', 'failed', 'dead_letter')),
    error TEXT,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    duration_ms INTEGER
);

CREATE INDEX idx_job_attempts_job ON job_attempts(job_id, attempt);

CREATE TABLE provider_usage (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    operation TEXT NOT NULL,
    request_id TEXT,
    input_tokens INTEGER,
    output_tokens INTEGER,
    latency_ms INTEGER,
    status TEXT NOT NULL,
    error TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_provider_usage_provider ON provider_usage(provider, created_at DESC);

CREATE TABLE provider_costs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('actual', 'estimate')),
    amount REAL NOT NULL CHECK (amount >= 0),
    currency TEXT NOT NULL,
    unit TEXT NOT NULL,
    reference TEXT,
    unknown INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_provider_costs_provider ON provider_costs(provider, created_at DESC);

CREATE TABLE audit_logs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    entity TEXT NOT NULL,
    entity_id TEXT,
    details TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);

CREATE INDEX idx_audit_logs_tenant ON audit_logs(tenant_id, created_at DESC);

CREATE TABLE apify_runs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    actor_registry_id TEXT NOT NULL REFERENCES actor_registry(id) ON DELETE CASCADE,
    job_id TEXT REFERENCES jobs(id) ON DELETE SET NULL,
    provider_run_id TEXT,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed', 'aborted', 'timed_out')),
    items_count INTEGER NOT NULL DEFAULT 0,
    dataset_id TEXT,
    cost_estimate_eur REAL,
    cost_actual_eur REAL,
    input TEXT NOT NULL DEFAULT '{}',
    error TEXT,
    started_at TEXT,
    finished_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE INDEX idx_apify_runs_tenant ON apify_runs(tenant_id, created_at DESC);
