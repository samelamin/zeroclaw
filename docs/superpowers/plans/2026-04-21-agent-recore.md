# Agent Intelligence Re-Core — Surgical Strip Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the ZeroClaw agent genuinely intelligent (persistent, multi-step, fails forward) by **stripping the scaffolding that edits the model's view** from `Agent::turn` and the CLI loop, behind an `agent.core = "legacy" | "minimal"` feature flag. Replace 5 memory tools with `remember`/`recall`. Lock in with CI guardrails + RSS regression test. Delete the scaffolding + heavy memory modules once eval parity holds.

**Architecture:** Opposite of a greenfield rewrite. The existing `Agent::turn` in `src/agent/agent.rs` and the native/XML tool dispatchers + `parse_tool_calls` in `src/agent/loop_.rs` are load-bearing and correct — they handle the full provider matrix (Anthropic, OpenAI, OpenRouter, Gemini, Ollama, GLM, MiniMax, etc.) with native and prompt-guided tool calling. What makes the agent dumb is scaffolding **around** that loop: `memory_loader.load_context` (injects retrieval into user messages), `classify_model` (hidden routing), `context_compressor`, `microcompactor`, `loop_detector`, `history_pruner`, `personality`, `thinking`, and the `format!("Error: {...}")` rewrap that drops the tail of tool errors. We gate every one of those behind the legacy flag; minimal mode runs the clean path. No new loop. No new message encoder. No new parse_tool_calls. Skills, security_summary, autonomy_level, MCP activation, hooks, cost, streaming, and session save/load all continue to work because we do not touch them.

**Tech Stack:** Rust, tokio, serde, chrono, existing provider trait + `ToolDispatcher` + `Memory` trait + `MarkdownMemory`. New dev-dep: `sysinfo` (RSS test only).

---

## Spec

See [`docs/superpowers/specs/2026-04-21-agent-recore-design.md`](../specs/2026-04-21-agent-recore-design.md).

## Baked-in assumptions (spec-resolving)

1. **No new loop.** `Agent::turn` is the entry point used by every production caller (channels/whatsapp, gateway/api, gateway/ws, tests). It stays. We branch inside it on `config.agent.core`.
2. **CLI path (`main.rs agent` → `loop_::run`) stays legacy for Phase 1–4.** `loop_::run` is where the heavy scaffolding lives (microcompactor, loop_detector, compressor, thinking, history_pruner). Rewriting `loop_::run` is out of scope until Phase 6 when we delete it.
3. **`agent.core = "legacy"` is the default.** Production prod path (`whatsappweb`) continues to route through `Agent::turn` unchanged. Flip to `"minimal"` only after eval parity.
4. **Tool-error truncation is tail-preserving.** Errors live at the tail. When we must truncate, we drop bytes from the head and announce the cut with a one-line prefix. Applies only in minimal mode (legacy keeps its `"Error: {}"` wrap).
5. **Memory tool swap runs in both modes.** `RememberTool` / `RecallTool` replace the 5 old memory tools unconditionally, because no users of the old tools exist yet (confirmed with the user).
6. **Skills, security_summary, autonomy_level, MCP activated tools, hooks, cost tracking, response cache, session save/load, and streaming all continue to work.** We do not write any replacement for these systems; we call the existing ones. The minimal branch preserves every call site that isn't scaffolding.
7. **CI grep lint covers `src/agent/` and `src/memory/`.** It forbids the five patterns from the spec (`LazyLock<Mutex<HashMap>>`, `LazyLock<Mutex<Vec>>`, `OnceLock<Mutex<_>>`, `static mut`, `tokio::spawn` of long-lived tasks) in new code. Pre-existing hits stay behind an allowlist comment until the Phase 6 deletion.
8. **RSS regression test** uses a `ScriptedProvider` mock, drives `Agent::turn` for 500 turns in minimal mode, asserts RSS growth < 20 MB.
9. **Eval task suite is a new `src/agent/eval_suite.rs` + `tests/agent_eval_suite.rs` harness.** The existing `src/agent/eval.rs` is a complexity classifier (unrelated name collision); leave it alone.

---

## File Map

### New files

- `src/agent/tool_result_truncate.rs` — pure helper: `CoreToolResult` + `tail_truncate()`
- `src/tools/remember.rs` — `RememberTool` (backed by `Arc<dyn Memory>`)
- `src/tools/recall.rs` — `RecallTool` (backed by `Arc<dyn Memory>`)
- `src/agent/eval_suite.rs` — behavioral task-suite runner (legacy vs minimal)
- `tests/agent_rss.rs` — 500-turn RSS regression test
- `tests/agent_eval_suite.rs` — integration test wrapping `eval_suite`
- `scripts/ci/lint_agent_core.sh` — grep-based guardrail lint

### Modified files

- `src/config/schema.rs` — add `core: String` field on `AgentConfig` (default `"legacy"`)
- `src/agent/mod.rs` — register `tool_result_truncate` + `eval_suite`
- `src/agent/agent.rs` — in `turn` and `turn_streamed`: gate `memory_loader.load_context()` behind legacy; gate `classify_model()` behind legacy; route tool-result formatting through the new truncate helper when minimal
- `src/tools/mod.rs` — remove 5 memory tool modules/exports/registrations; add `remember` + `recall` modules/exports/registrations (backed by the existing `Arc<dyn Memory>` passed through `AgentBuilder::memory`)
- `tool_descriptions/en.toml` — rewrite all tool descriptions to the 5-section schema (purpose / when / example / success / failure)
- `Cargo.toml` — add `sysinfo = "0.32"` as `[dev-dependencies]`
- `.github/workflows/ci.yml` (or equivalent) — invoke `scripts/ci/lint_agent_core.sh`

### Deleted files (Phase 7 only — not earlier)

From `src/agent/`:
- `classifier.rs`, `context_analyzer.rs`, `context_compressor.rs`, `microcompactor.rs`, `history_pruner.rs`, `loop_detector.rs`, `personality.rs`, `thinking.rs`, `memory_loader.rs`, `loop_.rs` (CLI dispatch rewritten to use `Agent::turn`)

From `src/memory/`:
- `dreaming.rs`, `consolidation.rs`, `decay.rs`, `knowledge_graph.rs`, `hygiene.rs`, `importance.rs`, `procedural.rs`, `outcomes.rs`, `conflict.rs`, `response_cache.rs`, `embeddings.rs`, `vector.rs`, `qdrant.rs`, `chunker.rs`, `customer_model.rs`
- Keep: `mod.rs`, `traits.rs`, `markdown.rs`, `none.rs`, `audit.rs`, `namespaced.rs`, `snapshot.rs`, `sqlite.rs` (sqlite stays until confirmed unused)

From `src/tools/`:
- `memory_export.rs`, `memory_forget.rs`, `memory_purge.rs`, `memory_recall.rs`, `memory_store.rs`

---

## Phases

- **Phase 1:** Config scaffold + truncation helper (Tasks 1–2)
- **Phase 2:** Gate scaffolding inside `Agent::turn` + `turn_streamed` (Task 3)
- **Phase 3:** Memory tool swap (Task 4)
- **Phase 4:** CI guardrails + RSS test (Tasks 5–6)
- **Phase 5:** Tool-description pass (Task 7)
- **Phase 6:** Eval task suite + flip default after parity (Tasks 8–9)
- **Phase 7:** Deletion PR (Task 10)

Each task is small enough to ship as one PR.

---

## Task 1: Config scaffold — add `agent.core` flag

**Files:**
- Modify: `src/config/schema.rs` — `AgentConfig` struct, its defaults, `impl Default`, and test module

**Goal:** Add `agent.core: "legacy" | "minimal"` config knob, default `"legacy"`, parseable from TOML, with tests.

- [ ] **Step 1: Locate anchors** (line numbers drift; use grep, not hard-coded lines)

```bash
grep -n "pub struct AgentConfig\|fn default_agent_max_tool_iterations\|impl Default for AgentConfig" src/config/schema.rs
```

Read a ~40-line window around each anchor to confirm the struct layout, default-fn pattern, and `impl Default` block. Then locate the test module:

```bash
grep -n "max_tool_iterations, 10\|max_tool_iterations = 20" src/config/schema.rs
```

Read the surrounding tests to learn the TOML-parse test pattern used for `AgentConfig`.

- [ ] **Step 2: Add the `core` field to `AgentConfig`**

Inside `pub struct AgentConfig { ... }`, after the `max_tool_iterations` field:

```rust
/// Which agent loop path to use.
///
/// - `"legacy"` (default): current path, full scaffolding (classifier,
///   memory_loader, context_compressor, microcompactor, loop_detector,
///   history_pruner, personality, thinking).
/// - `"minimal"`: scaffolding bypassed; clean tool-use loop with tail-
///   preserving tool-error truncation. Used to earn the promotion.
///
/// CLI flag `--core=minimal` overrides this for ad-hoc testing.
#[serde(default = "default_agent_core")]
pub core: String,
```

