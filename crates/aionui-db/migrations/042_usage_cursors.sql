-- Session cumulative counters are independent from current context occupancy.
CREATE TABLE usage_cursors (
    user_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    input_tokens INTEGER NOT NULL CHECK(input_tokens>=0),
    output_tokens INTEGER NOT NULL CHECK(output_tokens>=0),
    cache_read_tokens INTEGER NOT NULL CHECK(cache_read_tokens>=0),
    cache_creation_tokens INTEGER NOT NULL CHECK(cache_creation_tokens>=0),
    PRIMARY KEY(user_id,conversation_id,source)
);

ALTER TABLE usage_requests ADD COLUMN billing_reported INTEGER NOT NULL DEFAULT 1 CHECK(billing_reported IN (0,1));

ALTER TABLE usage_requests ADD COLUMN context_reported INTEGER NOT NULL DEFAULT 1 CHECK(context_reported IN (0,1));
