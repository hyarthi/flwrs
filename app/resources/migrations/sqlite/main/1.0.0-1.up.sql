CREATE TABLE IF NOT EXISTS scenes
(
    id TEXT PRIMARY KEY,
    "name" TEXT NOT NULL,
    create_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX scenes__name_idx ON scenes (name);
CREATE INDEX scenes__create_time_idx ON scenes (create_time);
CREATE INDEX scenes__update_time_idx ON scenes (update_time);
