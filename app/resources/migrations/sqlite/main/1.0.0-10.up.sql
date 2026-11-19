-- Edge layout for the editor: a JSON object (`{"x": …, "y": …}`, the point
-- the edge passes through, where its box sits), live and in drafts. As node
-- layout (1.0.0-9): not content, so changing it is no graph change.
ALTER TABLE scene_edges ADD COLUMN ui TEXT CHECK (ui IS NULL OR (json_valid(ui) AND json_type(ui) = 'object'));
ALTER TABLE scene_draft_edges ADD COLUMN ui TEXT CHECK (ui IS NULL OR (json_valid(ui) AND json_type(ui) = 'object'));
