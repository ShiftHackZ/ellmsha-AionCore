-- Canonical history revisions and durable redaction fences. No plaintext before-images.
ALTER TABLE messages ADD COLUMN revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE messages ADD COLUMN redacted INTEGER NOT NULL DEFAULT 0;
CREATE TABLE IF NOT EXISTS message_redaction_receipts (
    owner_user_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    receipt TEXT NOT NULL,
    PRIMARY KEY(owner_user_id, conversation_id, operation_id)
);
CREATE TRIGGER IF NOT EXISTS messages_content_revision
AFTER UPDATE OF content ON messages
WHEN NEW.content <> OLD.content AND NEW.revision = OLD.revision
BEGIN
    UPDATE messages SET revision=OLD.revision+1 WHERE id=NEW.id;
END;