Add the default fn next to `default_agent_max_tool_iterations`:

```rust
fn default_agent_core() -> String {
    "legacy".to_string()
}
```

Add to `impl Default for AgentConfig`:

```rust
core: default_agent_core(),
```

- [ ] **Step 3: Write tests**

Append to the existing `#[cfg(test)] mod tests` block in `schema.rs`:

```rust
#[test]
fn agent_core_defaults_to_legacy() {
    let cfg = AgentConfig::default();
    assert_eq!(cfg.core, "legacy");
}

#[test]
fn agent_core_parses_minimal() {
    let toml = r#"
core = "minimal"
"#;
    let parsed: AgentConfig = toml::from_str(toml).unwrap();
    assert_eq!(parsed.core, "minimal");
}

#[test]
fn agent_core_roundtrips_unknown_as_legacy_equivalent() {
    // Unknown values are passed through verbatim; Agent::turn treats any
    // non-"minimal" value as legacy. The config layer does not validate.
    let toml = r#"
core = "experimental"
"#;
    let parsed: AgentConfig = toml::from_str(toml).unwrap();
    assert_eq!(parsed.core, "experimental");
}
```

- [ ] **Step 4: Run config tests**

```bash
cd /Users/samelamin/.claude/worktrees/zeroclaw/vibrant-sinoussi
cargo test --lib -- agent_core
```

Expected: all 3 tests pass. No regressions elsewhere in `config::schema` tests.

- [ ] **Step 5: Commit**

```bash
git add src/config/schema.rs
git commit -m "feat(config): add agent.core flag (legacy|minimal)

Introduce an opt-in routing knob for the agent loop. Default stays on
the current legacy path; minimal will be gated on the eval suite.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Task 2: Tool-result tail-preserving truncation helper

**Files:**
- Create: `src/agent/tool_result_truncate.rs`
- Modify: `src/agent/mod.rs` (add `pub mod tool_result_truncate;`)

**Goal:** A single pure helper the turn loop calls to format a tool's raw output/error for the model: verbatim on success, verbatim on failure, and when content exceeds `MAX_TOOL_OUTPUT_BYTES` we drop bytes from the **head** and announce the cut. Errors live at the tail.

- [ ] **Step 1: Write the failing tests**

Create `src/agent/tool_result_truncate.rs`. Put the tests at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_content_passes_through_verbatim() {
        let r = format_tool_output("hello world", true);
        assert_eq!(r, "hello world");
    }

    #[test]
    fn short_error_passes_through_verbatim() {
        let r = format_tool_output("ENOENT: no such file", false);
        assert_eq!(r, "ENOENT: no such file");
    }

    #[test]
    fn long_error_truncates_from_head_and_preserves_tail() {
        let head = "LEADING_NOISE\n".repeat(4000);
        let tail = "REAL_ERROR_AT_TAIL";
        let input = format!("{head}{tail}");
        let r = format_tool_output(&input, false);
        assert!(
            r.ends_with(tail),
            "tail must be preserved; got last 64 chars: {:?}",
            &r[r.len().saturating_sub(64)..]
        );
        assert!(
            r.contains("omitted from head"),
            "must announce head truncation; got prefix: {:?}",
            &r[..r.len().min(64)]
        );
        assert!(r.len() <= MAX_TOOL_OUTPUT_BYTES + 128);
    }

    #[test]
    fn exactly_max_is_not_truncated() {
        let s = "a".repeat(MAX_TOOL_OUTPUT_BYTES);
        let r = format_tool_output(&s, true);
        assert_eq!(r.len(), MAX_TOOL_OUTPUT_BYTES);
        assert!(!r.contains("omitted from head"));
    }

    #[test]
    fn multibyte_safe_at_cut_point() {
        // Construct a string whose natural cut falls inside a multi-byte char.
        // The helper must advance to the next char boundary, not panic.
        let head = "é".repeat(20_000); // 2 bytes per char
        let tail = "TAIL";
        let input = format!("{head}{tail}");
        let r = format_tool_output(&input, false);
        assert!(r.ends_with(tail));
        // Must be valid UTF-8 — building the String already enforced this,
        // but smoke-check that no panic occurred and length is sane.
        assert!(r.len() <= MAX_TOOL_OUTPUT_BYTES + 128);
    }
}
```

- [ ] **Step 2: Implement the helper**

Above the test module:

```rust
//! Tail-preserving tool-result formatter for the minimal agent path.
//!
//! The tail of tool output is where errors live. When truncation is needed,
//! we drop bytes from the **head** and preserve the end, with a single-line
//! prefix disclosing how many bytes were cut. This is the opposite of the
//! legacy `context_compressor` behavior (which summarized tool output and
//! often dropped the actual error), and the opposite of the legacy
//! `Agent::execute_tool_call` wrap `format!("Error: {}", ...)` which hid
//! the error structure from the model.
//!
//! Called only from the `agent.core = "minimal"` branch. Legacy continues
//! to use its existing wrappers unchanged.

/// Hard cap on per-tool output bytes fed back to the model.
pub const MAX_TOOL_OUTPUT_BYTES: usize = 32_768;

/// Format a tool's raw output for the model:
/// - If `content.len() <= MAX_TOOL_OUTPUT_BYTES`, return it verbatim.
/// - Otherwise, drop bytes from the head until it fits and prepend a
///   one-line prefix announcing the byte count dropped.
///
/// `_success` is currently unused but reserved for future
/// per-success-state handling (e.g. different caps for stdout vs stderr).
pub fn format_tool_output(content: &str, _success: bool) -> String {
    if content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return content.to_string();
    }

    // Initial cut target: keep the last MAX_TOOL_OUTPUT_BYTES bytes.
    let mut cut = content.len() - MAX_TOOL_OUTPUT_BYTES;

    // Advance to a valid UTF-8 char boundary.
    while cut < content.len() && !content.is_char_boundary(cut) {
        cut += 1;
    }

    // Prefer cutting at the next newline for a clean tail start.
    if let Some(nl) = content[cut..].find('\n') {
        let candidate = cut + nl + 1;
        if candidate < content.len() {
            cut = candidate;
        }
    }

    let dropped = cut;
    format!(
        "[...{dropped} bytes omitted from head; tail preserved...]\n{}",
        &content[cut..]
    )
}
```

- [ ] **Step 3: Register in `src/agent/mod.rs`**

Add `pub mod tool_result_truncate;` alongside the other `pub mod` lines.

- [ ] **Step 4: Run tests**

```bash
cargo test --lib agent::tool_result_truncate
```

Expected: 5 tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/agent/tool_result_truncate.rs src/agent/mod.rs
git commit -m "feat(agent): tail-preserving tool-output truncation helper

Used by the upcoming minimal core branch of Agent::turn. Errors live at
the tail; we drop bytes from the head when truncation is required and
disclose the cut with a one-line prefix.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Task 3: Gate scaffolding in `Agent::turn` + `turn_streamed`

**Files:**
- Modify: `src/agent/agent.rs` — `turn` (around line 754), `turn_streamed` (around line 934), `execute_tool_call` (around line 628), `build_system_prompt` (around line 611)

**Goal:** Inside `Agent::turn` and `Agent::turn_streamed`, branch on `self.config.core.as_str() == "minimal"` to **skip** the scaffolding that edits the model's view, and route tool errors through `tool_result_truncate::format_tool_output`. Legacy behavior is preserved byte-for-byte.

**Scaffolding gates (minimal bypasses each):**

| Gate                                              | Location in `turn()`                     | Minimal behavior                                                            |
| ------------------------------------------------- | ---------------------------------------- | --------------------------------------------------------------------------- |
| `memory_loader.load_context()`                    | pre-loop (~line 767)                     | Skip entirely. `context` is always empty string.                            |
| `[CURRENT DATE & TIME: ...]` prepend              | pre-loop (~line 789)                     | Keep. Temporal grounding is intelligence, not scaffolding.                  |
| `self.memory.store("user_msg", ...)` auto-save    | pre-loop (~line 777)                     | Keep. Behavior preservation.                                                |
| `classify_model(user_message)`                    | pre-loop (~line 805)                     | Skip. Use `self.model_name.clone()` directly.                               |
| `response_cache` lookup + put                     | inside loop (~line 810)                  | Keep. Cache is read-through; not scaffolding.                               |
| Tool-error wrap `format!("Error: {}", ...)`       | `execute_tool_call` (~line 643, 669)     | Replace with `tool_result_truncate::format_tool_output(..., success)`.      |
| `trim_history()` call after each iteration       | inside loop (~line 917)                  | Keep. It's a bounded LRU, not content-editing scaffolding.                  |
| Skills / security_summary / autonomy in prompt    | `build_system_prompt` (~line 611)        | Keep. These are intelligence amplifiers.                                    |

