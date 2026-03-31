# Stability Hardening Design

**Date:** 2026-03-31
**Status:** Draft
**Risk tier:** High (touches agent loop, tools, providers, gateway)

## Goal

Make the ZeroClaw agent runtime bulletproof by closing the five biggest stability gaps, following patterns proven in production by Claude Code.

## Approach

Hybrid — matches Claude Code's structure:

- **Extracted modules** for complex subsystems (microcompaction, tool executor)
- **Inline additions** for concerns inseparable from their host (file edit safety, LLM recovery, gateway errors)
- **No new top-level `src/stability/` module** — each fix lives where the concern lives

## 1. Microcompaction — `src/agent/microcompactor.rs`

**Pattern source:** Claude Code `src/services/compact/` (11 files, dedicated service)

### What it does

Surgically replaces old tool result content with short placeholders before each LLM call. Runs every turn (no LLM call needed — pure string manipulation). Acts as a first-pass filter before the existing `ContextCompressor` which handles full summarization.

### Design

```rust
pub struct MicrocompactionConfig {
    pub enabled: bool,                // default: true
    pub protect_recent_turns: usize,  // default: 6 — skip last N tool results
    pub max_result_chars: usize,      // default: 500 — threshold to trigger clearing
    pub preview_chars: usize,         // default: 200 — chars to keep as preview
}

pub struct MicrocompactionResult {
    pub cleared_count: usize,
    pub chars_reclaimed: usize,
}

pub fn microcompact(
    messages: &mut Vec<ChatMessage>,
    config: &MicrocompactionConfig,
) -> MicrocompactionResult
```

### Rules

- Only targets messages with role `"tool"` older than `protect_recent_turns`
- Replaces content exceeding `max_result_chars` with: `[Tool result cleared — {preview}...]`
- First `preview_chars` preserved inline for context
- No LLM call — runs in microseconds
- Idempotent — already-cleared messages are skipped (detected by `[Tool result cleared` prefix)

### Integration

Called in `loop_.rs` before `compress_if_needed()`:

```
microcompact(&mut history, &config.microcompaction) -> log result
compress_if_needed(&mut history, provider, model) -> existing flow
```

### Config addition (schema.rs)

New `microcompaction` field in the agent/runtime config section, with serde defaults.

## 2. Tool Executor with Retry — `src/agent/tool_executor.rs`

**Pattern source:** Claude Code `StreamingToolExecutor` (dedicated class wrapping tool execution)

### What it does

Wraps tool execution with per-tool retry policies, timeouts, and circuit breakers. Replaces the current inline tool execution in `loop_.rs`.

### Design

```rust
pub struct ToolRetryPolicy {
    pub max_retries: u32,         // default: 0 (no retry) — tools opt in
    pub backoff_base_ms: u64,     // default: 500
    pub backoff_max_ms: u64,      // default: 5000
    pub retryable: fn(&ToolResult) -> bool,  // determines if failure is retryable
}

pub struct ToolExecutor {
    tools: Vec<Arc<dyn Tool>>,
    default_timeout: Duration,     // default: 60s
    policies: HashMap<String, ToolRetryPolicy>,  // per-tool overrides
}

impl ToolExecutor {
    pub async fn execute(
        &self,
        tool_name: &str,
        args: Value,
    ) -> ToolResult

    pub async fn execute_batch(
        &self,
        calls: Vec<(String, Value)>,
    ) -> Vec<ToolResult>  // parallel execution
}
```

### Retry classification

Retryable failures (tool result `success: false`):
- Timeout errors
- Rate limit / transient network errors
- "File not found" where a race condition is plausible

Non-retryable:
- Security policy violations
- Parameter validation errors
- Explicit permission denials

### Default policies

| Tool | max_retries | Notes |
|------|-------------|-------|
| `shell` | 0 | Side effects — never auto-retry |
| `file_read` | 1 | Transient FS errors |
| `file_edit` | 0 | Side effects — never auto-retry |
| `file_write` | 0 | Side effects — never auto-retry |
| `web_fetch` | 2 | Network transience |
| `content_search` | 1 | Transient FS errors |
| All others | 0 | Conservative default |

### Integration

Replace direct `tool.execute(args)` calls in `loop_.rs` with `tool_executor.execute(name, args)`. The executor is constructed once per agent session from config + tool registry.

## 3. File Edit Safety — inline in `src/tools/file_edit.rs`

**Pattern source:** Claude Code `EditTool` — staleness detection + atomic writes + content-hash backups, all inline in the tool implementation.

### What it does

Three additions to the existing `FileEditTool::execute()`:

#### 3a. Staleness detection

After reading the file, capture `(content_hash, mtime)`. Before writing, re-stat the file. If mtime changed, re-read and re-check the hash. If content changed, abort with a clear error.

```rust
// After read
let pre_hash = blake3::hash(content.as_bytes());
let pre_mtime = tokio::fs::metadata(&resolved_target).await?.modified()?;

// ... match & replace logic ...

// Before write — staleness check
let post_mtime = tokio::fs::metadata(&resolved_target).await?.modified()?;
if post_mtime != pre_mtime {
    let current = tokio::fs::read_to_string(&resolved_target).await?;
    if blake3::hash(current.as_bytes()) != pre_hash {
        return Ok(ToolResult {
            success: false,
            error: Some("File was modified by another process since read — aborting edit".into()),
            ..
        });
    }
}
```

