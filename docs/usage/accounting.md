# Token usage

The authenticated `GET /api/usage` API returns durable usage for the current account.
The shared renderer view is mounted in Settings → Usage and as an existing preview
panel tab, opened beside Code indexes. Reads do not create or resume agent runtimes.

## Accounting boundaries

- Native eLLMsha/AionRS emits a unique engine-generated request ID after a provider
  reports nonzero usage. Input/output are request deltas, including agentic tool
  rounds. Internal native children forward accounting without exposing their text;
  separate routed child conversations remain in their own exact-chat views.
- `input_tokens` already includes provider cache reads/creation. Cached counts are
  subsets, not extra tokens. Reasoning is included only when the provider includes
  it in output. Prices and dollar costs are deliberately not inferred.
- Context is the latest non-child request's input plus output, not a sum across
  requests. Its composition reuses AionRS's local estimator, normalized to the
  reported size. Tools here means definition schemas; tool results are in messages.
  The context limit is the configured capacity, not independently probed capacity.
- Session-only probes, ordinary native chats, and native models behind configured
  API providers use the same path. A provider omitting counters produces no record.
- Direct Claude uses the captured result's per-turn aggregate; Codex uses
  `tokenUsage.total` with persisted session high-water marks while `last` remains
  context occupancy. Antigravity counts only final result usage, never both step
  and final usage. CLI-owned internal children are not host-routed child chats;
  their independent spend is unavailable unless the adapter reports it in these
  authoritative counters. It must not be inferred from workflow progress.
- ACP end-turn SDK `Usage` is cumulative across turns, differenced durably per
  provider/session. The supported quota dialect is per prompt. `UsageUpdate`
  notifications supply context only. Adapters omitting billing counters show
  unavailable spend, rather than fabricated zeroes. An unseen resumed/forked
  session establishes a baseline, so historical usage is not charged today.
- Native compaction, successful memory-generation completions, and V2 custom
  classifier completions with host-owned conversation identity contribute spend
  without replacing the chat's context snapshot. Malformed classifier JSON is
  still counted when provider usage was received. A generation failure before
  completion or cancelled stream without final usage cannot be accounted for.
- This ledger does not reconstruct old sessions. Reports are recorded when
  received; an aggregate CLI turn crossing midnight is attributed to receipt day.
  "Usage reports" counts measurements, not a comparable API-call count across
  different protocols. No estimates are inserted for missing provider counters.
- No prompts, message bodies, tool arguments, credentials or reasoning are stored.
  Conversation deletion cascades to the associated usage. Account ownership is
  enforced at both ingestion and querying. Repeated request delivery is ignored.
- Persistence errors and event-bus lag are logged as errors; no estimate silently
  replaces a lost record. This is observable request accounting, not a billing ledger.

## Query

All parameters are optional. Authentication and user scope follow ordinary chat
routes. Errors use the existing API error envelope and HTTP 400/401/500 conventions.

| Field | Type | Meaning |
| --- | --- | --- |
| `days` | integer, 1–365 | Rolling duration, default 30; ignored for the start boundary when `start` is supplied |
| `start` | nonnegative int64 | Inclusive UTC Unix milliseconds; defaults to `end - days` |
| `end` | nonnegative int64 | Exclusive UTC Unix milliseconds; defaults to current time + 1 ms |
| `conversation_id` | string | Exact owned chat only (not other routed conversations); omitted means all owned chats |
| `provider` | string | Exact configured provider or agent ID; omitted means all |
| `model` | string | Exact executed model ID; omitted means all |

A conversation query with no `days`, `start` or `end` covers its entire recorded lifetime (`start=0`). Explicit/global ranges cannot exceed 366 days. Reversed, overflowing, negative and malformed ranges
are rejected. Provider display names are resolved only inside the current account.
UTC days are used for aggregation; custom-date selection uses local midnight bounds.

## Response

`{ "success": true, "data": UsageResponse }`; the Rust response contract is in
`crates/aionui-db/src/repository/usage_types.rs` and the renderer contract in
`components/Usage/usageModel.ts`. All token counts are nonnegative integers.

- `start`, `end`: actual resolved range in UTC Unix milliseconds.
- `totals`: `input_tokens`, `output_tokens`, `cache_read_tokens`,
  `cache_creation_tokens`, `requests` across matching unique records.
- `series`: disjoint rows grouped by UTC `period` (`YYYY-MM-DD`), `provider`,
  current `provider_name`, and actual `model`, with the same five counters.
- `contexts`: latest non-child snapshot per matching conversation, newest first.
  Each includes `provider`, current `provider_name`, `model`, `conversation_id`,
  current `conversation_name`, `context_tokens`,
  `context_window`, `created_at` (UTC Unix ms), and estimated `breakdown`:
  `system`, `memory`, `skills`, `tools`, `messages`, `unattributed`.
  Context values never participate in the spend totals.

