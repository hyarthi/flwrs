-- The warnings of a node's configurator runs, so `check` can show them (a
-- run's errors refuse the save; its warnings don't). Keyed by the node and a
-- hash of the launch args the run produced: live and the draft find the
-- warnings of the settings they hold, and a submit needs no copy. Not part of
-- a node's content (no revision, not a draft change, not exported). Rows of
-- nodes that are gone are removed with the scene service's garbage.
CREATE TABLE scene_node_configure_warnings
(
    node_id  TEXT     NOT NULL,
    args_key TEXT     NOT NULL,
    warnings TEXT     NOT NULL CHECK (json_valid(warnings) AND json_type(warnings) = 'array'),
    run_time DATETIME NOT NULL,
    PRIMARY KEY (node_id, args_key)
);
