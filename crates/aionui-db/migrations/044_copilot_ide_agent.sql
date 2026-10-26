-- Desktop Copilot uses the native harness with its managed IDE model provider.
-- Keep it disabled until the desktop connects and discovers its model catalog.
INSERT INTO agent_metadata
(id, agent_id, name, backend, agent_type, agent_source, enabled, command, args, env,
 behavior_policy, sort_order, created_at, updated_at)
VALUES ('copilot-ide', 'copilot-ide', 'Copilot IDE', 'aionrs', 'aionrs', 'internal', 0, NULL, '[]', '[]',
 '{"supports_team":true}', 91, unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(agent_id) DO NOTHING;
