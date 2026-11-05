-- Whether the scene's workflow runs (TRUE) or is dormant (FALSE).
-- SQLite stores BOOLEAN as INTEGER 0/1; the CHECK rejects anything else.
ALTER TABLE scenes ADD COLUMN enabled BOOLEAN NOT NULL DEFAULT FALSE CHECK (enabled IN (FALSE, TRUE));

CREATE INDEX IF NOT EXISTS scenes__enabled_idx ON scenes (enabled);
