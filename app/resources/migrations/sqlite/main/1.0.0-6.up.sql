-- Scene-level defaults for the scene's nodes, as a JSON object (see
-- node_config::SceneDefaults): `durable` (queues), `remote_auth`
-- ("token" | "mtls") and `require_tls` (remote nodes). A node's own setting
-- wins over the scene's.
ALTER TABLE scenes ADD COLUMN defaults TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(defaults) AND json_type(defaults) = 'object');
