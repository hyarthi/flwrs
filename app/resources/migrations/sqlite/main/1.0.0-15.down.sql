-- Rows that only recorded a refusal have no warnings to keep.
DELETE FROM scene_node_configure_warnings WHERE errors <> '[]' AND warnings = '[]';
ALTER TABLE scene_node_configure_warnings DROP COLUMN errors;
