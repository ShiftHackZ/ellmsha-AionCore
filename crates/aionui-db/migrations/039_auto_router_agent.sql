-- Virtual agent resolves to an existing concrete runtime before chat creation.
INSERT INTO agent_metadata
(id, agent_id, name, backend, agent_type, agent_source, enabled, command, args, env,
 behavior_policy, sort_order, created_at, updated_at)
VALUES ('auto-router', 'auto-router', 'Auto Router', 'auto-router', 'aionrs', 'internal', 1, NULL, '[]', '[]',
 '{"supports_team":true}', 90, unixepoch('now','subsec')*1000, unixepoch('now','subsec')*1000)
ON CONFLICT(agent_id) DO NOTHING;