An empty series means no recorded data in this range, not proof of zero usage.

## Focused verification

- `cargo test -p aionui-db --test usage_accounting`: deduplication, ownership,
  intersecting filters, snapshot vs. accumulated semantics, descendants and
  global non-duplication, cache subsets, deletion and invalid range/counter bounds.
- `cargo test -p aionui-ai-agent --lib backend_output_sink`: metadata forwarding.
- In AionRS: `cargo test -p aion-agent --lib usage_reports_are_per_request`.
- In UI: `vitest run tests/unit/usage`: grouping, empty/error states and charts.
- Disposable native-provider/SQLite/API acceptance and browser evidence live under
  root `.data/acceptance/usage-accounting-2026-10-19`. Provider answers in that test
  are scripted: this tests the transport/accounting pipeline without paid inference.

## Adapter evidence and routing boundaries

- Direct Claude: `aionui-session/tests/fixtures/claude_2.1.176_workflow_multiagent_3parallel_1fail.ndjson`.
- Direct Codex normalization: `aionui-session/src/backend/codex_conn.rs` maps
  authoritative thread `total` separately from `last` and regression-checks both.
- ACP: `agent-client-protocol-schema-1.5.0/src/v1/agent.rs`, `Usage` documents
  counters across all session turns; `McpCapabilities` makes stdio the baseline.
- Root/child routing uses the existing runtime factory and host MCP bridge.
  Generic ACP must support inline stdio MCP and acknowledge the frozen model.
  Unsupported transport/model selection fails explicitly, without fallback.
- Stateless classifier execution additionally requires a proven generation-only,
  no-tools adapter. Currently native API and direct Claude/Codex provide that
  contract. ACP's baseline protocol supplies no universal no-tools mode; those
  classifier entries remain visible with an unsupported reason. Catalog presence
  alone never proves that an installed third-party CLI implements its protocol.
- Cross-adapter acceptance uses a disposable real backend, factory, MCP stdio,
  SQLite and scripted ACP/HTTP peers. It proves host transport/accounting behavior,
  not model quality or compatibility with every upstream CLI release.

## Request size diagnostics

Native snapshots include `request_metrics`: content-free integer character counts for system sections (including AGENTS.md, memory and skill catalog), user/assistant text, thinking, tool results, tool-call arguments, advertised tool definition text/schema sizes (excluding unloaded catalog schemas), message/tool/image counts and explicitly named local token estimates. Captured before response/history mutation. These describe the provider-neutral request before transport formatting; they are not exact model tokenization or billing. CLI/ACP snapshots leave this map empty when the harness cannot inspect their internal prompts. No prompt bodies are retained. Historical requests cannot be reconstructed from this new ledger.

Per-chat UI requests exact-chat all-time totals and shows tiles and a usage donut, without global filters or aggregation tables. Latest context is a separate snapshot, not lifetime spend. Settings reuses the standard settings header/background and retains global filters and breakdown.


### On-demand MCP schemas in external adapters

Desktop and Web CLI package the same MCP discovery sidecar. Direct CLI,
Antigravity and generic ACP factories wrap each selected integration after
normal launch resolution. The sidecar preserves tool names/annotations and
upstream headers and the real input schemas. Verbose tool documentation is
loaded through `ellmsha_search_tools` (several keywords or exact tool names),
with refreshed documentation announced through `tools/list_changed`. Clients
that freeze signatures can still call tools correctly before discovery. Empty
permissive argument stubs are not used: they caused real agents to guess types. Calls are checked against the full upstream input schema; normal
adapter approvals still apply to the original tool name. Existing grants may
require renewed approval when their launcher fingerprint changes.

The proxy preserves resources/prompts and client callback capabilities through
the SDK protocol, bounds catalog pagination, and invalidates changed schemas.
Server instructions are integration documentation returned on discovery, not
system authority. Native aionrs uses its own ToolSearch index and does not need
the sidecar. Raw standalone Core deployments without packaged JS resources keep
their original external MCP transports; packaged desktop/Web CLI configure the
sidecar explicitly. An ACP vendor disabling baseline stdio cannot use this
stdio discovery bridge.


### Preserve provider agent instructions

Memory, skills and integration guidance augment the native agent policy. Normal
Codex sessions use `developerInstructions`; normal Claude sessions use
`--append-system-prompt`. Bounded generation/classification sessions intentionally
replace the base instruction and disable agent tools.

Codex 0.162 retains the rollout base and does not apply changed developer guidance
on resume/fork. Those operations restore the selected model's exact native
`model_messages.instructions_template` from its provider-authored model catalog,
then append current host guidance. Missing or mismatched model instructions fail
setup with a specific diagnostic rather than silently retaining a damaged base.
No provider prompt or user transcript is written to logs.
