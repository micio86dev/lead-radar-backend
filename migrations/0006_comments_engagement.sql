-- Human-reviewed comment workflow and manual engagement recording.
CREATE TABLE generated_comments (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    lead_id TEXT NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
    variant TEXT NOT NULL CHECK (variant IN ('helpful', 'soft_pitch', 'direct_pitch')),
    body TEXT NOT NULL,
    language TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'draft'
        CHECK (status IN ('draft', 'pending_review', 'needs_edit', 'approved', 'rejected', 'published_manually')),
    version INTEGER NOT NULL DEFAULT 1,
    published_at TEXT,
    publication_note TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, lead_id, variant)
);

CREATE INDEX idx_comments_tenant_status ON generated_comments(tenant_id, status);

CREATE TABLE comment_versions (
    id TEXT PRIMARY KEY,
    comment_id TEXT NOT NULL REFERENCES generated_comments(id) ON DELETE CASCADE,
    version INTEGER NOT NULL,
    body TEXT NOT NULL,
    actor TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (comment_id, version)
);

CREATE TABLE review_events (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    comment_id TEXT NOT NULL REFERENCES generated_comments(id) ON DELETE CASCADE,
    event TEXT NOT NULL,
    actor TEXT NOT NULL,
    note TEXT,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_review_events_comment ON review_events(comment_id, created_at);

CREATE TABLE engagements (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    lead_id TEXT NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
    comment_id TEXT REFERENCES generated_comments(id) ON DELETE SET NULL,
    kind TEXT NOT NULL CHECK (kind IN ('reply', 'dm', 'meeting', 'interest', 'lost', 'other')),
    note TEXT,
    occurred_at TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX idx_engagements_lead ON engagements(tenant_id, lead_id, occurred_at DESC);
