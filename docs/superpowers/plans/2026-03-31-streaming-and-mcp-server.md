# Tool Streaming & MCP Server Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add real-time tool progress streaming and an MCP server mode following Claude Code's exact patterns.

**Architecture:** Tool streaming extends the existing `DraftEvent` enum with a `ToolProgress` variant, emitted from `execute_one_tool()`. MCP server is a new `src/mcp_server/` module with a `zeroclaw mcp` subcommand supporting stdio and HTTP+SSE transports.

**Tech Stack:** Rust, tokio, serde_json, axum (HTTP transport reuses existing gateway)

---

## ⚠️ CRITICAL: WhatsApp Flow Protection

Same rules as stability hardening — no signature changes, all additions are additive, WhatsApp handlers untouched.

---

## File Structure

| File | Responsibility | New/Modified |
|------|---------------|-------------|
| `src/agent/loop_.rs` | `ToolPhase` enum, `ToolProgress` variant, emit events | Modified |
| `src/mcp_server/mod.rs` | MCP server orchestration, transport selection | **New** |
| `src/mcp_server/handlers.rs` | JSON-RPC request handlers (tools/list, tools/call, etc.) | **New** |
| `src/mcp_server/transport.rs` | Stdio + HTTP+SSE transport implementations | **New** |
| `src/main.rs` | `mcp` subcommand | Modified |
| `src/lib.rs` | `pub mod mcp_server` export | Modified |

---

### Task 1: Add ToolPhase and ToolProgress to DraftEvent

**Files:**
- Modify: `src/agent/loop_.rs`

- [ ] **Step 1: Add ToolPhase enum and extend DraftEvent**

In `src/agent/loop_.rs`, after the `DraftEvent` enum (around line 367), add `ToolPhase` and extend `DraftEvent`:

```rust
/// Phase of tool execution for streaming progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolPhase {
    Started,
    Running,
    Completed,
    Failed,
}

pub enum DraftEvent {
    Clear,
    Progress(String),
    Content(String),
    /// Structured tool execution progress — emitted in real-time during tool runs.
    ToolProgress {
        tool_name: String,
        tool_id: String,
        phase: ToolPhase,
        detail: Option<String>,
    },
}
```

- [ ] **Step 2: Emit ToolProgress in execute_one_tool**

In `execute_one_tool()`, emit `Started` before the tool executes and `Completed`/`Failed` after. Find where the tool result is obtained and wrap it:

```rust
// Before tool.execute()
if let Some(ref tx) = on_delta {
    let _ = tx.send(DraftEvent::ToolProgress {
        tool_name: tool_name.to_string(),
        tool_id: call_id.to_string(),
        phase: ToolPhase::Started,
        detail: None,
    }).await;
}

// After tool.execute() succeeds
if let Some(ref tx) = on_delta {
    let _ = tx.send(DraftEvent::ToolProgress {
        tool_name: tool_name.to_string(),
        tool_id: call_id.to_string(),
        phase: if result.success { ToolPhase::Completed } else { ToolPhase::Failed },
        detail: Some(truncate_for_progress(&result.output, 120)),
    }).await;
}
```

- [ ] **Step 3: Emit ToolProgress in execute_tools_parallel**

Same pattern in `execute_tools_parallel()` — emit per-tool progress as each tool starts and completes within the parallel batch.

- [ ] **Step 4: Verify compilation and tests**

Run: `cargo check && cargo +stable test --lib 2>&1 | tail -20`

- [ ] **Step 5: Commit**

```bash
git add src/agent/loop_.rs
git commit -m "feat(agent): add ToolProgress streaming events during tool execution

Extends DraftEvent with ToolProgress variant and ToolPhase enum.
Events emitted at tool start/complete/fail in both sequential and
parallel execution paths. Existing consumers (WhatsApp, gateway)
ignore the new variant — no behavior change for them."
```

---

### Task 2: Create MCP server module — types and protocol

**Files:**
- Create: `src/mcp_server/mod.rs`

- [ ] **Step 1: Create the mcp_server module directory and mod.rs**

```rust
//! MCP (Model Context Protocol) server for ZeroClaw.
//!
//! Exposes ZeroClaw's tools over MCP protocol, following Claude Code's
//! `mcp` subcommand pattern. Supports stdio and HTTP+SSE transports.

pub mod handlers;
pub mod transport;

use crate::tools::mcp_protocol::{JsonRpcRequest, JsonRpcResponse};

/// MCP server capabilities declaration.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServerCapabilities {
    pub tools: serde_json::Value,
    pub resources: serde_json::Value,
    pub prompts: serde_json::Value,
}

impl Default for ServerCapabilities {
    fn default() -> Self {
        Self {
            tools: serde_json::json!({}),
            resources: serde_json::json!({}),
            prompts: serde_json::json!({}),
        }
    }
}

/// MCP server info sent during initialization.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// MCP server configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub transport: TransportMode,
    pub port: u16,
    pub api_key: Option<String>,
    pub debug: bool,
    pub workspace_dir: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub enum TransportMode {
    Stdio,
    Http,
}

/// Start the MCP server with the given config.
pub async fn serve(config: McpServerConfig) -> anyhow::Result<()> {
    let info = ServerInfo {
        name: "zeroclaw".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let capabilities = ServerCapabilities::default();

    tracing::info!(
        transport = ?config.transport,
        "Starting MCP server: {} v{}",
        info.name,
        info.version,
    );

    match config.transport {
        TransportMode::Stdio => transport::serve_stdio(info, capabilities, &config).await,
        TransportMode::Http => transport::serve_http(info, capabilities, &config).await,
    }
}
```