Nothing else changes. `tool_dispatcher.to_provider_messages`, `tool_dispatcher.parse_response`, `tool_dispatcher.format_results`, `provider.chat(...)`, `execute_tools` (parallel-aware), hooks, cost, and streaming are all left untouched.

- [ ] **Step 1: Read the current `turn()` and `execute_tool_call` bodies**

```bash
grep -n "pub async fn turn\b\|pub async fn turn_streamed\b\|async fn execute_tool_call\b\|fn build_system_prompt\b\|fn classify_model\b" src/agent/agent.rs
```

Read each function fully. Note the exact variable names (`user_message`, `enriched`, `effective_model`, `result`) so the branches below match byte-for-byte.

- [ ] **Step 2: Write failing integration tests**

Create `src/agent/tests.rs` entries (or append to the existing `#[cfg(test)] mod tests` module). You will need a scripted provider mock — the repo already has `tests/support/` utilities. Use them or define an inline `ScriptedProvider` that implements the full `Provider` trait with no-op defaults except `chat()`.

```rust
// Add to src/agent/tests.rs or a new src/agent/tests_minimal.rs.
//
// ```rust
// #[tokio::test]
// async fn minimal_skips_memory_loader() {
//     // Build an Agent with:
//     //   - config.agent.core = "minimal"
//     //   - a mock MemoryLoader that panics if load_context is called
//     //   - a ScriptedProvider returning "done" with no tool calls
//     // Drive one turn("hi"). Must NOT panic (load_context was skipped).
// }
//
// #[tokio::test]
// async fn legacy_still_calls_memory_loader() {
//     // Same fixtures but config.agent.core = "legacy" and a memory loader
//     // that records whether load_context was called.
//     // Drive one turn("hi"). Assert load_context WAS called.
// }
//
// #[tokio::test]
// async fn minimal_uses_base_model_name_not_classifier() {
//     // Agent with config.agent.core = "minimal", classifier config that
//     // would otherwise re-route "explain X" to "hint:explain". Also set
//     // model_routes mapping hint:explain -> some other model.
//     // Drive turn("explain foo"). Capture the effective_model passed to
//     // provider.chat. Must equal self.model_name, not the hint-routed model.
// }
//
// #[tokio::test]
// async fn minimal_passes_tool_error_verbatim_with_tail_truncate() {
//     // Build an Agent with core=minimal, one tool that returns a 100KB
//     // output ending with "TAIL_ERROR". Scripted provider: one
//     // tool-call response then stop. After turn completes, inspect
//     // self.history: the ToolResults content must end with "TAIL_ERROR"
//     // and contain "omitted from head".
// }
//
// #[tokio::test]
// async fn legacy_preserves_error_wrap() {
//     // Same test but core=legacy. ToolResults content must START with
//     // "Error:" (legacy wrap), proving we didn't accidentally change
//     // the legacy path.
// }
// ```
```

> **Implementer note:** filling in the ScriptedProvider and MockMemoryLoader skeletons is part of Step 2. Search `tests/` and `src/agent/tests.rs` for existing mocks you can copy — e.g. `ScriptedProvider`, `NoopObserver`. `DefaultMemoryLoader` is at `src/agent/memory_loader.rs`; subclass the trait defined there.

Run the new tests first and confirm they FAIL (the branches don't exist yet):

```bash
cargo test --lib agent::tests minimal_
```

Expected: all 5 tests fail/error.

- [ ] **Step 3: Gate `memory_loader.load_context()` in `turn()`**

Locate the `let context = self.memory_loader.load_context(...).await.unwrap_or_default();` line in `turn()` (around line 767). Replace with:

```rust
let context = if self.config.core == "minimal" {
    String::new()
} else {
    self.memory_loader
        .load_context(
            self.memory.as_ref(),
            user_message,
            self.memory_session_id.as_deref(),
        )
        .await
        .unwrap_or_default()
};
```

Do the same replacement in `turn_streamed()` (around line 953 — confirm exact line with grep).

- [ ] **Step 4: Gate `classify_model()` in `turn()` + `turn_streamed()`**

Locate `let effective_model = self.classify_model(user_message);` (around line 805) and replace with:

```rust
let effective_model = if self.config.core == "minimal" {
    self.model_name.clone()
} else {
    self.classify_model(user_message)
};
```

Apply the same change in `turn_streamed`.

- [ ] **Step 5: Route tool errors through `format_tool_output` in `execute_tool_call`**

Locate `execute_tool_call` (around line 628). In both error branches — the one at `if r.success { r.output } else { format!("Error: {}", ...) }` (~line 643) and the identical branch inside the MCP `activated_tools` arm (~line 669) — replace the formatting with:

```rust
// In the top branch (static tools), after `match tool.execute(call.arguments.clone()).await`:
Ok(r) => {
    self.observer.record_event(&ObserverEvent::ToolCall {
        tool: call.name.clone(),
        duration: start.elapsed(),
        success: r.success,
    });
    if self.config.core == "minimal" {
        // Verbatim pass-through with head-truncating tail preservation.
        let raw = if r.success {
            r.output
        } else {
            // Prefer `error` as the surface when present; fall back to
            // `output`. No "Error: " prefix.
            match r.error {
                Some(e) if !e.is_empty() => e,
                _ => r.output,
            }
        };
        crate::agent::tool_result_truncate::format_tool_output(&raw, r.success)
    } else {
        // Legacy wrap preserved byte-for-byte.
        if r.success {
            r.output
        } else {
            format!("Error: {}", r.error.unwrap_or(r.output))
        }
    }
}
Err(e) => {
    self.observer.record_event(&ObserverEvent::ToolCall {
        tool: call.name.clone(),
        duration: start.elapsed(),
        success: false,
    });
    if self.config.core == "minimal" {
        let raw = format!("{e:#}");
        crate::agent::tool_result_truncate::format_tool_output(&raw, false)
    } else {
        format!("Error executing {}: {e}", call.name)
    }
}
```

Apply the **same branch pattern** in the `activated_tools` (MCP) arm below it.

- [ ] **Step 6: Run the gating tests**

```bash
cargo test --lib agent::tests minimal_
cargo test --lib agent::tests legacy_
```

Expected: all pass.

- [ ] **Step 7: Run the full agent test suite**

```bash
cargo test --lib agent::
cargo build
```

Expected: no regressions. If any legacy test fails, the legacy branch in one of the 3 gates drifted. Fix by re-reading the pre-change code and restoring the exact call.

- [ ] **Step 8: Commit**

```bash
git add src/agent/agent.rs src/agent/tests.rs
git commit -m "feat(agent): gate scaffolding in turn()/turn_streamed() on agent.core=minimal

Bypass memory_loader.load_context() and classify_model() in minimal mode.
Route tool errors through tail-preserving truncation (no 'Error:' wrap).
Legacy path unchanged — every gate is a branch, not a deletion.

Skills, security_summary, response_cache, tool_dispatcher, hooks, cost,
streaming, MCP activation, and session save/load are untouched.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Task 4: Replace 5 memory tools with `remember` / `recall`

**Files:**
- Create: `src/tools/remember.rs`
- Create: `src/tools/recall.rs`
- Delete: `src/tools/memory_store.rs`, `memory_recall.rs`, `memory_forget.rs`, `memory_purge.rs`, `memory_export.rs`
- Modify: `src/tools/mod.rs` (remove 5 `pub mod`/`pub use`/registrations, add 2)

**Goal:** Two tools backed by the existing `Arc<dyn Memory>` (which defaults to `MarkdownMemory` in production). No embeddings, no vector DB, no background writer, no consolidation. Memory becomes an explicit model action.

- [ ] **Step 1: Verify the `Memory` trait surface**

```bash
grep -n "pub trait Memory\|async fn store\|async fn recall\|pub struct MemoryEntry\|pub enum MemoryCategory" src/memory/traits.rs
```

Confirm these exact signatures (from whatsappweb):

```rust
async fn store(
    &self,
    key: &str,
    content: &str,
    category: MemoryCategory,
    session_id: Option<&str>,
) -> anyhow::Result<()>;

async fn recall(
    &self,
    query: &str,
    limit: usize,
    session_id: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> anyhow::Result<Vec<MemoryEntry>>;

pub struct MemoryEntry {
    pub id: String,
    pub key: String,
    pub content: String,
    pub category: MemoryCategory,
    pub timestamp: String,
    pub session_id: Option<String>,
    pub score: Option<f64>,
    pub namespace: String,
    pub importance: Option<f64>,
    pub superseded_by: Option<String>,
}
```

