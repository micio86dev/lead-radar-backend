-- Community engagement policy.
--
-- A community is not only a place to read: it is a place with its own rules about promotion. The
-- operator records what the moderators published, and the drafting and publication paths refuse to
-- pretend it is not there. `self_promotion` stays 'unknown' until a human records it, and unknown is
-- treated exactly like forbidden when a pitch is about to be recorded as published.

ALTER TABLE communities ADD COLUMN self_promotion TEXT NOT NULL DEFAULT 'unknown'
    CHECK (self_promotion IN ('allowed', 'ask_mods', 'forbidden', 'unknown'));
ALTER TABLE communities ADD COLUMN min_account_age_days INTEGER NOT NULL DEFAULT 0;
ALTER TABLE communities ADD COLUMN min_comment_karma INTEGER NOT NULL DEFAULT 0;
ALTER TABLE communities ADD COLUMN flair_required INTEGER NOT NULL DEFAULT 0;
ALTER TABLE communities ADD COLUMN promo_cooldown_days INTEGER NOT NULL DEFAULT 7;
ALTER TABLE communities ADD COLUMN rules_summary TEXT NOT NULL DEFAULT '';
