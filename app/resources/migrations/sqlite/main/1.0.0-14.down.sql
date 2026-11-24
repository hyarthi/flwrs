-- Node configs are read strictly, so memberships go with the groups. Undo
-- states recorded with groups could bring them back, so those go too.
UPDATE scene_nodes SET config = json_remove(config, '$.groups')
    WHERE json_type(config, '$.groups') IS NOT NULL;
UPDATE scene_draft_nodes SET config = json_remove(config, '$.groups')
    WHERE json_type(config, '$.groups') IS NOT NULL;
DELETE FROM scene_draft_history WHERE json_type(snapshot, '$.draft.groups') IS NOT NULL;
ALTER TABLE scene_drafts DROP COLUMN "groups";
ALTER TABLE scenes DROP COLUMN "groups";
