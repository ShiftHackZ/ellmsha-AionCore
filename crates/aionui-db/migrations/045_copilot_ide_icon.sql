-- Correct the managed IDE agent without changing provider or account state.
UPDATE agent_metadata SET icon = '/api/assets/logos/tools/copilot.svg' WHERE agent_id = 'copilot-ide';
