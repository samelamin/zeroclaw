# Tool Streaming & MCP Server Design

**Date:** 2026-03-31
**Status:** Draft
**Risk tier:** Medium (additive features, no existing signatures change)

## Goal

Add two high-impact features following Claude Code's exact patterns:
1. Streaming tool progress during execution (not just after completion)
2. MCP server mode exposing ZeroClaw's tools over the Model Context Protocol

## ⚠️ WhatsApp Flow Protection

Same rules as stability hardening:
- No function signature changes to `process_message()`, `run_tool_call_loop()`, `run_gateway_chat_with_tools()`
- All changes additive — existing consumers ignore new event variants
- WhatsApp gateway handlers in `mod.rs` are NOT modified

---

## 1. Tool Result Streaming — `src/agent/loop_.rs` + tool files

**Pattern source:** Claude Code `StreamingToolExecutor` — tracks tool state, yields progress immediately, buffers results in order.

### What it does

Emits `DraftEvent::ToolProgress` events during tool execution so clients see real-time status instead of waiting for completion.

### Design

Add a new variant to the existing `DraftEvent` enum:

```rust
pub enum DraftEvent {
    Clear,
    Progress(String),
    Content(String),
    // NEW: Structured tool progress
    ToolProgress {
        tool_name: String,
        tool_id: String,
        phase: ToolPhase,
        detail: Option<String>,
    },
}

pub enum ToolPhase {
    Started,
    Running,
    Completed,
    Failed,
}
```

### Integration points

In `execute_one_tool()` (line ~2603):
- Emit `ToolProgress { phase: Started }` before `tool.execute()`
- Emit `ToolProgress { phase: Completed/Failed }` after
- For shell tool: forward stdout/stderr lines as `ToolProgress { phase: Running, detail: line }`

In `execute_tools_parallel()` (line ~2807):
- Same pattern per tool in the parallel batch

### Shell streaming

The shell tool already captures output. Add an optional `progress_tx: Option<Sender<String>>` to the shell tool's execute path that forwards lines as they arrive. The agent loop wraps these into `ToolProgress` events.

### Consumer impact

- Gateway SSE/WebSocket: can render tool progress in real-time
- WhatsApp: ignores `ToolProgress` events (same as it ignores `Progress`) — **safe**
- CLI: can show spinner/status per tool

---

## 2. MCP Server Mode — `src/mcp_server/` + `src/main.rs`

**Pattern source:** Claude Code `mcp-server/` — dedicated MCP server exposing tools over stdio and HTTP+SSE transports.

### What it does

Adds a `zeroclaw mcp` subcommand that starts an MCP server, exposing ZeroClaw's tools to any MCP client (Claude Desktop, Cursor, VS Code, etc.).

### Architecture

Following Claude Code exactly:
- New module `src/mcp_server/` with server logic
- New subcommand `mcp` in `src/main.rs`
- Two transport modes: stdio (default, for local integration) and HTTP+SSE (for remote)
- Exposes ZeroClaw's tool registry over MCP protocol

### Transport

**Stdio (default):** Parent process communicates via stdin/stdout JSON-RPC 2.0. Used by Claude Desktop, IDE extensions.

**HTTP+SSE (optional, `--http` flag):**
- POST /mcp — JSON-RPC requests
- GET /mcp — SSE stream
- DELETE /mcp — Session cleanup
- GET /health — Server health check
- Optional bearer token auth via `MCP_API_KEY` env var

### MCP capabilities

```rust
capabilities: {
    tools: {},      // ZeroClaw's tool registry
    resources: {},  // Configuration, memory, workspace info
    prompts: {},    // Reusable prompt templates
}
```

### Tools exposed

All tools from ZeroClaw's tool registry that are safe for external use:
- `shell_command` — Execute shell commands
- `file_read`, `file_write`, `file_edit` — File operations
- `content_search` — Search files
- `web_fetch` — Fetch URLs
- `memory_search` — Search agent memory
- MCP client tools (passthrough to connected MCP servers)

Dangerous tools (e.g., those requiring approval) are gated by the existing approval system or excluded.

### Server initialization flow

1. Parse CLI args (`zeroclaw mcp [--http] [--port PORT] [--debug]`)
2. Load workspace config
3. Initialize tool registry (same as agent mode)
4. Create MCP server with capabilities
5. Register request handlers (ListTools, CallTool, ListResources, ReadResource)
6. Connect transport (stdio or HTTP)
7. Await shutdown signal

### Protocol

- JSON-RPC 2.0
- MCP protocol version: 2024-11-05 (same as ZeroClaw's client)
- Reuse existing `src/tools/mcp_protocol.rs` types for JSON-RPC messages

### Request handlers

**tools/list:** Enumerate tool registry → MCP tool definitions with JSON schemas
**tools/call:** Route to tool.execute(), return result as MCP content blocks
**resources/list:** Expose workspace config, memory stats, agent status
**resources/read:** Read specific resource by URI
**prompts/list:** List available prompt templates (system prompts, personas)
**prompts/get:** Return prompt content with optional arguments

---

## Files changed

| File | Change type | Risk |
|------|------------|------|
| `src/agent/loop_.rs` | Add `ToolPhase` enum, `ToolProgress` variant, emit in execute functions | Low |
| `src/mcp_server/mod.rs` | **New** — MCP server module | Medium |
| `src/mcp_server/handlers.rs` | **New** — Request handlers for tools/resources/prompts | Medium |
| `src/mcp_server/transport.rs` | **New** — Stdio + HTTP transport layer | Medium |
| `src/main.rs` | Add `mcp` subcommand | Low |
| `Cargo.toml` | Add MCP server dependencies if needed | Low |

## Dependencies

- Existing: `serde_json`, `tokio`, `axum` (for HTTP transport)
- Reuse: `src/tools/mcp_protocol.rs` JSON-RPC types
- No new external crates needed — ZeroClaw already has everything

## Testing strategy

- Tool streaming: unit tests with mock `on_delta` channel, verify events emitted in order
- MCP server: integration tests sending JSON-RPC requests over in-memory transport
- MCP tools/list: verify all safe tools are enumerated with correct schemas
- MCP tools/call: verify tool execution returns proper MCP content blocks
