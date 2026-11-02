# Shared interactive terminals

The desktop and WebUI use one conversation-owned PTY service, compiled into Core with portable-pty. The bundled `ellmsha-background-processes` MCP exposes this same service to native, ACP and hybrid sessions through the shared session factory. A user-created terminal and an agent-created terminal have the same ID, shell, cwd, screen, output and input revision. No provider-specific terminal daemon is required.

## Deployment and lifetime

New sessions start in the conversation's canonical workspace: a project root or the assigned temporary-chat directory. An optional cwd must resolve inside that workspace, including symlink checks. Desktop uses its host shell; Docker uses its container shell and mounted workspace. Session creation never opens a host terminal from WebUI. Shells and commands must be installed in that environment; Docker includes procps/watch and ncurses terminfo.

An empty command starts an interactive shell (the user's existing absolute `$SHELL` on Unix, falling back to `/bin/sh`; `cmd.exe /D /Q` on Windows). A nonempty command starts one foreground command. Unix uses a controlling PTY; Windows uses ConPTY with a Job Object assigned before accepting input. Input/output are never mirrored to host logs. Ctrl+C interrupts the foreground program; Stop ends the shell and its owned children. Closing a UI tab performs Stop before removal. Hiding/unmounting the view only detaches it. Normal shutdown and desktop-parent death close owned sessions. Explicit daemonization/detachment is unsupported and agents are instructed not to use it.

At most 64 sessions are retained; exited sessions may be pruned on admission. Output uses a 256 KiB byte ring and a VT screen with 2000 lines of scrollback. Bytes are base64 encoded on the renderer transport to preserve split UTF-8/control sequences. The renderer parses them with xterm.js. Backend screen parsing works with no renderer connected. State is in memory and does not survive Core restart. The initial cwd in metadata does not track later `cd`; the actual shared shell does.

## Authentication

Every route uses CurrentUser, global auth and CSRF protection for mutations. Runtime-token requests must have the host-validated user/conversation identity and match the route's conversation. Request bodies cannot change the owner, endpoint, account, actor or OS PID. All session operations check both conversation ownership and entry ownership; an opaque terminal ID is not authority across chats. Removed workspaces do not prevent Stop/Read. Terminal access inherits the server user's filesystem permissions; cwd validation is not a sandbox.

## HTTP contract

Base: `/api/conversations/{conversation}/background-processes`. All successful responses use `ApiResponse<T>` with `success: true, data: T`. Fields and validation are defined in `aionui-api-types::background_process`.

| Method / suffix | Request | Response |
| --- | --- | --- |
| POST base | `command?: string` (0–16384 bytes, no NUL), `title?: string` (120 characters, no controls), `cwd?: string` | Session snapshot |
| GET base | None | Session summaries without screen text |
| GET `/{id}` | None | Current rendered screen and status |
| POST `/{id}/input` | `data: string` (1–65536 UTF-8 bytes, no implicit Enter), `expected_revision?: u64` | Current snapshot after accepting input |
| POST `/{id}/resize` | `rows: u16` (2–300), `cols: u16` (2–500) | Current snapshot |
| GET `/{id}/output` | Query `after?: u64`, `screen_revision?: u64` | Incremental output |
| POST `/{id}/wait` | `text: string` (literal, at most 4096 bytes), `timeout_ms: u64` (0–10000) | Snapshot on match, exit or timeout |
| POST `/{id}/close` | `{}` | Stops the session and removes its shared UI tabs; retains final snapshot |
| POST `/{id}/stop` | `{}` | Final snapshot; repeated calls are safe |

Snapshots contain `id`, `conversation_id`, `title`, original `command`, initial canonical `cwd`, display-only `pid`, `running`, `closed`, nullable `exit_code`, `stdout` (current plain-text rendered screen), `stderr` (empty because PTYs combine streams), `stdout_truncated`, `input_revision`, `human_input_active`, `rows`, `cols`, and zero-based `[row,column]` `cursor`.

Output contains base64 VT `data`, absolute byte `next_offset`, `reset`, `running`, `input_revision`, `rows`, `cols`, and `screen_revision`. Missing/stale offsets or resize revisions cause a full screen resynchronization. When `reset` is true, resize/reset the emulator before applying data. Apply output before advancing either revision. A full resync restores the current screen; historical client scrollback is not reconstructed. Do not strip ANSI and re-render the entire transcript on every read.

The actor is derived from authenticated transport: runtime token means agent. Agent input requires `expected_revision` equal to the latest snapshot and is refused during the two-second human typing lease. Every input attempt accepted for writing reserves a revision before I/O, including partial/uncertain writes. Human keyboard input is serialized. Lost/uncertain writes must not be blindly retried; reconnect and inspect the screen. A successful input request means accepted input, not completed command. Silence is never completion. A wait response must be inspected for its requested text/running state.

Invalid input/cwd/title/dimensions/revision/lease/shutdown returns 400, missing auth 401, CSRF or runtime-conversation mismatch 403, unowned/missing conversation/session 404, OS/transport failures 500. Errors use the standard fixed API error envelope without raw command output/credentials. Input conflict messages tell callers to read again.

## Agent tools

`ellmsha_terminal_create`, `list`, `read`, `write`, `execute`, `send_key`, `resize`, `wait`, `close` operate on these same sessions. `execute` appends Enter to input; agents must verify a shell prompt first or create a fresh session. `send_key` supports Enter, Tab, Escape, Backspace, CtrlC/D/Z, arrows, Home/End and PageUp/Down. Input tools require `expected_revision`; ownership and approvals remain enforced. The four legacy `ellmsha_background_start/list/read_stdout/stop` tools remain compatible and address the same inventory. There is no heuristic shell-command rewriting.

## Build contract

The shared production Vite configuration bundles xterm.js and Fit, and emits `terminal-contract.json` with actual JS/CSS artifacts and `terminal-licenses.txt`. Desktop afterPack verifies every referenced asset on every OS. Docker/WebCLI use the same renderer build, bundled MCP entry and Core sources; portable-pty is a mandatory locked dependency, not an optional Cargo feature. The root Shared terminal CI job runs real PTY tests on Linux, macOS and Windows. Cross-platform results require those jobs to actually run; a local macOS pass alone is not Windows verification.
