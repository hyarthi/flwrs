-- Scene drafts: a scene's graph edited apart from the live one, then
-- submitted at once (one transaction, one reload). At most one draft per
-- scene. Node and edge ids are the live ones (a node added in the draft gets
-- its id there), so the draft tables mirror the live ones and diffs are by id.
--
-- `scenes.revision` counts committed changes to the live graph (settings,
-- nodes, edges, uploads); a draft remembers the revision it's based on.
ALTER TABLE scenes ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;

-- A draft's scene settings and where it started from.
CREATE TABLE scene_drafts
(
    scene_id      TEXT     PRIMARY KEY REFERENCES scenes (id) ON DELETE CASCADE,
    -- The live revision the draft is based on (or was last rebased onto).
    base_revision INTEGER  NOT NULL,
    "name"        TEXT     NOT NULL,
    max_hops      INTEGER  CHECK (max_hops IS NULL OR max_hops BETWEEN 1 AND 10000),
    defaults      TEXT     NOT NULL DEFAULT '{}' CHECK (json_valid(defaults) AND json_type(defaults) = 'object'),
    -- Hash of the live settings at the base (to tell who changed what).
    base_hash     TEXT     NOT NULL,
    -- Set by a rebase when the draft and live both changed the settings.
    conflict      TEXT     CHECK (conflict IS NULL OR conflict = 'changed_both'),
    create_time   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time   DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- The draft's nodes: the live columns, plus
-- - base_hash: the live node's content hash at the base; NULL = added in the draft;
-- - removed: removed in the draft (kept as a tombstone, shown as a shadow);
-- - conflict: set by a rebase (changed_both, removed_live: removed live but
--   changed here, changed_live: changed live but removed here).
CREATE TABLE scene_draft_nodes
(
    id             TEXT     PRIMARY KEY,
    "name"         TEXT,
    scene_id       TEXT     NOT NULL REFERENCES scene_drafts (scene_id) ON DELETE CASCADE,
    "type"         TEXT     NOT NULL CHECK ("type" IN ('SOURCE', 'SINK', 'TRANSFORM')),
    plugin_id      TEXT     NOT NULL,
    plugin_version TEXT,
    config         TEXT     NOT NULL DEFAULT '{}' CHECK (json_valid(config) AND json_type(config) = 'object'),
    create_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    base_hash      TEXT,
    removed        BOOLEAN  NOT NULL DEFAULT FALSE,
    conflict       TEXT     CHECK (conflict IS NULL OR conflict IN ('changed_both', 'removed_live', 'changed_live')),
    UNIQUE (scene_id, id)
);

-- The draft's edges: as the nodes. At most one per direction among those
-- not removed.
CREATE TABLE scene_draft_edges
(
    id             TEXT     PRIMARY KEY,
    "name"         TEXT,
    scene_id       TEXT     NOT NULL REFERENCES scene_drafts (scene_id) ON DELETE CASCADE,
    source_node_id TEXT     NOT NULL,
    target_node_id TEXT     NOT NULL,
    mapping        TEXT     CHECK (mapping IS NULL OR (json_valid(mapping) AND json_type(mapping) = 'object')),
    create_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    base_hash      TEXT,
    removed        BOOLEAN  NOT NULL DEFAULT FALSE,
    conflict       TEXT     CHECK (conflict IS NULL OR conflict IN ('changed_both', 'removed_live', 'changed_live')),
    FOREIGN KEY (scene_id, source_node_id) REFERENCES scene_draft_nodes (scene_id, id) ON DELETE CASCADE,
    FOREIGN KEY (scene_id, target_node_id) REFERENCES scene_draft_nodes (scene_id, id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS scene_draft_edges__direction_idx
    ON scene_draft_edges (scene_id, source_node_id, target_node_id) WHERE removed = FALSE;
CREATE INDEX IF NOT EXISTS scene_draft_edges__target_node_idx ON scene_draft_edges (scene_id, target_node_id);

-- Files uploaded to draft nodes: as scene_node_uploads (the same
-- content-addressed store; a stored file stays while either table names it).
CREATE TABLE scene_draft_node_uploads
(
    node_id     TEXT     NOT NULL REFERENCES scene_draft_nodes (id) ON DELETE CASCADE,
    "name"      TEXT     NOT NULL,
    sha256      TEXT     NOT NULL CHECK (length(sha256) = 64),
    "size"      INTEGER  NOT NULL CHECK ("size" >= 0),
    create_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (node_id, "name")
);

CREATE INDEX IF NOT EXISTS scene_draft_node_uploads__file_idx ON scene_draft_node_uploads ("name", sha256);