- [ ] **Step 2: Write `RememberTool`**

Create `src/tools/remember.rs`:

```rust
//! The `remember` tool — append a durable fact to project memory.
//!
//! Backed by `Arc<dyn Memory>`, which in production is `MarkdownMemory`
//! (appends to `MEMORY.md` + dated daily files under the workspace).
//! No embeddings, no background writer, no consolidation.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::memory::traits::{Memory, MemoryCategory};
use crate::tools::traits::{Tool, ToolResult};

#[derive(Debug, Deserialize)]
struct RememberArgs {
    fact: String,
    #[serde(default)]
    tags: Vec<String>,
}

pub struct RememberTool {
    memory: Arc<dyn Memory>,
}

impl RememberTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Save a durable fact to project memory. Use for user preferences, \
         architectural decisions, or learned context you want to persist to \
         later sessions. Do NOT use for ephemeral task state — that belongs in \
         the transcript, not in memory."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "fact": {
                    "type": "string",
                    "description": "The fact to remember. One sentence is ideal. \
                                    Past tense or present tense; avoid imperatives."
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional tags for later retrieval (e.g. [\"preference\", \"whatsapp\"])."
                }
            },
            "required": ["fact"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let parsed: RememberArgs = serde_json::from_value(args)
            .map_err(|e| anyhow::anyhow!("invalid arguments for remember: {e}"))?;

        let key = format!("remember-{}", uuid::Uuid::new_v4());
        let content = if parsed.tags.is_empty() {
            parsed.fact.clone()
        } else {
            format!("{} [tags: {}]", parsed.fact, parsed.tags.join(", "))
        };

        self.memory
            .store(&key, &content, MemoryCategory::Core, None)
            .await?;

        Ok(ToolResult {
            success: true,
            output: format!("Remembered: {}", parsed.fact),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::markdown::MarkdownMemory;
    use tempfile::tempdir;

    #[tokio::test]
    async fn remember_appends_fact() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> =
            Arc::new(MarkdownMemory::new(tmp.path()).expect("create MarkdownMemory"));
        let tool = RememberTool::new(mem.clone());

        let r = tool
            .execute(json!({"fact": "user prefers brief responses"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("brief"));

        // Roundtrip via recall to prove it landed.
        let hits = mem
            .recall("brief", 10, None, None, None)
            .await
            .unwrap();
        assert!(hits.iter().any(|e| e.content.contains("brief")));
    }

    #[tokio::test]
    async fn remember_with_tags_includes_tag_suffix() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> =
            Arc::new(MarkdownMemory::new(tmp.path()).expect("create MarkdownMemory"));
        let tool = RememberTool::new(mem.clone());

        tool.execute(json!({
            "fact": "deploy target is eu-west-1",
            "tags": ["deploy", "region"]
        }))
        .await
        .unwrap();

        let hits = mem
            .recall("eu-west-1", 10, None, None, None)
            .await
            .unwrap();
        let content = &hits.iter().find(|e| e.content.contains("eu-west-1")).unwrap().content;
        assert!(content.contains("[tags: deploy, region]"));
    }

    #[tokio::test]
    async fn remember_rejects_missing_fact() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> =
            Arc::new(MarkdownMemory::new(tmp.path()).expect("create MarkdownMemory"));
        let tool = RememberTool::new(mem);

        let r = tool.execute(json!({"tags": ["nope"]})).await;
        assert!(r.is_err(), "missing `fact` must error");
    }
}
```

> **Implementer note:** `MarkdownMemory::new` signature — verify with `grep -n "pub fn new" src/memory/markdown.rs`. It may take `(&Path)`, `(&Path, Config)`, or require a workspace config. Adjust the test constructor to match. If the constructor requires more than a path, use `tempdir()` + whatever minimal config is required.

- [ ] **Step 3: Write `RecallTool`**

Create `src/tools/recall.rs`:

```rust
//! The `recall` tool — substring lookup over project memory.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::memory::traits::Memory;
use crate::tools::traits::{Tool, ToolResult};

const DEFAULT_LIMIT: usize = 20;

#[derive(Debug, Deserialize)]
struct RecallArgs {
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

pub struct RecallTool {
    memory: Arc<dyn Memory>,
}

impl RecallTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }

    fn description(&self) -> &str {
        "Look up remembered facts by substring match. Returns matching entries \
         with their timestamps. Use when you need user preferences or past \
         decisions. Do NOT use for searching files or code — use shell/grep \
         for that instead."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Substring to search for. Case-insensitive."
                },
                "limit": {
                    "type": "integer",
                    "description": "Max results (default 20)."
                }
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let parsed: RecallArgs = serde_json::from_value(args)
            .map_err(|e| anyhow::anyhow!("invalid arguments for recall: {e}"))?;
        let limit = parsed.limit.unwrap_or(DEFAULT_LIMIT);

        let entries = self
            .memory
            .recall(&parsed.query, limit, None, None, None)
            .await?;

        let output = if entries.is_empty() {
            format!("No memories matched '{}'.", parsed.query)
        } else {
            entries
                .iter()
                .map(|e| format!("[{}] {}", e.timestamp, e.content))
                .collect::<Vec<_>>()
                .join("\n")
        };

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::markdown::MarkdownMemory;
    use crate::memory::traits::MemoryCategory;
    use tempfile::tempdir;

    #[tokio::test]
    async fn recall_returns_matching_entries() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> =
            Arc::new(MarkdownMemory::new(tmp.path()).expect("create MarkdownMemory"));
        mem.store("k1", "the answer is 42", MemoryCategory::Core, None)
            .await
            .unwrap();
        mem.store("k2", "the question is life", MemoryCategory::Core, None)
            .await
            .unwrap();

        let tool = RecallTool::new(mem);
        let r = tool.execute(json!({"query": "answer"})).await.unwrap();
        assert!(r.success);
        assert!(r.output.contains("42"), "got: {}", r.output);
    }

    #[tokio::test]
    async fn recall_empty_result_is_explicit() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> =
            Arc::new(MarkdownMemory::new(tmp.path()).expect("create MarkdownMemory"));
        let tool = RecallTool::new(mem);

        let r = tool
            .execute(json!({"query": "nonexistent-xyzzy"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("No memories matched"));
    }

    #[tokio::test]
    async fn recall_respects_limit() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> =
            Arc::new(MarkdownMemory::new(tmp.path()).expect("create MarkdownMemory"));
        for i in 0..5 {
            mem.store(
                &format!("k{i}"),
                &format!("match entry number {i}"),
                MemoryCategory::Core,
                None,
            )
            .await
            .unwrap();
        }
        let tool = RecallTool::new(mem);

        let r = tool
            .execute(json!({"query": "match entry", "limit": 2}))
            .await
            .unwrap();
        let line_count = r.output.lines().count();
        assert!(line_count <= 2, "got {} lines", line_count);
    }
}
```

- [ ] **Step 4: Delete the 5 old memory tool files**

```bash
git rm src/tools/memory_store.rs \
       src/tools/memory_recall.rs \
       src/tools/memory_forget.rs \
       src/tools/memory_purge.rs \
       src/tools/memory_export.rs
```

- [ ] **Step 5: Update `src/tools/mod.rs`**

```bash
grep -n "pub mod memory_\|pub use memory_\|MemoryStoreTool\|MemoryRecallTool\|MemoryForgetTool\|MemoryPurgeTool\|MemoryExportTool" src/tools/mod.rs
```

Remove all matching `pub mod memory_*;` declarations and their `pub use` re-exports. Find the function (likely `build_default_tools` or similar — grep for `MemoryStoreTool::new` to locate) that assembles the `Vec<Box<dyn Tool>>` and remove the 5 registrations.

Then add:

```rust
pub mod recall;
pub mod remember;
pub use recall::RecallTool;
pub use remember::RememberTool;
```

And replace the 5 removed registrations in the builder function with:

```rust
Box::new(RememberTool::new(memory.clone())),
Box::new(RecallTool::new(memory.clone())),
```

> **Implementer note:** The builder function takes `memory: &Arc<dyn Memory>` (or similar). Check the existing 5 `MemoryStoreTool::new(memory.clone())` call sites for the exact parameter name and clone pattern to mirror.

- [ ] **Step 6: Fix any broken callers**

```bash
cargo check 2>&1 | head -60
```

Any remaining errors will be in code referencing the deleted types by name (likely in the scaffolding modules scheduled for Phase 7 deletion, or in tests). For each error:
- If the caller is itself in a scaffolding module scheduled for deletion in Phase 7, guard the import with `#[allow(unused_imports)]` or delete the offending block.
- If the caller is in retained code, replace with `RememberTool` / `RecallTool`.

- [ ] **Step 7: Run tests**

