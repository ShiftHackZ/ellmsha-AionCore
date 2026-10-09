-- The bundled native connection is presented as Ollama; preserve user-named connections.
UPDATE agent_metadata
SET name = 'Ollama', icon = '/api/assets/logos/ai-major/ollama.png',
    updated_at = unixepoch('now', 'subsec') * 1000
WHERE agent_type = 'aionrs' AND agent_source = 'internal'
  AND name IN ('Aion CLI', 'AionCLI', 'eLLMsha');
