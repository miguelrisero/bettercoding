-- Open Inputs created before priority existed carry their level as a "P<n> · " title prefix.
UPDATE workspace_inputs
SET priority = (
    SELECT substr(v.title, 1, 2) FROM workspace_input_versions v
    WHERE v.input_id = workspace_inputs.id AND v.version = workspace_inputs.current_version
)
WHERE status = 'open' AND priority = 'P2' AND EXISTS (
    SELECT 1 FROM workspace_input_versions v
    WHERE v.input_id = workspace_inputs.id AND v.version = workspace_inputs.current_version
      AND v.title GLOB 'P[0-4] · *'
);