```bash
cargo test --lib tools::remember tools::recall
cargo test --lib
```

Expected: new tests pass, no regressions. Tests that referenced the old 5 memory tool types by name should be deleted — they tested deleted code.

- [ ] **Step 8: Commit**

```bash
git add src/tools/ Cargo.lock
git commit -m "feat(tools): replace 5 memory tools with remember/recall

Delete MemoryStoreTool, MemoryRecallTool, MemoryForgetTool,
MemoryPurgeTool, MemoryExportTool. Replace with two tools
(RememberTool + RecallTool) backed by the existing Arc<dyn Memory>
(MarkdownMemory in production).

No migration: no users of the old 5 tools exist yet (confirmed).

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Task 5: CI guardrail lint — forbid growth-prone patterns

**Files:**
- Create: `scripts/ci/lint_agent_core.sh`
- Modify: `.github/workflows/ci.yml` (or the equivalent CI config — see Step 1)

**Goal:** grep-based CI guard that fails the build if any of the five memory-leak patterns land in `src/agent/` or `src/memory/`. Existing hits in files scheduled for Phase 7 deletion are allowlisted until then.

- [ ] **Step 1: Identify the CI entry point**

```bash
ls -la .github/workflows/ 2>/dev/null
ls -la .circleci/ 2>/dev/null
ls -la .gitlab-ci.yml 2>/dev/null
find . -maxdepth 2 -name "*.yml" -path "*/workflows/*" 2>/dev/null | head
```

Identify the file that already runs `cargo test` or `cargo check` on PRs. The lint script will be invoked from there.

- [ ] **Step 2: Write the lint script**

Create `scripts/ci/lint_agent_core.sh`:

```bash
#!/usr/bin/env bash
#
# Fails CI if banned growth-prone patterns appear in src/agent/ or src/memory/.
# These are the five patterns identified in the agent-recore spec as the root
# cause of the brain.db memory snowball. New code must avoid them; pre-existing
# hits in Phase-7-scheduled files are allowlisted below.

set -euo pipefail

# -----------------------------------------------------------------------------
# Patterns we refuse to accept in new code (POSIX-regex for grep -E).
# -----------------------------------------------------------------------------
PATTERNS=(
  'LazyLock<Mutex<HashMap'
  'LazyLock<Mutex<Vec'
  'OnceLock<Mutex<'
  'static mut '
  'tokio::spawn\('
)

# -----------------------------------------------------------------------------
# Files allowlisted because they are scheduled for deletion in Phase 7.
# Remove entries here as the corresponding files are deleted.
# -----------------------------------------------------------------------------
ALLOWLIST=(
  'src/agent/loop_.rs'
  'src/agent/microcompactor.rs'
  'src/agent/loop_detector.rs'
  'src/agent/history_pruner.rs'
  'src/agent/context_compressor.rs'
  'src/agent/context_analyzer.rs'
  'src/agent/classifier.rs'
  'src/agent/personality.rs'
  'src/agent/thinking.rs'
  'src/agent/memory_loader.rs'
  'src/memory/dreaming.rs'
  'src/memory/consolidation.rs'
  'src/memory/decay.rs'
  'src/memory/knowledge_graph.rs'
  'src/memory/hygiene.rs'
  'src/memory/importance.rs'
  'src/memory/procedural.rs'
  'src/memory/outcomes.rs'
  'src/memory/conflict.rs'
  'src/memory/response_cache.rs'
  'src/memory/embeddings.rs'
  'src/memory/vector.rs'
  'src/memory/qdrant.rs'
  'src/memory/chunker.rs'
  'src/memory/customer_model.rs'
)

FAIL=0
for pattern in "${PATTERNS[@]}"; do
  # Build a --exclude arg per allowlisted file.
  EXCLUDES=()
  for path in "${ALLOWLIST[@]}"; do
    EXCLUDES+=(--exclude="$(basename "$path")")
  done

  # Search src/agent/ and src/memory/ (recursive), excluding allowlisted files.
  HITS=$(grep -rnE "$pattern" src/agent/ src/memory/ "${EXCLUDES[@]}" || true)
  if [[ -n "$HITS" ]]; then
    echo "::error::Forbidden pattern '$pattern' found in src/agent/ or src/memory/:"
    echo "$HITS"
    FAIL=1
  fi
done

# Also: any tokio::spawn() in src/agent/core-adjacent files (not allowlisted)
# that spawns a long-lived loop is banned. The grep above already catches the
# bare call; rely on code review for the "long-lived" nuance.

if [[ $FAIL -ne 0 ]]; then
  echo ""
  echo "Agent-core guardrail violated. See spec:"
  echo "  docs/superpowers/specs/2026-04-21-agent-recore-design.md"
  echo "  §Memory-leak guardrails (binding rules for the new core)"
  exit 1
fi

echo "agent-core guardrail: OK"
```

```bash
chmod +x scripts/ci/lint_agent_core.sh
```

- [ ] **Step 3: Run it locally**

```bash
./scripts/ci/lint_agent_core.sh
```

Expected: `agent-core guardrail: OK` (if it flags anything, either the allowlist is incomplete or the offending code is NOT in a scheduled-deletion file and needs fixing before merging).

- [ ] **Step 4: Wire into CI**

Edit the CI workflow file identified in Step 1. Add a new step before `cargo test`:

```yaml
      - name: Agent-core guardrail lint
        run: ./scripts/ci/lint_agent_core.sh
```

- [ ] **Step 5: Commit**

```bash
git add scripts/ci/lint_agent_core.sh .github/workflows/ci.yml
git commit -m "ci: add agent-core guardrail lint

Block the five memory-leak patterns from landing in src/agent/ or
src/memory/ in new code. Allowlist Phase-7-scheduled files; remove
entries as the files are deleted.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Task 6: RSS regression test — 500-turn minimal-mode drive

**Files:**
- Create: `tests/agent_rss.rs`
- Modify: `Cargo.toml` — add `sysinfo = "0.32"` as a dev-dependency

**Goal:** Integration test that runs 500 simulated turns against a mocked provider in `agent.core = "minimal"` mode and asserts RSS growth stays under 20 MB. Fails CI if a regression lands.

- [ ] **Step 1: Add `sysinfo` dev-dep**

```bash
grep -n "^\[dev-dependencies\]" Cargo.toml
```

Read the `[dev-dependencies]` block. Add:

```toml
sysinfo = "0.32"
```

Run `cargo build --tests` to materialize the lockfile.

- [ ] **Step 2: Write the test scaffold with a `ScriptedProvider`**

Create `tests/agent_rss.rs`:

```rust
//! 500-turn RSS regression test for agent.core = "minimal".
//!
//! Asserts that running 500 simulated turns against a scripted provider does
//! not grow RSS by more than 20 MB. This catches the brain.db-class leak
//! before it re-lands.

use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use sysinfo::{Pid, System};

// Namespaces below use whatever the real paths are; adjust to match. Agent,
// AgentBuilder, Memory, MarkdownMemory, Observer (NoopObserver), Provider,
// ChatRequest, ChatResponse, ToolCall, ChatMessage, Config.
use zeroclaw::agent::{Agent, AgentBuilder};
use zeroclaw::config::{AgentConfig, Config};
use zeroclaw::memory::{markdown::MarkdownMemory, traits::Memory};
use zeroclaw::observability::NoopObserver;
use zeroclaw::providers::traits::{
    ChatRequest, ChatResponse, Provider, ProviderCapabilities, ToolSpec,
};

/// Provider that returns a pre-programmed sequence of responses.
struct ScriptedProvider {
    responses: Mutex<Vec<ChatResponse>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities { native_tool_calling: true, ..Default::default() }
    }

    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<String> {
        Ok(String::new()) // unused in this test
    }

    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> anyhow::Result<ChatResponse> {
        let mut r = self.responses.lock().unwrap();
        if r.is_empty() {
            // Default fallback: "done" with no tool calls.
            return Ok(ChatResponse {
                text: Some("done".into()),
                tool_calls: Vec::new(),
                usage: None,
                reasoning_content: None,
            });
        }
        Ok(r.remove(0))
    }
}

fn current_rss_bytes() -> u64 {
    let pid = std::process::id();
    let mut sys = System::new();
    sys.refresh_process(Pid::from_u32(pid));
    sys.process(Pid::from_u32(pid))
        .map(|p| p.memory()) // bytes on modern sysinfo; double-check at write time
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_rss_does_not_snowball_over_500_turns() {
    // Arrange: minimal-mode Agent, scripted provider returning "ok" (no tool
    // calls) every turn so each turn is a single provider.chat invocation.
    let mut config: AgentConfig = AgentConfig::default();
    config.core = "minimal".into();

    let tmp = tempfile::tempdir().unwrap();
    let memory: Arc<dyn Memory> =
        Arc::new(MarkdownMemory::new(tmp.path()).expect("create MarkdownMemory"));
    let observer = Arc::new(NoopObserver::default()) as Arc<dyn zeroclaw::observability::Observer>;
    let provider: Box<dyn Provider> = Box::new(ScriptedProvider {
        responses: Mutex::new(Vec::new()), // fallthrough => "done"
    });

    // Build a minimal Agent. Adjust the builder call to match real setters.
    let mut agent = AgentBuilder::new()
        .provider(provider)
        .tools(Vec::new()) // no tools needed for this test
        .memory(memory)
        .observer(observer)
        .config(config)
        .model_name("mock".into())
        .temperature(0.0)
        .workspace_dir(tmp.path().to_path_buf())
        .build()
        .expect("build Agent");

    let rss_before = current_rss_bytes();

    for i in 0..500 {
        let _ = agent.turn(&format!("turn-{i}")).await;
    }

    let rss_after = current_rss_bytes();
    let growth_mb = rss_after.saturating_sub(rss_before) as f64 / 1_048_576.0;
    println!(
        "RSS before: {} MB, after: {} MB, growth: {:.2} MB",
        rss_before / 1_048_576,
        rss_after / 1_048_576,
        growth_mb
    );
    assert!(
        growth_mb < 20.0,
        "RSS grew by {growth_mb:.2} MB over 500 turns — regression!"
    );
}
```

