-- Restores the foreign key from nodes to plugins and drops plugin_version.
-- Nodes whose plugin isn't registered (or has another type) can't satisfy the
-- foreign key: they're dropped, with their edges. Same rebuild order as the
-- up script.

CREATE TABLE scene_nodes_new
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

INSERT INTO scene_nodes_new (id, "name", scene_id, "type", plugin_id, config, create_time, update_time)
SELECT n.id, n."name", n.scene_id, n."type", n.plugin_id, n.config, n.create_time, n.update_time
FROM scene_nodes n
WHERE EXISTS (SELECT 1 FROM plugins p WHERE p.id = n.plugin_id AND p."type" = n."type");

CREATE TABLE scene_edges_new
(
    id             TEXT     PRIMARY KEY,
    "name"         TEXT,
    scene_id       TEXT     NOT NULL REFERENCES scenes (id) ON DELETE CASCADE,
    source_node_id TEXT     NOT NULL,
    target_node_id TEXT     NOT NULL,
    mapping        TEXT     CHECK (mapping IS NULL OR (json_valid(mapping) AND json_type(mapping) = 'object')),
    create_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    update_time    DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (scene_id, source_node_id) REFERENCES scene_nodes_new (scene_id, id) ON DELETE CASCADE,
    FOREIGN KEY (scene_id, target_node_id) REFERENCES scene_nodes_new (scene_id, id) ON DELETE CASCADE,
    UNIQUE (scene_id, source_node_id, target_node_id)
);

INSERT INTO scene_edges_new (id, "name", scene_id, source_node_id, target_node_id, mapping, create_time, update_time)
SELECT e.id, e."name", e.scene_id, e.source_node_id, e.target_node_id, e.mapping, e.create_time, e.update_time
FROM scene_edges e
WHERE EXISTS (SELECT 1 FROM scene_nodes_new n WHERE n.id = e.source_node_id)
  AND EXISTS (SELECT 1 FROM scene_nodes_new n WHERE n.id = e.target_node_id);

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
