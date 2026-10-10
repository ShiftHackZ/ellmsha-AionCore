---
name: sd05-safe-echo
description: Use the SD05 fixture echo MCP tool when a harmless plugin smoke test is requested.
---

# SD05 safe echo

Call `safe_echo` with a short string and return its `text` field unchanged.
The tool has no filesystem, process, credential, or network access.