- [ ] **Step 2: Verify it compiles (module stubs)**

Create stub files for `handlers.rs` and `transport.rs` so it compiles.

- [ ] **Step 3: Commit**

```bash
git add src/mcp_server/
git commit -m "feat(mcp-server): add MCP server module skeleton with types"
```

---

### Task 3: MCP server handlers — tools/list, tools/call

**Files:**
- Create: `src/mcp_server/handlers.rs`

- [ ] **Step 1: Implement JSON-RPC request router and tool handlers**

```rust
use crate::tools::mcp_protocol::{JsonRpcRequest, JsonRpcResponse};
use serde_json::{json, Value};

/// Route a JSON-RPC request to the appropriate handler.
pub async fn handle_request(
    req: &JsonRpcRequest,
    tool_registry: &[ToolDef],
) -> JsonRpcResponse {
    match req.method.as_str() {
        "initialize" => handle_initialize(req),
        "tools/list" => handle_tools_list(req, tool_registry),
        "tools/call" => handle_tools_call(req, tool_registry).await,
        "resources/list" => handle_resources_list(req),
        "resources/read" => handle_resources_read(req),
        "prompts/list" => handle_prompts_list(req),
        "prompts/get" => handle_prompts_get(req),
        "notifications/initialized" => handle_notification(req),
        _ => method_not_found(req),
    }
}
```

Implement each handler following Claude Code's pattern:
- `initialize` → return server info + capabilities
- `tools/list` → enumerate tool registry with JSON schemas
- `tools/call` → execute tool, return MCP content blocks
- `resources/list` → workspace info, config, memory stats
- `prompts/list` → available prompt templates

- [ ] **Step 2: Add tests for handlers**

Unit tests with mock tool registry verifying JSON-RPC responses.

- [ ] **Step 3: Commit**

```bash
git add src/mcp_server/handlers.rs
git commit -m "feat(mcp-server): implement JSON-RPC request handlers for tools/resources/prompts"
```

---

### Task 4: MCP server transport — stdio

**Files:**
- Create: `src/mcp_server/transport.rs`

- [ ] **Step 1: Implement stdio transport**

Read JSON-RPC lines from stdin, dispatch to handlers, write responses to stdout.

```rust
pub async fn serve_stdio(
    info: ServerInfo,
    capabilities: ServerCapabilities,
    config: &McpServerConfig,
) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Some(line) = lines.next_line().await? {
        let req: JsonRpcRequest = serde_json::from_str(&line)?;
        let resp = handlers::handle_request(&req, &tool_registry).await;
        if req.id.is_some() {  // not a notification
            let out = serde_json::to_string(&resp)? + "\n";
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Implement HTTP+SSE transport stub**

Basic axum server with POST /mcp, GET /mcp (SSE), GET /health endpoints.

- [ ] **Step 3: Commit**

```bash
git add src/mcp_server/transport.rs
git commit -m "feat(mcp-server): implement stdio and HTTP+SSE transport layer"
```

---

### Task 5: Wire MCP server into main.rs subcommand

**Files:**
- Modify: `src/main.rs`
- Modify: `src/lib.rs`

- [ ] **Step 1: Add `pub mod mcp_server` to lib.rs**

- [ ] **Step 2: Add `mcp` subcommand to CLI**

In `src/main.rs`, add to the `Commands` enum:

```rust
/// Start MCP server (Model Context Protocol)
Mcp {
    /// Use HTTP+SSE transport instead of stdio
    #[arg(long)]
    http: bool,
    /// Port for HTTP transport (default: 3000)
    #[arg(long, default_value = "3000")]
    port: u16,
    /// Enable debug logging
    #[arg(long)]
    debug: bool,
},
```

And in the match arm:

```rust
Commands::Mcp { http, port, debug } => {
    let config = mcp_server::McpServerConfig {
        transport: if http { mcp_server::TransportMode::Http } else { mcp_server::TransportMode::Stdio },
        port,
        api_key: std::env::var("MCP_API_KEY").ok(),
        debug,
        workspace_dir: config_dir.clone(),
    };
    mcp_server::serve(config).await?;
}
```

- [ ] **Step 3: Verify compilation and test**

Run: `cargo check && cargo +stable test --lib 2>&1 | tail -20`

- [ ] **Step 4: Commit**

```bash
git add src/main.rs src/lib.rs
git commit -m "feat(cli): add 'zeroclaw mcp' subcommand for MCP server mode

Supports --http flag for HTTP+SSE transport (default: stdio).
Optional auth via MCP_API_KEY env var. Follows Claude Code's
mcp subcommand pattern."
```

---

### Task 6: Full validation

- [ ] **Step 1: cargo fmt**
- [ ] **Step 2: cargo clippy -- -D warnings**
- [ ] **Step 3: cargo test --lib**
- [ ] **Step 4: Verify `zeroclaw mcp --help` works**
- [ ] **Step 5: Commit any fixes**
