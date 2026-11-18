-- Node layout for the editor: a JSON object (`{"x": …, "y": …}`, the
-- node's position on the canvas), live and in drafts. Not part of a node's
-- content: changing it is no graph change (no revision, no reload) and not a
-- draft change.
ALTER TABLE scene_nodes ADD COLUMN ui TEXT CHECK (ui IS NULL OR (json_valid(ui) AND json_type(ui) = 'object'));
ALTER TABLE scene_draft_nodes ADD COLUMN ui TEXT CHECK (ui IS NULL OR (json_valid(ui) AND json_type(ui) = 'object'));
