# Tier 2 & 3 Improvements Implementation Plan

**Goal:** Implement 8 remaining Claude Code-inspired improvements: dynamic model routing, tool dependency graph, tool result caching, MCP circuit breakers, slow query alerting, tool input validation, backup cleanup, and shell output streaming.

**Architecture:** Each improvement is isolated — inline additions to existing modules. No new top-level modules.

---

## ⚠️ CRITICAL: WhatsApp Flow Protection

- No function signature changes to `process_message()`, `run_tool_call_loop()`, `run_gateway_chat_with_tools()`
- All changes additive
- WhatsApp gateway handlers in `mod.rs` NOT modified

---

## Tasks

### Task 1: Dynamic model routing by complexity
- `src/agent/classifier.rs` — add token estimation heuristic
- `src/providers/router.rs` — add complexity-based route selection

### Task 2: Tool dependency graph
- `src/agent/loop_.rs` — detect tool dependencies in parallel batches

### Task 3: Tool result caching
- `src/agent/loop_.rs` — cache identical tool calls within a session

### Task 4: MCP circuit breakers
- `src/tools/mcp_client.rs` — track health, back off on failures

### Task 5: Slow query alerting
- `src/agent/loop_.rs` — warn on slow LLM calls

### Task 6: Tool input validation
- `src/agent/loop_.rs` — validate args against schema before execute

### Task 7: Backup cleanup
- `src/tools/file_edit.rs` — prune old backups on startup

### Task 8: Shell output streaming
- `src/tools/shell.rs` — stream stdout lines as ToolProgress events
