-- Undo / redo for drafts: the states a draft has been in (after each
-- change), as JSON snapshots of its rows; `scene_drafts.history_at` is the
-- current one. Undo restores the one before, redo the one after; a change
-- after an undo drops the states after it. Bounded per draft (oldest go).
-- Goes with the draft (submit, discard).
ALTER TABLE scene_drafts ADD COLUMN history_at INTEGER NOT NULL DEFAULT 0;

CREATE TABLE scene_draft_history
(
    scene_id    TEXT     NOT NULL REFERENCES scene_drafts (scene_id) ON DELETE CASCADE,
    seq         INTEGER  NOT NULL,
    -- What the change that led here was (e.g. "Add Ticker").
    label       TEXT     NOT NULL,
    -- Requests sent with the same undo group make one step.
    undo_group  TEXT,
    snapshot    TEXT     NOT NULL CHECK (json_valid(snapshot)),
    "size"      INTEGER  NOT NULL,
    create_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (scene_id, seq)
);

-- The uploads the snapshots name, so their stored files are kept.
CREATE TABLE scene_draft_history_uploads
(
    scene_id TEXT    NOT NULL,
    seq      INTEGER NOT NULL,
    "name"   TEXT    NOT NULL,
    sha256   TEXT    NOT NULL,
    FOREIGN KEY (scene_id, seq) REFERENCES scene_draft_history (scene_id, seq) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS scene_draft_history_uploads__entry_idx ON scene_draft_history_uploads (scene_id, seq);
