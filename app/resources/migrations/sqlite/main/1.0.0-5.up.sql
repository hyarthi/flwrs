-- Nodes no longer reference the plugin registry with a foreign key: a scene
-- imported from another server may use plugins this server doesn't have (yet).
-- Such a scene is stored, but can't be enabled until its plugins are installed
-- (checked by the scene service). Nodes also record the plugin version they
-- were configured with (informational; carried in scene exports).
--
-- SQLite can't drop a constraint, so both graph tables are rebuilt. With
-- foreign keys enforced, the order matters: build the new tables, drop the
-- old edges then the old nodes, then rename (renaming scene_nodes_new also
-- rewrites the reference to it in scene_edges_new). Index names are global,
-- so indexes are created after the old tables (and theirs) are gone.

CREATE TABLE scene_nodes_new
(
    id             TEXT     PRIMARY KEY,
    "name"         TEXT,
    scene_id       TEXT     NOT NULL REFERENCES scenes (id) ON DELETE CASCADE,
    "type"         TEXT     NOT NULL CHECK ("type" IN ('SOURCE', 'SINK', 'TRANSFORM')),
    -- plugins.id, without a foreign key (see above).
    plugin_id      TEXT     NOT NULL,
    plugin_version TEXT,
    config         TEXT     NOT NULL DEFAULT '{}' CHECK (json_valid(config) AND json_type(config) = 'object'),
    create_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE (scene_id, id)
);

INSERT INTO scene_nodes_new (id, "name", scene_id, "type", plugin_id, plugin_version, config, create_time, update_time)
SELECT n.id, n."name", n.scene_id, n."type", n.plugin_id, p."version", n.config, n.create_time, n.update_time
FROM scene_nodes n
LEFT JOIN plugins p ON p.id = n.plugin_id;

CREATE TABLE scene_edges_new
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
    FOREIGN KEY (scene_id, source_node_id) REFERENCES scene_nodes_new (scene_id, id) ON DELETE CASCADE,
    FOREIGN KEY (scene_id, target_node_id) REFERENCES scene_nodes_new (scene_id, id) ON DELETE CASCADE,
    UNIQUE (scene_id, source_node_id, target_node_id)
);

INSERT INTO scene_edges_new (id, "name", scene_id, source_node_id, target_node_id, mapping, create_time, update_time)
SELECT id, "name", scene_id, source_node_id, target_node_id, mapping, create_time, update_time
FROM scene_edges;

DROP TABLE scene_edges;
DROP TABLE scene_nodes;
ALTER TABLE scene_nodes_new RENAME TO scene_nodes;
ALTER TABLE scene_edges_new RENAME TO scene_edges;

CREATE INDEX IF NOT EXISTS scene_nodes__name_idx ON scene_nodes ("name");
CREATE INDEX IF NOT EXISTS scene_nodes__type_idx ON scene_nodes ("type");
CREATE INDEX IF NOT EXISTS scene_nodes__plugin_id_idx ON scene_nodes (plugin_id, "type");
CREATE INDEX IF NOT EXISTS scene_nodes__create_time_idx ON scene_nodes (create_time);
CREATE INDEX IF NOT EXISTS scene_nodes__update_time_idx ON scene_nodes (update_time);

CREATE INDEX IF NOT EXISTS scene_edges__target_node_idx ON scene_edges (scene_id, target_node_id);
CREATE INDEX IF NOT EXISTS scene_edges__name_idx ON scene_edges ("name");
CREATE INDEX IF NOT EXISTS scene_edges__create_time_idx ON scene_edges (create_time);
CREATE INDEX IF NOT EXISTS scene_edges__update_time_idx ON scene_edges (update_time);
