-- Scene-level groups (`[{id, family, policy}]`), live and in drafts. A node's
-- membership is in its `config` (`groups: [id, …]`).
ALTER TABLE scenes ADD COLUMN "groups" TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid("groups") AND json_type("groups") = 'array');
ALTER TABLE scene_drafts ADD COLUMN "groups" TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid("groups") AND json_type("groups") = 'array');