#### 3b. Atomic write via temp + rename

Write to a temp file in the same directory, then `rename()`. This ensures the file is never half-written.

```rust
let tmp_path = resolved_target.with_extension("zeroclaw-tmp");
tokio::fs::write(&tmp_path, &new_content).await?;
tokio::fs::rename(&tmp_path, &resolved_target).await?;
```

#### 3c. Content-hash backup

Before overwriting, copy the original to a backup keyed by content hash. Allows recovery if an edit goes wrong.

```rust
let backup_dir = workspace_dir.join(".zeroclaw/backups");
let backup_name = format!("{}.{}", file_name, &hex_hash[..12]);
tokio::fs::copy(&resolved_target, backup_dir.join(&backup_name)).await?;
```

Backups are best-effort — failure to create a backup does not block the edit. Old backups cleaned up by age (>24h) on a background task or at agent start.

## 4. LLM Error Recovery — inline in `src/agent/loop_.rs`

**Pattern source:** Claude Code `src/query.ts` — multi-stage recovery coded directly in the query loop, calling out to helper functions.

### What it does

Adds staged recovery for the two most common LLM failures, directly in the tool-call loop.

### 4a. Prompt-too-long (413 / context_length_exceeded) recovery

Current behavior: `compress_on_error()` adjusts window and re-compresses.
New behavior — three stages:

```
Stage 1: microcompact (clear old tool results) → retry
Stage 2: compress_on_error (full summarization) → retry  [existing]
Stage 3: emergency truncation (drop oldest half of non-system messages) → retry
Stage 4: surface error to user (give up)
```

Each stage only runs if the previous one failed to bring tokens under the limit.

### 4b. Max output tokens recovery

Current behavior: none — truncated response is returned as-is.
New behavior:

```
Stage 1: inject a continuation prompt ("Continue from where you left off") → retry (up to 2 times)
Stage 2: accept the truncated response
```

The continuation prompt is appended as a user message with the truncated assistant content preserved, so the model has context to continue.

### 4c. Model fallback notification

Current behavior: `ReliableProvider` records fallback via task-local, channel code can read it.
Addition: When a fallback occurs, inject a brief system note into the conversation so the model knows it's running on a different model (may need to adjust behavior).

### Integration

These are `if/match` blocks added to the existing error handling in the tool-call loop, with helper functions extracted for testability:

```rust
// In loop_.rs, near the provider call error handling:
fn recover_prompt_too_long(...) -> Result<RecoveryAction>
fn recover_max_output_tokens(...) -> Result<RecoveryAction>

enum RecoveryAction {
    Retry,
    RetryWithMessages(Vec<ChatMessage>),
    GiveUp(String),
}
```

## 5. Gateway Error Mapping — inline in `src/gateway/api.rs`

**Pattern source:** Standard HTTP API practice (Claude Code is CLI-only, no gateway).

### What it does

Replace the blanket `StatusCode::INTERNAL_SERVER_ERROR` with proper HTTP status codes and retry hints.

### Mapping

| Error condition | Status code | Retry-After header |
|----------------|-------------|-------------------|
| Bad request / validation | 400 | — |
| Auth failure | 401 | — (existing) |
| Rate limited | 429 | `60` |
| Agent busy (processing) | 409 | `5` |
| Memory/cron/config not found | 404 | — |
| Provider error (upstream) | 502 | `10` |
| Internal error (catch-all) | 500 | — |

### Implementation

A helper function that classifies `anyhow::Error` into the right status:

```rust
fn error_to_response(err: anyhow::Error) -> (StatusCode, Json<Value>) {
    // Classify by error type/message → return appropriate status + JSON body
}
```

Applied to each handler's `.map_err()` chain. Each JSON error body includes `{"error": "...", "retryable": bool}`.

## Files changed

| File | Change type | Risk |
|------|------------|------|
| `src/agent/microcompactor.rs` | **New file** | Medium |
| `src/agent/tool_executor.rs` | **New file** | Medium |
| `src/agent/mod.rs` | Add module exports | Low |
| `src/agent/loop_.rs` | Wire microcompactor, tool_executor, recovery helpers | High |
| `src/tools/file_edit.rs` | Add staleness + atomic write + backup | Medium |
| `src/gateway/api.rs` | Error classification helper | Low |
| `src/config/schema.rs` | Add `MicrocompactionConfig` | Low |
| `Cargo.toml` | Add `blake3` dependency (for content hashing) | Low |

## Dependencies

- `blake3` — fast content hashing for file staleness detection. ~Zero overhead, pure Rust, no heavy deps. Already widely used in the ecosystem.

## Testing strategy

- `microcompactor.rs` — unit tests with synthetic message histories
- `tool_executor.rs` — unit tests with mock tools, retry/timeout scenarios
- `file_edit.rs` — integration tests: concurrent modification, atomic write verification, backup creation
- `loop_.rs` recovery — integration tests with mock providers that return 413/max-output errors
- `gateway/api.rs` — unit tests verifying status code mapping

## Out of scope

- Streaming parallel tool execution during model streaming (future enhancement)
- Cross-turn prefetching of memory (future enhancement)
- Prompt cache stability / tool ordering (future enhancement)
- Per-tool sandbox isolation (future enhancement)
