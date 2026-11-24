-- What a configurator refused about settings a draft keeps anyway (an
-- unfinished graph): `check` lists them as problems until the settings change.
ALTER TABLE scene_node_configure_warnings
    ADD COLUMN errors TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(errors) AND json_type(errors) = 'array');
