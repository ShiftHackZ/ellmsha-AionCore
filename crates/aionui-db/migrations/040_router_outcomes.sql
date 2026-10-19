-- Explicit authenticated user acceptance, never inferred from ordinary completion.
CREATE TABLE IF NOT EXISTS router_outcomes (
 user_id TEXT NOT NULL, conversation_id TEXT NOT NULL, decision_id TEXT NOT NULL, message_id TEXT NOT NULL,
 workspace TEXT NOT NULL, target_json TEXT NOT NULL, task TEXT NOT NULL,
 accepted INTEGER NOT NULL CHECK(accepted IN(0,1)), updated_at INTEGER NOT NULL,
 PRIMARY KEY(user_id, conversation_id),
 FOREIGN KEY(conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS router_outcomes_scope ON router_outcomes(user_id, workspace, task, updated_at);
