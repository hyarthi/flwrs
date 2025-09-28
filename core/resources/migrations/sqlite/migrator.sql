CREATE TABLE IF NOT EXISTS migrations
(
    id            INTEGER  NOT NULL PRIMARY KEY AUTOINCREMENT,
    version_major INTEGER  NOT NULL,
    version_minor INTEGER  NOT NULL,
    version_patch INTEGER  NOT NULL,
    build_number  INTEGER  NOT NULL,
    file_name     TEXT     NOT NULL,
    file_hash     TEXT     NOT NULL,
    start_time    DATETIME NOT NULL,
    finish_time   DATETIME NULL,
    state         INTEGER  NOT NULL -- ENUM
);

CREATE UNIQUE INDEX IF NOT EXISTS migrations__uniq_idx ON migrations (version_major, version_minor, version_patch, build_number);
CREATE INDEX IF NOT EXISTS migrations__file_name_idx ON migrations (file_name);
CREATE INDEX IF NOT EXISTS migrations__state_idx ON migrations (state);
-- CREATE INDEX migrations__file_hash_idx ON migrations (file_hash);
