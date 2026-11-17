-- Files uploaded to a node (manifest v2 `"source": "upload"` launch args).
-- The bytes live in `[director] files_dir`, stored once per (name, content)
-- as `<stem>_<sha256><.ext>`; a stored file is deleted once no row names it.
-- Rows go with their node.
CREATE TABLE scene_node_uploads
(
    node_id     TEXT     NOT NULL REFERENCES scene_nodes (id) ON DELETE CASCADE,
    "name"      TEXT     NOT NULL,
    -- Lowercase hex SHA-256 of the content.
    sha256      TEXT     NOT NULL CHECK (length(sha256) = 64),
    "size"      INTEGER  NOT NULL CHECK ("size" >= 0),
    create_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (node_id, "name")
);

CREATE INDEX IF NOT EXISTS scene_node_uploads__file_idx ON scene_node_uploads ("name", sha256);
