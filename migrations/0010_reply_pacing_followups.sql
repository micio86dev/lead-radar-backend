-- Reply pacing and follow-ups.
--
-- Reddit throttles an account on its own terms. reply_pacing stores the next time a reply is expected to
-- be accepted (either the conservative floor after a recorded publication, or the wait Reddit announced),
-- and followups tracks threads where a reply was published and someone may answer.

CREATE TABLE reply_pacing (
    tenant_id TEXT PRIMARY KEY REFERENCES tenants(id) ON DELETE CASCADE,
    next_reply_at TEXT,
    source TEXT NOT NULL CHECK (source IN ('publication', 'reddit_reported', 'manual')),
    updated_at TEXT NOT NULL
);

CREATE TABLE followups (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    lead_id TEXT NOT NULL REFERENCES leads(id) ON DELETE CASCADE,
    comment_id TEXT REFERENCES generated_comments(id) ON DELETE SET NULL,
    state TEXT NOT NULL DEFAULT 'waiting' CHECK (state IN ('waiting', 'answered', 'closed')),
    new_replies INTEGER NOT NULL DEFAULT 0,
    last_reporter TEXT,
    last_excerpt TEXT,
    last_reply_at TEXT,
    reported_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    UNIQUE (tenant_id, lead_id)
);

CREATE INDEX idx_followups_state ON followups(tenant_id, state, updated_at DESC);
