-- Only provider-reported request deltas belong here; never infer spend from context occupancy.
CREATE TABLE usage_requests (
 request_id TEXT PRIMARY KEY,
 user_id TEXT NOT NULL,
 conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
 provider TEXT NOT NULL,
 model TEXT NOT NULL,
 is_child INTEGER NOT NULL DEFAULT 0 CHECK(is_child IN(0,1)),
 input_tokens INTEGER NOT NULL CHECK(input_tokens >= 0),
 output_tokens INTEGER NOT NULL CHECK(output_tokens >= 0),
 cache_read_tokens INTEGER NOT NULL CHECK(cache_read_tokens >= 0),
 cache_creation_tokens INTEGER NOT NULL CHECK(cache_creation_tokens >= 0),
 context_tokens INTEGER NOT NULL CHECK(context_tokens >= 0),
 context_window INTEGER NOT NULL CHECK(context_window >= 0),
 breakdown TEXT NOT NULL,
 created_at INTEGER NOT NULL
);
CREATE INDEX usage_requests_scope ON usage_requests(user_id, created_at, conversation_id);
