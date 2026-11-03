-- Structured decisions, generative analyses, benchmarks, provider defaults.
CREATE TABLE decision_runs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    post_id TEXT NOT NULL REFERENCES discovered_posts(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    model_version TEXT,
    provider_request_id TEXT,
    state_excerpt TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed')),
    latency_ms INTEGER,
    input_tokens INTEGER,
    output_tokens INTEGER,
    charged_tokens INTEGER,
    error TEXT,
    created_at TEXT NOT NULL,
    completed_at TEXT
);

CREATE INDEX idx_decision_runs_post ON decision_runs(tenant_id, post_id, created_at DESC);

CREATE TABLE decision_answers (
    id TEXT PRIMARY KEY,
    decision_run_id TEXT NOT NULL REFERENCES decision_runs(id) ON DELETE CASCADE,
    question TEXT NOT NULL,
    answer_type TEXT NOT NULL CHECK (answer_type IN ('noul', 'choice', 'score')),
    noul REAL,
    choice TEXT,
    score REAL,
    legend TEXT,
    probabilities TEXT,
    confidence REAL,
    created_at TEXT NOT NULL,
    UNIQUE (decision_run_id, question)
);

CREATE TABLE ai_analyses (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    post_id TEXT NOT NULL REFERENCES discovered_posts(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('generative_analysis', 'comment_generation')),
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    prompt_summary TEXT NOT NULL,
    summary TEXT,
    raw TEXT NOT NULL DEFAULT '{}',
    input_tokens INTEGER,
    output_tokens INTEGER,
    latency_ms INTEGER,
    error TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_ai_analyses_post ON ai_analyses(tenant_id, post_id, created_at DESC);

CREATE TABLE benchmark_items (
    id TEXT PRIMARY KEY,
    tenant_id TEXT REFERENCES tenants(id) ON DELETE CASCADE,
    dataset TEXT NOT NULL,
    external_ref TEXT NOT NULL,
    text TEXT NOT NULL,
    language TEXT NOT NULL,
    expected_relevant INTEGER NOT NULL CHECK (expected_relevant IN (0, 1)),
    expected_intent TEXT NOT NULL,
    expected_vertical TEXT NOT NULL,
    split TEXT NOT NULL DEFAULT 'evaluation' CHECK (split IN ('tuning', 'evaluation')),
    created_at TEXT NOT NULL,
    UNIQUE (dataset, external_ref)
);

CREATE TABLE benchmark_runs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    dataset TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('queued', 'running', 'succeeded', 'failed')),
    providers TEXT NOT NULL,
    item_count INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    created_at TEXT NOT NULL,
    finished_at TEXT
);

CREATE INDEX idx_benchmark_runs_tenant ON benchmark_runs(tenant_id, created_at DESC);

CREATE TABLE benchmark_results (
    id TEXT PRIMARY KEY,
    benchmark_run_id TEXT NOT NULL REFERENCES benchmark_runs(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    model_version TEXT,
    labels TEXT NOT NULL,
    predictions TEXT NOT NULL,
    metrics TEXT NOT NULL,
    latency_ms_avg REAL,
    cost_per_1000_usd REAL,
    unknown_costs INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    UNIQUE (benchmark_run_id, provider)
);

-- Provider defaults without credentials. Secrets stay in the environment.
CREATE TABLE provider_configs (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    provider TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('source', 'decision', 'generative')),
    enabled INTEGER NOT NULL DEFAULT 0,
    model TEXT,
    base_url TEXT,
    config TEXT NOT NULL DEFAULT '{}',
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, provider)
);
