-- Projects: the product a tenant works on, and the communities that fit it.
--
-- Until now the product was a constant in the code. A project carries the operator's own words —
-- a summary and the public pages that describe it — and the matching analysis ranks the already
-- validated and enabled communities against that material. A match is advisory: it never enables,
-- disables or validates a community, and nothing here collects content.

CREATE TABLE projects (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    summary TEXT NOT NULL DEFAULT '',
    -- JSON array of public URLs the analysis may read.
    website_urls TEXT NOT NULL DEFAULT '[]',
    -- The project whose description the drafting prompts use. One per tenant at most.
    is_default INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE UNIQUE INDEX projects_tenant_name ON projects(tenant_id, name);
CREATE UNIQUE INDEX projects_one_default ON projects(tenant_id) WHERE is_default = 1;

CREATE TABLE project_communities (
    project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
    community_id TEXT NOT NULL REFERENCES communities(id) ON DELETE CASCADE,
    relevance INTEGER NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    matched_at TEXT NOT NULL,
    PRIMARY KEY (project_id, community_id)
);
