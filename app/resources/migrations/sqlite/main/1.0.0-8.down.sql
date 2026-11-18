-- Drafts go (their uploads' stored files are collected at the next start).
DROP INDEX IF EXISTS scene_draft_node_uploads__file_idx;
DROP TABLE scene_draft_node_uploads;
DROP INDEX IF EXISTS scene_draft_edges__target_node_idx;
DROP INDEX IF EXISTS scene_draft_edges__direction_idx;
DROP TABLE scene_draft_edges;
DROP TABLE scene_draft_nodes;
DROP TABLE scene_drafts;
ALTER TABLE scenes DROP COLUMN revision;
