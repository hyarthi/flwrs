-- A draft's submit, when its pre-flight check held it back: WAITING (retrying
-- in the background) or FAILED, with the problems found. Live is untouched in
-- both. Not part of the draft's content (no history, no content hash).
ALTER TABLE scene_drafts ADD COLUMN submit_state TEXT CHECK (submit_state IN ('WAITING', 'FAILED'));
ALTER TABLE scene_drafts ADD COLUMN submit_problems TEXT
    CHECK (submit_problems IS NULL OR (json_valid(submit_problems) AND json_type(submit_problems) = 'array'));
ALTER TABLE scene_drafts ADD COLUMN submit_message TEXT;
ALTER TABLE scene_drafts ADD COLUMN submit_time DATETIME;
ALTER TABLE scene_drafts ADD COLUMN submit_id TEXT;
