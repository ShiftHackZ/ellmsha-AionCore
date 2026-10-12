# Conversation working brief API

`GET /api/conversations/{id}/task-context` and `PUT /api/conversations/{id}/task-context` use the existing authenticated conversation API. `id` is a conversation ID owned by the current user. Runtime-token callers are additionally restricted to their token's conversation header. Cookie-authenticated writes retain the application CSRF protection; runtime credentials use the existing non-cookie channel.

This is bounded working state in `conversation.extra.task_context`, not canonical knowledge, a permission grant, an enforced interpretation of natural language, or a task-completion verifier. Newer user input wins. Generic conversation-extra updates cannot replace the brief. Reset clears it; forks reconstruct it from their own reminted messages.

## GET response

Standard `ApiResponse.data` contains:

| Property | Type | Meaning |
| --- | --- | --- |
| `conversation_id` | string | Owning conversation ID |
| `context` | object or null | Last validated brief; null before the first update |
| `latest_user_message_id` | string or null | Latest visible text message on the user side, excluding hidden continuations |
| `needs_reconciliation` | boolean | Brief predates the latest user input; re-read that input before relying on it |

`context` contains `revision` (positive integer), `updated_at` (server epoch milliseconds), `based_on_user_message_id`, `constraints`, and `evidence` (the latter three have the PUT shapes below).

## PUT request

Full replacement, maximum 16 KiB serialized UTF-8; unknown fields are rejected. No secrets, passwords, token-bearing URLs or copied private tool payloads: use account references and short safe quotations.

| Property | Type | Constraints |
| --- | --- | --- |
| `expected_revision` | integer | GET revision, or 0 if context is null; less than signed 64-bit maximum |
| `based_on_user_message_id` | string | 1–256 bytes; must still be the latest visible user message when the write commits |
| `constraints` | array | Full replacement; at most 16 entries; retain applicable earlier requirements |
| `evidence` | array | Full replacement; at most 16 entries; empty means no acceptance evidence recorded |

Constraint properties:

| Property | Type | Constraints |
| --- | --- | --- |
| `key` | string | Unique stable key, 1–64 ASCII letters/digits/underscore/hyphen |
| `kind` | enum | `branch`, `environment`, `connector`, `browser_target`, `receiver`, `acceptance`, `other` |
| `value` | string | Agent summary, 1–512 UTF-8 bytes |
| `source_message_id` | string | 1–256 bytes; existing visible user text message in this conversation |
| `source_quote` | string | Exact nonempty substring of that message, at most 512 UTF-8 bytes |

A later source for a key cannot be overwritten by an older source for that key. Source-existence/exact-quote checks establish provenance, not that the model interpreted the text correctly. Replace a changed criterion's outdated evidence rather than carrying it forward.

Evidence properties:

| Property | Type | Constraints |
| --- | --- | --- |
| `criterion_key` | string | References an `acceptance` constraint in this replacement |
| `level` | enum | `unit`, `integration`, `ui`, `manual`; these categories remain distinct |
| `reported_result` | enum | `passed`, `failed`, `incomplete`; explicitly model-reported |
| `message_ids` | string array | 1–8 existing tool-message IDs in this conversation, not older than the criterion's user source; each 1–256 bytes |
| `summary` | string | Factual observation, 1–512 UTF-8 bytes; do not duplicate private output |

The server verifies reference existence and tool-message type, not the truth of a success claim. Passing a unit test does not establish that a requested browser/user scenario worked. Read the referenced outputs and distinguish completed checks from missing ones.

PUT returns the same shape as GET. It atomically compares both revision and latest visible user-message ID; a concurrent correction or writer returns 409. Read and reconcile instead of blind retry. A response may already show `needs_reconciliation` when another user message arrives immediately after the committed write.

## Errors and runtime delivery

- 400: unknown/malformed fields, bounds, nonexact/missing/foreign source, invalid evidence reference, older-source replacement.
- 401: missing/invalid authentication (existing middleware).
- 403: runtime caller attempts another conversation, or cookie write fails CSRF.
- 404: conversation not owned by caller or nonexistent.
- 409: revision or latest user input changed (`TASK_CONTEXT_CONFLICT` in message).
- 500: repository failure; no success is claimed.

`task_context_read` and `task_context_update` on the existing chat-history MCP accept no conversation target: they bind to the calling runtime. Use them for multi-step work, not trivial replies. At each application turn dispatch, including continuation/replay, the backend reloads any brief and prepends escaped JSON labelled as agent-recorded working data. The new user message remains last. No provider-specific undocumented system field is used. This keeps the brief available after provider compaction/restart, without claiming that a model is guaranteed to obey it. Existing tool outputs and user messages remain authoritative evidence in the conversation store.
