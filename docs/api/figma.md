# Figma plugin accounts and tools

Figma account operations use the normal authenticated `/api/` router and CSRF protection. Connections are scoped to `CurrentUser.id`; an opaque connection UUID does not grant access across users. API tokens are accepted only in request bodies and encrypted with the existing application credential key before persistence. Public responses contain no token, encrypted token or private process environment.

## HTTP contract

Base: `/api/plugins/figma/connections`.

| Method and path | Request | Success |
| --- | --- | --- |
| GET base | No body | 200 `ApiResponse<FigmaConnectionsResponse>` |
| POST base | `ConnectFigmaConnectionRequest` | 201 `ApiResponse<FigmaConnectionResponse>` |
| POST `/{connection_id}/connect` | Same connect request | 200 connection; replacement token must verify the same Figma user |
| POST `/{connection_id}/disconnect` | No fields required | 200 connection; encrypted token removed |

`ConnectFigmaConnectionRequest` rejects unknown fields. `personal_access_token` is a required string, 1–16384 ASCII graphic bytes, with no whitespace. `expected_user_id` is an optional eLLMsha user ID used by private browser import; if present it must match the authenticated user before any provider request. It cannot change the connection owner. Reconnect/disconnect path IDs must be valid UUIDs belonging to the current user. Reconnect never transfers an existing account connection to a different provider identity.

`FigmaConnectionsResponse.connections` is the user's array of public connections, empty when none exist. Each connection has:

| Field | Type and meaning |
| --- | --- |
| `id` | Opaque connection UUID |
| `namespace` | Stable account-specific MCP namespace |
| `label` | Verified provider handle and optional email |
| `status` | `connected`, `disconnected`, `reconnect_required`, or `error`; last local verification state |
| `identity` | Optional verified identity: `account_id` (stable provider ID), `account_name` (handle or ID fallback), optional `account_email` |
| `last_error` | Optional credential-free local failure description |
| `created_at`, `updated_at` | Unix timestamps in milliseconds |

Standard success envelopes are `{success:true,data:T}`. Errors use the normal API error envelope: 400 malformed/unknown fields, invalid token syntax, duplicate account, different identity on reconnect or the 25-account limit; 401 missing authentication; 403 invalid CSRF or browser owner mismatch; 404 invalid/unowned connection; 502 `FIGMA_AUTH_FAILED` when `/v1/me` cannot verify the account; 500 unavailable persistence, with internal details withheld. Provider bodies are never forwarded as authentication errors. Expired/failed reconnects remove the old runtime credential and require reconnecting. Restarts with an unavailable encryption key do not expose tools from undecryptable credentials.

Disconnect is local: it removes the saved token and changes runtime revision so new turns, resumed sessions and children receive the new account inventory. It does not revoke the PAT at Figma or cancel a previously authorized in-flight operation. Disabling the plugin withholds all its account servers. Existing project bindings do not select an account automatically.

## MCP contract

Core includes `mcp-figma-stdio` in every target. Each connected account receives its own process environment and approval identity. Private tokens are injected into the child environment, never command-line arguments or the public plugin catalog. Every tool rechecks `/v1/me` against the bound provider ID before reading or writing another resource. HTTP redirects are disabled and the API origin is fixed to `https://api.figma.com`; clients cannot supply arbitrary API URLs.

| Tool | Input and behavior |
| --- | --- |
| `figma_whoami` | No fields; current verified identity |
| `figma_get_file` | `file_key`, optional `depth` 1–10 (default 2) |
| `figma_get_nodes` | `file_key`, `node_ids` (1–50), optional depth |
| `figma_export_images` | `file_key`, 1–50 node IDs, optional `format` (`png`, `svg`, `jpg`, `pdf`, default png) and `scale` 0.01–4 (default 1); returns provider URLs, including null entries for failed renders |
| `figma_list_team_folders` | `team_id` from a user-supplied team URL; top-level folders only |
| `figma_list_subfolders` | `folder_id`; direct child folders only |
| `figma_list_folder_files` | `folder_id`; direct files only |
| `figma_list_comments` | `file_key`; visible file comments |
| `figma_add_comment` | `file_key`, exact authorized `message` (1–10000 characters), optional root `comment_id` for a reply; one external POST |

All MCP parameter objects reject unknown fields. Provider keys contain only ASCII letters, digits, `_` and `-`, at most 200 bytes. Node IDs additionally permit `:` and `;`. IDs are encoded as query values rather than concatenated into query strings. Responses wrap `connection_id`, verified `account` and the provider `data`. Raw provider data is bounded to 8 MiB; model-visible output is bounded to 1 MiB. Oversized responses fail explicitly, never masquerade as complete truncated JSON. The returned provider payload follows the referenced Figma endpoint's schema; tools do not invent scopes or flatten nested folder inventories into an account-wide list.

Errors explain 401/403 access or expiry, 404 unshared/missing resources, 429 limits with numeric Retry-After when present, transport failures and malformed/oversized responses without provider bodies or tokens. Comment writes retain normal tool approval and are not automatically retried. After a timeout, inspect comments before posting again. This REST integration does not implement arbitrary canvas writes or official MCP-only tools.

## Private desktop browser import

The bundled browser MCP adds `browser_figma_connect_account` with `tab_id`, `selector`, optional `connection_id`. Its scoped native bridge command is `Aionui.importFigmaAccount` with `targetId`, `selector`, optional `connectionId`. No credential value is accepted by these tools. The exact registered native tab must belong to the still-valid visible conversation profile and have a trusted Figma HTTPS origin (`figma.com` or `www.figma.com`). Foreign/closed/recreated targets and changed URLs fail closed.

One CSS selector (1–1000 characters) must match exactly one generated-token input or leaf text element. Password inputs and non-leaf containers are rejected. The native main process privately reads a 20–16384-byte ASCII graphic token, revalidates the scope and target, then calls the authenticated connect route with its application session, CSRF header and verified `expected_user_id`. Only whitelisted public connection metadata returns over CDP/MCP. Backend/provider error bodies and echoed credentials are withheld. A timeout is explicitly an unknown outcome: inspect the account list before retrying.

Token creation in Figma is a separate, user-authorized browser action guided by the plugin skill. Private import requires the desktop scoped browser. WebUI/Docker support the private Settings form and the same Core account tools, with API calls executed on the server.

## Build and provider references

Desktop local and CI builds use the same afterPack resource gate, requiring the Figma manifest and skill. WebCLI copies and checks those same plugin resources; Docker copies the plugin tree and verifies these files. Core tools are compiled into the same backend used by all deployments, with no npm downloads or platform-specific Figma executable.

- https://developers.figma.com/docs/rest-api/users-endpoints/
- https://developers.figma.com/docs/rest-api/users-types/
- https://developers.figma.com/docs/rest-api/personal-access-tokens/
- https://developers.figma.com/docs/rest-api/scopes/
- https://developers.figma.com/docs/rest-api/file-endpoints/
- https://developers.figma.com/docs/rest-api/folders-endpoints/
- https://developers.figma.com/docs/rest-api/comments-endpoints/
- https://developers.figma.com/docs/figma-mcp-server/remote-server-installation/