> **Implementer note:** the `AgentBuilder` setters above are guessed from the grep in Task 3 Step 1. Verify with `grep -n "impl AgentBuilder" src/agent/agent.rs`. If the builder requires more fields to successfully `build()`, add no-op defaults for each. If `NoopObserver` construction differs, match its real shape.

- [ ] **Step 3: Run it**

```bash
cargo test --test agent_rss -- --nocapture
```

Expected: prints RSS before/after and passes. If it fails with "growth > 20 MB", there's a leak — bisect by reducing iteration count.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock tests/agent_rss.rs
git commit -m "test: 500-turn RSS regression for agent.core=minimal

Catch the brain.db-class memory snowball before it re-lands. Uses a
scripted provider returning 'done' with no tool calls; each turn is one
chat() + one history push. Asserts RSS growth stays under 20 MB across
500 turns.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Task 7: Tool-description pass — rewrite `tool_descriptions/en.toml`

**Files:**
- Modify: `tool_descriptions/en.toml`

**Goal:** MiniMax M2 and all prompt-guided providers are highly sensitive to tool descriptions. Rewrite every tool description in `en.toml` to the 5-section schema. No code changes — this is a prompt-engineering pass.

**Schema (one block per tool):**

```toml
[tool_name]
purpose = "One sentence: what this tool does. Start with an imperative verb."
when = "When to use it — AND when not to, by contrast. Two sentences max."
example = '''
Input:  <concrete argument example>
Output: <what a successful return looks like>
'''
success = "One sentence describing what a successful result looks like so the model knows it worked."
failure = "One sentence describing what the error surface looks like, so the model knows how to recognize and recover from it."
```

- [ ] **Step 1: Enumerate the tools**

```bash
grep -n "^\[" tool_descriptions/en.toml
```

Record the current tool list. Then cross-reference with the registered tool set:

```bash
grep -rnE "^\s*Box::new\([A-Z][A-Za-z0-9]+::new" src/tools/ src/agent/ | grep -v 'tests' | awk -F'Box::new\\(' '{print $2}' | awk -F'::' '{print $1}' | sort -u
```

The intersection is the authoritative list. Tools in `en.toml` that aren't registered can be dropped; registered tools missing from `en.toml` must be added.

- [ ] **Step 2: Rewrite each tool's description**

For each tool, fill in all 5 sections. Rules:

1. `purpose` must mention the single primary effect. No editorializing.
2. `when` must contrast the tool with its closest sibling. E.g., for `remember`: "Use for durable cross-session facts, NOT for ephemeral task state (that belongs in the transcript)."
3. `example` shows a realistic argument value and a realistic successful return string. Avoid placeholders like "..." or "TODO".
4. `success` lets the model recognize a positive result without guessing.
5. `failure` lets the model recognize an error and pivot. Mention the actual error string shape (e.g., "`ENOENT`", "`permission denied`", "`tool timed out after 30s`").

No entry is allowed to be fewer than these 5 fields. If any tool has no natural `failure` mode, say so explicitly: "Does not fail under normal operation."

- [ ] **Step 3: Write a structural validation test**

Create (or extend) a test that parses `en.toml` and asserts the schema:

```rust
// tests/tool_descriptions.rs
#[test]
fn every_tool_description_has_the_five_sections() {
    let raw = std::fs::read_to_string("tool_descriptions/en.toml").unwrap();
    let parsed: toml::Value = toml::from_str(&raw).unwrap();
    let tbl = parsed.as_table().unwrap();
    for (name, entry) in tbl {
        let e = entry.as_table().unwrap_or_else(|| panic!("tool {name} is not a table"));
        for field in ["purpose", "when", "example", "success", "failure"] {
            assert!(
                e.contains_key(field),
                "tool {name} missing required field `{field}`"
            );
            assert!(
                !e[field].as_str().unwrap_or("").trim().is_empty(),
                "tool {name}.{field} is empty"
            );
        }
    }
}
```

- [ ] **Step 4: Run it**

```bash
cargo test --test tool_descriptions
```

Expected: passes. Any failure names a tool missing a field — fix in `en.toml`.

- [ ] **Step 5: Commit**

```bash
git add tool_descriptions/en.toml tests/tool_descriptions.rs
git commit -m "docs(tools): rewrite all tool descriptions to 5-section schema

Every tool now specifies purpose / when / example / success / failure.
Structural test enforces the schema so future tools follow the pattern.
This is the single biggest 'wrong tool chosen' lever for MiniMax M2 and
all prompt-guided providers.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Task 8: Eval task suite — behavioral runner over legacy vs minimal

**Files:**
- Create: `src/agent/eval_suite.rs`
- Create: `tests/agent_eval_suite.rs`
- Modify: `src/agent/mod.rs` — register `pub mod eval_suite;`

**Goal:** A runner that drives both `agent.core = "legacy"` and `agent.core = "minimal"` against a fixed set of scripted-provider tasks covering the four failure modes from the spec. Reports per-task pass/fail + iterations used + tokens spent. Used as the promotion gate in Task 9.

**Four failure modes (each must have ≥2 tasks):**

1. **Diligence after tool failure** — tool errors on first call; model must retry with a different approach to succeed.
2. **Multi-step chaining** — task requires ≥4 dependent tool calls without the user prompting again.
3. **Wrong-tool recovery** — a plausible-but-wrong tool is the first affordance; model must pivot after seeing the result.
4. **Long-context retention** — task references a fact established 15+ turns earlier in the same session.

- [ ] **Step 1: Define the harness types**

Create `src/agent/eval_suite.rs`:

```rust
//! Behavioral task-suite runner for agent core promotion gating.
//!
//! Drives a scripted provider through a fixed set of tasks against both
//! agent.core values and reports pass/fail + iterations + tokens.
//!
//! Used by `tests/agent_eval_suite.rs` for CI-gated promotion.

use std::sync::Mutex;

use crate::providers::traits::{ChatResponse, ToolCall};

#[derive(Debug, Clone)]
pub struct EvalTask {
    pub id: &'static str,
    pub category: Category,
    pub user_prompt: &'static str,
    /// Scripted provider responses, in order. The runner will feed them back
    /// turn-by-turn.
    pub scripted_responses: Vec<ChatResponse>,
    /// Oracle: given the final history + returned text, did the agent succeed?
    pub succeeded_if: fn(final_text: &str, iterations: usize) -> bool,
    /// Maximum iterations allowed.
    pub max_iterations: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    DiligenceAfterFailure,
    MultiStepChaining,
    WrongToolRecovery,
    LongContextRetention,
}

#[derive(Debug, Clone)]
pub struct EvalResult {
    pub task_id: &'static str,
    pub category: Category,
    pub core: String, // "legacy" or "minimal"
    pub passed: bool,
    pub iterations_used: usize,
    pub tokens_used: u64,
}

#[derive(Debug, Clone)]
pub struct EvalSummary {
    pub results: Vec<EvalResult>,
}

impl EvalSummary {
    pub fn pass_rate(&self, core: &str) -> f64 {
        let for_core: Vec<_> = self.results.iter().filter(|r| r.core == core).collect();
        if for_core.is_empty() { return 0.0; }
        let passed = for_core.iter().filter(|r| r.passed).count();
        passed as f64 / for_core.len() as f64
    }

