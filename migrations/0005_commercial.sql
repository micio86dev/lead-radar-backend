-- Versioned scoring configuration, leads and score history.
CREATE TABLE lead_score_versions (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    weights TEXT NOT NULL,
    bands TEXT NOT NULL,
    thresholds TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL,
    UNIQUE (tenant_id, version)
);

CREATE UNIQUE INDEX idx_score_version_active
    ON lead_score_versions(tenant_id) WHERE active = 1;

CREATE TABLE leads (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    post_id TEXT NOT NULL REFERENCES discovered_posts(id) ON DELETE CASCADE,
    title TEXT,
    language TEXT,
    source_id TEXT REFERENCES sources(id) ON DELETE SET NULL,
    community TEXT,
    source_url TEXT,
    author TEXT,
    published_at TEXT,
    discovered_at TEXT NOT NULL,
    vertical_primary TEXT NOT NULL,
    vertical_secondary TEXT NOT NULL DEFAULT '[]',
    intent TEXT NOT NULL,
    customer_type TEXT NOT NULL,
    buying_intent TEXT NOT NULL,
    business_potential TEXT NOT NULL,
    product_fit TEXT NOT NULL,
    confidence REAL,
    evidence TEXT NOT NULL DEFAULT '[]',
    score INTEGER,
    score_category TEXT,
    qualification_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (qualification_status IN ('pending', 'qualified', 'needs_review', 'rejected')),
    review_status TEXT NOT NULL DEFAULT 'none'
        CHECK (review_status IN ('none', 'pending', 'in_review', 'resolved')),
    is_active INTEGER NOT NULL DEFAULT 1,
    analysis_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, post_id)
);

CREATE INDEX idx_leads_score ON leads(tenant_id, score DESC);
CREATE INDEX idx_leads_status ON leads(tenant_id, qualification_status);
CREATE INDEX idx_leads_vertical ON leads(tenant_id, vertical_primary);
CREATE INDEX idx_leads_published ON leads(tenant_id, published_at DESC);

CREATE TABLE lead_scores (
    id TEXT PRIMARY KEY,
    lead_id TEXT NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
    score_version INTEGER NOT NULL,
    score INTEGER NOT NULL,
    category TEXT NOT NULL,
    components TEXT NOT NULL,
    inputs TEXT NOT NULL,
    decision_run_id TEXT REFERENCES decision_runs(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_lead_scores_lead ON lead_scores(lead_id, created_at DESC);
