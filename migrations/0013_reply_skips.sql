-- Reply skips: a lead the operator decided not to answer. Reversible, audited, and never a substitute for
-- a publication record: the two states are independent columns and the queue views are disjoint.
ALTER TABLE leads ADD COLUMN reply_skipped_at TEXT;
ALTER TABLE leads ADD COLUMN reply_skip_reason TEXT;

CREATE INDEX idx_leads_reply_skipped ON leads(tenant_id, reply_skipped_at);