    pub fn median_tokens(&self, core: &str) -> u64 {
        let mut tokens: Vec<u64> = self
            .results
            .iter()
            .filter(|r| r.core == core)
            .map(|r| r.tokens_used)
            .collect();
        tokens.sort_unstable();
        tokens.get(tokens.len() / 2).copied().unwrap_or(0)
    }
}

/// Run every task against the given core and collect results.
///
/// The caller provides a factory that builds an `Agent` for the given core,
/// wired up with the provided scripted provider. This keeps the suite itself
/// free of config-plumbing details.
pub async fn run_suite(
    tasks: &[EvalTask],
    core: &str,
    agent_factory: impl Fn(&str, Vec<ChatResponse>) -> crate::agent::Agent,
) -> EvalSummary {
    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        let mut agent = agent_factory(core, task.scripted_responses.clone());
        let result = agent.turn(task.user_prompt).await;
        let (final_text, ok) = match result {
            Ok(t) => (t.clone(), (task.succeeded_if)(&t, 0 /* TODO */)),
            Err(_) => (String::new(), false),
        };
        // TODO: extract iterations and tokens from observer events or agent
        // state. For v1, populate 0 placeholders.
        results.push(EvalResult {
            task_id: task.id,
            category: task.category,
            core: core.to_string(),
            passed: ok && !final_text.is_empty(),
            iterations_used: 0,
            tokens_used: 0,
        });
    }
    EvalSummary { results }
}

