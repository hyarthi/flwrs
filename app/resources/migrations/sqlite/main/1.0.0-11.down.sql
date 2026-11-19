DROP INDEX IF EXISTS scene_draft_history_uploads__entry_idx;
DROP TABLE scene_draft_history_uploads;
DROP TABLE scene_draft_history;
ALTER TABLE scene_drafts DROP COLUMN history_at;
