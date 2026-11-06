-- Per-scene hop limit: an event is dropped after this many handovers.
-- NULL = the server default ([director] max_hops). The range matches
-- scene::service::MAX_HOPS_LIMIT.
ALTER TABLE scenes ADD COLUMN max_hops INTEGER CHECK (max_hops IS NULL OR max_hops BETWEEN 1 AND 10000);
