-- Plugin registry, and the scene graph: nodes are plugin instances, edges are
-- directed data handovers between them, routed by the hub. Graphs may be
-- cyclic or acyclic.

-- Registered plugin packages; id comes from the manifest and is the install folder name.
CREATE TABLE IF NOT EXISTS plugins
(
    id                 TEXT     PRIMARY KEY,
    "name"             TEXT     NOT NULL,
    "version"          TEXT     NOT NULL,
    "type"             TEXT     NOT NULL CHECK ("type" IN ('SOURCE', 'SINK', 'TRANSFORM')),
    description        TEXT,
    manifest           TEXT     NOT NULL CHECK (json_valid(manifest) AND json_type(manifest) = 'object'),
    launch_args_schema TEXT     CHECK (launch_args_schema IS NULL OR json_valid(launch_args_schema)),
    in_schema          TEXT     CHECK (in_schema IS NULL OR json_valid(in_schema)),
    out_schema         TEXT     CHECK (out_schema IS NULL OR json_valid(out_schema)),
    install_path       TEXT     NOT NULL,
    archive_sha256     TEXT     NOT NULL,
    create_time        DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time        DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (id, "type")
);

CREATE INDEX IF NOT EXISTS plugins__name_idx ON plugins ("name");
CREATE INDEX IF NOT EXISTS plugins__type_idx ON plugins ("type");
CREATE INDEX IF NOT EXISTS plugins__create_time_idx ON plugins (create_time);
CREATE INDEX IF NOT EXISTS plugins__update_time_idx ON plugins (update_time);

-- plugin_id is the plugin type (plugins.id); the node's own id is the instance
-- id, which the plugin protocol calls plugin_id.
CREATE TABLE IF NOT EXISTS scene_nodes
(
    id          TEXT     PRIMARY KEY,
    "name"      TEXT,
    scene_id    TEXT     NOT NULL REFERENCES scenes (id) ON DELETE CASCADE,
    "type"      TEXT     NOT NULL CHECK ("type" IN ('SOURCE', 'SINK', 'TRANSFORM')),
    plugin_id   TEXT     NOT NULL,
    config      TEXT     NOT NULL DEFAULT '{}' CHECK (json_valid(config) AND json_type(config) = 'object'),
    create_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (plugin_id, "type") REFERENCES plugins (id, "type") ON DELETE RESTRICT,
    UNIQUE (scene_id, id)
);

CREATE INDEX IF NOT EXISTS scene_nodes__name_idx ON scene_nodes ("name");
CREATE INDEX IF NOT EXISTS scene_nodes__type_idx ON scene_nodes ("type");
CREATE INDEX IF NOT EXISTS scene_nodes__plugin_id_idx ON scene_nodes (plugin_id, "type");
CREATE INDEX IF NOT EXISTS scene_nodes__create_time_idx ON scene_nodes (create_time);
CREATE INDEX IF NOT EXISTS scene_nodes__update_time_idx ON scene_nodes (update_time);

CREATE TABLE IF NOT EXISTS scene_edges
(
    id             TEXT     PRIMARY KEY,
    "name"         TEXT,
    scene_id       TEXT     NOT NULL REFERENCES scenes (id) ON DELETE CASCADE,
    source_node_id TEXT     NOT NULL,
    target_node_id TEXT     NOT NULL,
    -- NULL: pass events through as-is.
    mapping        TEXT     CHECK (mapping IS NULL OR (json_valid(mapping) AND json_type(mapping) = 'object')),
    create_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (scene_id, source_node_id) REFERENCES scene_nodes (scene_id, id) ON DELETE CASCADE,
    FOREIGN KEY (scene_id, target_node_id) REFERENCES scene_nodes (scene_id, id) ON DELETE CASCADE,
    UNIQUE (scene_id, source_node_id, target_node_id)
);

-- Incoming-edge lookups, and the cascade from deleting a target node.
CREATE INDEX IF NOT EXISTS scene_edges__target_node_idx ON scene_edges (scene_id, target_node_id);
CREATE INDEX IF NOT EXISTS scene_edges__name_idx ON scene_edges ("name");
CREATE INDEX IF NOT EXISTS scene_edges__create_time_idx ON scene_edges (create_time);
CREATE INDEX IF NOT EXISTS scene_edges__update_time_idx ON scene_edges (update_time);