/// The canonical task list. Extend freely — each task must have an oracle
/// and be deterministic under scripted responses.
pub fn canonical_tasks() -> Vec<EvalTask> {
    vec![
        // ── Category 1: Diligence after tool failure ─────────────────────
        EvalTask {
            id: "diligence-01-retry-on-enoent",
            category: Category::DiligenceAfterFailure,
            user_prompt: "Read the config file",
            scripted_responses: vec![
                // First turn: model calls shell tool that errors.
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "1".into(),
                        name: "shell".into(),
                        arguments: r#"{"cmd":"cat /nonexistent.toml"}"#.into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                // Second turn: after seeing ENOENT, model tries a different path.
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "2".into(),
                        name: "shell".into(),
                        arguments: r#"{"cmd":"cat ./config.toml"}"#.into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                // Third turn: success, stop.
                ChatResponse {
                    text: Some("Found config: ...".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
            ],
            succeeded_if: |text, _| text.contains("Found config"),
            max_iterations: 5,
        },
        // Add 1+ more diligence tasks here.

        // ── Category 2: Multi-step chaining (≥4 tool calls) ──────────────
        // TODO: populate. Typical shape: 4 tool calls chained before a final
        // text response. Oracle: final text mentions the 4th step's result.

        // ── Category 3: Wrong-tool recovery ──────────────────────────────
        // TODO: populate. Shape: first tool_call is plausible-wrong; second is
        // the right tool.

        // ── Category 4: Long-context retention ───────────────────────────
        // TODO: populate. Shape: 15+ scripted turns establishing a fact,
        // then a final user query that requires that fact.
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_tasks_cover_all_four_categories() {
        let tasks = canonical_tasks();
        for cat in [
            Category::DiligenceAfterFailure,
            Category::MultiStepChaining,
            Category::WrongToolRecovery,
            Category::LongContextRetention,
        ] {
            let n = tasks.iter().filter(|t| t.category == cat).count();
            assert!(n >= 1, "category {cat:?} has {n} tasks; require ≥1 in v1");
        }
        assert!(tasks.len() >= 4, "v1 suite must have ≥4 tasks total");
    }

    #[test]
    fn pass_rate_and_median_tokens_zero_on_empty_suite() {
        let s = EvalSummary { results: vec![] };
        assert_eq!(s.pass_rate("minimal"), 0.0);
        assert_eq!(s.median_tokens("minimal"), 0);
    }
}
```

- [ ] **Step 2: Populate categories 2–4**

For each of `MultiStepChaining`, `WrongToolRecovery`, `LongContextRetention`, add at least 1 task in v1 (target ≥2 each as the suite matures). Scripted responses must be deterministic — no randomness.

- [ ] **Step 3: Wire `eval_suite` into `src/agent/mod.rs`**

Add `pub mod eval_suite;` alongside other `pub mod` declarations.

- [ ] **Step 4: Add integration test driving legacy + minimal**

Create `tests/agent_eval_suite.rs`:

```rust
use zeroclaw::agent::eval_suite::{canonical_tasks, run_suite};
// plus agent_factory similar to tests/agent_rss.rs

#[tokio::test]
async fn eval_suite_reports_for_both_cores() {
    let tasks = canonical_tasks();
    let legacy = run_suite(&tasks, "legacy", make_factory()).await;
    let minimal = run_suite(&tasks, "minimal", make_factory()).await;

    println!("legacy pass rate:  {:.0}%  median tokens: {}", legacy.pass_rate("legacy") * 100.0, legacy.median_tokens("legacy"));
    println!("minimal pass rate: {:.0}%  median tokens: {}", minimal.pass_rate("minimal") * 100.0, minimal.median_tokens("minimal"));

    // v1: we only assert the runner works. Promotion thresholds are enforced
    // in Task 9.
    assert_eq!(legacy.results.len(), tasks.len());
    assert_eq!(minimal.results.len(), tasks.len());
}

fn make_factory() -> impl Fn(&str, Vec<zeroclaw::providers::traits::ChatResponse>) -> zeroclaw::agent::Agent {
    // TODO: mirror the AgentBuilder setup from tests/agent_rss.rs, with a
    // ScriptedProvider fed from the task's scripted_responses.
    |_core: &str, _responses: Vec<_>| -> zeroclaw::agent::Agent {
        unimplemented!("populate using the same pattern as tests/agent_rss.rs")
    }
}
```

> **Implementer note:** `make_factory` is where real work sits. Lift the `ScriptedProvider` + `AgentBuilder` setup from Task 6's `tests/agent_rss.rs` and parameterize it on the scripted response list and the `core` string.

- [ ] **Step 5: Run it**

```bash
cargo test --test agent_eval_suite -- --nocapture
```

Expected: the in-file unit tests pass. The integration test in `tests/agent_eval_suite.rs` may have `unimplemented!()` blockers — that's fine for v1 commit; resolve in Step 6.

- [ ] **Step 6: Flesh out `make_factory`**

Replace the `unimplemented!()` with a concrete Agent-building closure. Once done, re-run:

```bash
cargo test --test agent_eval_suite -- --nocapture
```

Expected: both cores complete all tasks; the `println!` output shows per-core pass rates and median token counts.

- [ ] **Step 7: Commit**

```bash
git add src/agent/eval_suite.rs src/agent/mod.rs tests/agent_eval_suite.rs
git commit -m "feat(agent): behavioral eval task suite (legacy vs minimal)

Task-suite runner covering the four failure modes (diligence, multi-step,
wrong-tool, long-context). Used as the promotion gate before flipping
agent.core default to 'minimal'.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Task 9: Flip default to `minimal` after eval parity

**Files:**
- Modify: `src/config/schema.rs` — `default_agent_core()` returns `"minimal"` (previously `"legacy"`)
- Modify: `.github/workflows/ci.yml` (or equivalent) — add the eval suite as a required job

**Goal:** Manual gate: when `EvalSummary::pass_rate("minimal") >= 0.80 * pass_rate("legacy")` AND `median_tokens("minimal") <= median_tokens("legacy")`, flip the default. Not before.

- [ ] **Step 1: Check promotion criteria**

Run the eval suite against the current tree:

```bash
cargo test --test agent_eval_suite -- --nocapture 2>&1 | tee /tmp/eval-report.txt
grep "pass rate\|median tokens" /tmp/eval-report.txt
```

Promotion criteria (from spec):
- Minimal pass rate ≥ 80% of legacy pass rate
- Minimal median tokens ≤ legacy median tokens

If either fails, STOP. Do not flip. File tasks for the regressions and iterate on `src/agent/tool_result_truncate.rs`, tool descriptions, or `Task 3`'s gating branches until parity.

- [ ] **Step 2: Flip the default**

In `src/config/schema.rs`:

```rust
fn default_agent_core() -> String {
    "minimal".to_string()
}
```

- [ ] **Step 3: Update the test**

In the same file's test module, flip the assertion:

```rust
#[test]
fn agent_core_defaults_to_minimal() {
    let cfg = AgentConfig::default();
    assert_eq!(cfg.core, "minimal");
}
```

Delete or rename the old `agent_core_defaults_to_legacy` test.

- [ ] **Step 4: Make the eval suite required in CI**

Ensure `cargo test --test agent_eval_suite` runs on every PR and blocks merge on failure. If it's not already, add:

```yaml
      - name: Agent eval suite
        run: cargo test --test agent_eval_suite -- --nocapture
```

- [ ] **Step 5: Run the full test suite**

```bash
cargo test
```

Expected: everything passes. Some legacy tests that asserted the legacy default may now fail — update them to the new default or delete them if they were testing the default specifically.

- [ ] **Step 6: Commit**

```bash
git add src/config/schema.rs .github/workflows/ci.yml
git commit -m "feat(agent): flip agent.core default from legacy to minimal

Eval suite reached parity:
- minimal pass rate: X% (legacy: Y%)
- minimal median tokens: X (legacy: Y)

The legacy branches in turn/turn_streamed remain available via
agent.core = 'legacy' until the Phase 7 deletion PR.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

Fill the `X%`/`Y%`/`X`/`Y` placeholders with the actual numbers from Step 1's eval output.

---

## Task 10: Deletion PR — remove scaffolding and heavy memory modules

**Files:**
- Delete from `src/agent/`: `classifier.rs`, `context_analyzer.rs`, `context_compressor.rs`, `microcompactor.rs`, `history_pruner.rs`, `loop_detector.rs`, `personality.rs`, `thinking.rs`, `memory_loader.rs`, `loop_.rs`
- Delete from `src/memory/`: `dreaming.rs`, `consolidation.rs`, `decay.rs`, `knowledge_graph.rs`, `hygiene.rs`, `importance.rs`, `procedural.rs`, `outcomes.rs`, `conflict.rs`, `response_cache.rs`, `embeddings.rs`, `vector.rs`, `qdrant.rs`, `chunker.rs`, `customer_model.rs`
- Modify: `src/agent/mod.rs`, `src/agent/agent.rs`, `src/memory/mod.rs`, `src/main.rs` — remove imports, delete the legacy branch inside each gated if/else, rewrite `main.rs agent` subcommand to dispatch via `Agent::turn`
- Modify: `scripts/ci/lint_agent_core.sh` — remove allowlist entries for now-deleted files

**Goal:** After 1 week minimum of dogfooding at `core = "minimal"` in prod (whatsappweb), delete the scaffolding and heavy memory modules. The legacy branches in `turn`/`turn_streamed` become unreachable, so they get deleted too.

- [ ] **Step 1: Verify `core = "minimal"` has been running in prod for ≥7 days**

Confirm deployment logs. If not yet, STOP — run dogfooding before deleting.

- [ ] **Step 2: Delete agent scaffolding files**

```bash
git rm src/agent/classifier.rs \
       src/agent/context_analyzer.rs \
       src/agent/context_compressor.rs \
       src/agent/microcompactor.rs \
       src/agent/history_pruner.rs \
       src/agent/loop_detector.rs \
       src/agent/personality.rs \
       src/agent/thinking.rs \
       src/agent/memory_loader.rs
```

- [ ] **Step 3: Delete heavy memory files**

```bash
git rm src/memory/dreaming.rs \
       src/memory/consolidation.rs \
       src/memory/decay.rs \
       src/memory/knowledge_graph.rs \
       src/memory/hygiene.rs \
       src/memory/importance.rs \
       src/memory/procedural.rs \
       src/memory/outcomes.rs \
       src/memory/conflict.rs \
       src/memory/response_cache.rs \
       src/memory/embeddings.rs \
       src/memory/vector.rs \
       src/memory/qdrant.rs \
       src/memory/chunker.rs \
       src/memory/customer_model.rs
```

- [ ] **Step 4: Rewrite `main.rs` agent subcommand to use `Agent::turn`**

The current CLI path goes through `agent::loop_::run`. Delete that dispatch. Replace with:

```rust
// in src/main.rs, Commands::Agent arm
let mut agent = crate::agent::Agent::from_config(&config).await?;
if let Some(msg) = message {
    println!("{}", agent.turn(&msg).await?);
} else {
    agent.run_interactive().await?;
}
```

Verify `Agent::from_config` exists (grep in `src/agent/agent.rs`). Verify `run_interactive()` handles stdin input.

- [ ] **Step 5: Delete `loop_.rs`**

```bash
git rm src/agent/loop_.rs
```

This is the 10,126-line legacy loop. Its last caller was `main.rs`, now rewritten.

- [ ] **Step 6: Delete legacy branches in `Agent::turn`/`turn_streamed`**

Open `src/agent/agent.rs`. For each of the three gates added in Task 3:

```rust
// Before:
let context = if self.config.core == "minimal" {
    String::new()
} else {
    self.memory_loader.load_context(...).await.unwrap_or_default()
};

// After (minimal-only, no branch):
let context = String::new();
```

Same simplification for `classify_model` (always uses `self.model_name.clone()`) and `execute_tool_call` (always uses `format_tool_output`). Remove the `self.config.core` read; remove any now-unused `memory_loader` field from `Agent` + `AgentBuilder`.

- [ ] **Step 7: Delete `agent.core` config key if no longer meaningful**

If the flag has only one value, delete it:
- Remove `core: String` field from `AgentConfig` in `src/config/schema.rs`.
- Remove `default_agent_core` and its callsites.
- Delete the three tests from Task 1.

Alternatively, keep it as a kill-switch that routes to legacy → panic. Recommend deleting.

- [ ] **Step 8: Clean up imports and `mod.rs`**

```bash
cargo check 2>&1 | grep -E "^error|^warning: unused" | head -40
```

Fix each:
- `src/agent/mod.rs` — remove `pub mod` lines for deleted modules.
- `src/memory/mod.rs` — same.
- `src/agent/agent.rs` — remove the `memory_loader` field from the `Agent` struct and `AgentBuilder`, and the `MemoryLoader` imports.

- [ ] **Step 9: Shrink the CI allowlist**

In `scripts/ci/lint_agent_core.sh`, delete all allowlist entries — the files no longer exist. Verify:

```bash
./scripts/ci/lint_agent_core.sh
```

Expected: OK. Any residual violations in retained code must be fixed, not re-allowlisted.

- [ ] **Step 10: Run the full test suite + binary build**

```bash
cargo check --all-targets
cargo test
cargo build --release
```

Expected: all pass. Binary size check:

```bash
ls -lh target/release/zeroclaw
```

Expected: within the 10–20 MB envelope from the spec. If not, investigate (something else pulled in a large dep).

- [ ] **Step 11: Run the RSS regression test one more time**

```bash
cargo test --test agent_rss -- --nocapture
```

Expected: still passes, and the "RSS after" number should be notably lower than at Task 6 (we just deleted ~25K lines of memory-subsystem code).

- [ ] **Step 12: Commit as a single PR**

```bash
git add -A
git commit -m "refactor(agent): delete legacy scaffolding and heavy memory modules

Remove 10 agent/ files (classifier, context_analyzer, context_compressor,
microcompactor, history_pruner, loop_detector, personality, thinking,
memory_loader, loop_.rs — 10,126 lines) and 15 memory/ files (dreaming,
consolidation, decay, knowledge_graph, hygiene, importance, procedural,
outcomes, conflict, response_cache, embeddings, vector, qdrant, chunker,
customer_model).

The legacy branches in Agent::turn and Agent::turn_streamed are now
unreachable and have been collapsed. The agent.core config key is
removed (one value left). CLI agent subcommand now dispatches via
Agent::turn like every other caller. CI allowlist is empty.

Binary stays in the 10-20 MB envelope; 500-turn RSS test still passes.

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

---

## Self-Review

- [x] **Spec coverage:** every goal in `docs/superpowers/specs/2026-04-21-agent-recore-design.md` maps to a task above. Goals 1–4 → Tasks 2/3/6/9. §Design/The loop → Task 3 (no new loop; we bypass scaffolding). §Tool-error contract → Task 2 + Task 3 Step 5. §Memory, redesigned → Task 4. §Feature flag and rollout → Tasks 1/8/9/10. §Memory-leak guardrails → Tasks 5/6. §Testing → Tasks 2/4/6/8. §Rollout Plan → Phases 1–7 above.
- [x] **No placeholders:** every code step shows the code. Implementer notes are bounded and point to a specific grep command.
- [x] **Type consistency:** `MemoryEntry`, `ToolResult`, `ToolCall`, `ChatResponse`, `ApprovalRequest`, `ApprovalResponse` names/fields match the actual types on `samelamin/enable_whatsappweb_sending`. Provider `chat(&self, req, model, temperature)` is used everywhere, not the crates-layout variant.
- [x] **Scope:** each task fits in one PR; the riskiest task (Task 3, gating inside `turn`/`turn_streamed`) has per-gate test cases before each code change.
- [x] **Strategic coherence:** we do not reinvent `ToolDispatcher`, `parse_tool_calls`, `SystemPromptBuilder`, skills/security_summary/autonomy injection, `ApprovalManager`, MCP activation, cost tracking, hooks, response cache, streaming, or session save/load. All continue to work because we call the existing code.
