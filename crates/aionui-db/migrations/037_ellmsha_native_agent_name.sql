-- Rename the bundled native agent. Preserve custom agents and user-assigned names.
-- Generated assistant names are refreshed from this row by the existing catalog sync.
UPDATE agent_metadata
SET name = 'eLLMsha', updated_at = unixepoch('now', 'subsec') * 1000
WHERE agent_type = 'aionrs'
  AND agent_source = 'internal'
  AND name IN ('Aion CLI', 'AionCLI');
