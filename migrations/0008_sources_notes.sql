-- Validation evidence for a source (mirrors communities). Added after 0002 was released, so it is a
-- new migration rather than an edit of an applied one.
ALTER TABLE sources ADD COLUMN notes TEXT;
