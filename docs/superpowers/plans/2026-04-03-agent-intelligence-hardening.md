# Agent Intelligence Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Harden agent intelligence with circuit breakers, enhanced extended thinking (including MiniMax inline reasoning), conversation checkpointing for multi-step workflows, and sub-task planning for multi-agent orchestration.

**Architecture:** Four features built incrementally. Circuit breaker adds failure tracking to the existing ToolExecutor. Extended thinking wires ThinkingLevel to provider-specific APIs (Anthropic budget_tokens, OpenAI reasoning models, MiniMax `<think>` extraction). Conversation checkpointing adds SQLite snapshots at configurable points in the channel message pipeline. Sub-task planning adds a TaskPlanner tool that decomposes complex prompts into dependency-ordered sub-tasks before delegation.

**Tech Stack:** Rust, tokio, rusqlite, serde_json. No new crates.

**MiniMax Note:** Naseyma defaults to MiniMax which uses OpenAI-compatible API with `merge_system_into_user`, prompt-guided (XML) tool calling, and inline `<think>` reasoning blocks. All features must work with this provider.

---

### Task 1: Circuit Breaker for Tool Executor

**Files:**
- Modify: `src/agent/tool_executor.rs`

Add a circuit breaker that tracks per-tool failure counts and short-circuits execution when a tool is consistently failing, preventing cascading slowdowns in multi-agent workflows.

- [ ] **Step 1: Write the failing test for circuit breaker tripping**

Add to the `#[cfg(test)]` module in `src/agent/tool_executor.rs`:

```rust
#[tokio::test]
async fn circuit_breaker_trips_after_threshold() {
    let executor = ToolExecutor::new();
    let tool = FailNThenSucceed::new("web_fetch", 100); // always fails

    // Trip the circuit breaker by failing repeatedly
    for _ in 0..5 {
        let _ = executor.execute(&tool, serde_json::json!({})).await;
    }

    // Next call should be short-circuited
    let result = executor.execute(&tool, serde_json::json!({})).await;
    assert!(!result.success);
    assert!(
        result
            .error
            .as_ref()
            .unwrap()
            .contains("circuit breaker open"),
        "Expected circuit breaker error, got: {:?}",
        result.error
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p zeroclaw circuit_breaker_trips -- --nocapture 2>&1 | tail -20`
Expected: FAIL — no circuit breaker logic exists yet.

- [ ] **Step 3: Write the failing test for circuit breaker recovery**

Add to `src/agent/tool_executor.rs` tests:

```rust
#[tokio::test]
async fn circuit_breaker_recovers_after_window() {
    let executor = ToolExecutor::new_with_circuit_breaker(CircuitBreakerConfig {
        failure_threshold: 3,
        window_secs: 1, // 1-second window for test speed
    });
    let tool = FailNThenSucceed::new("web_fetch", 100);

    // Trip the breaker
    for _ in 0..3 {
        let _ = executor.execute(&tool, serde_json::json!({})).await;
    }

    // Should be open
    let result = executor.execute(&tool, serde_json::json!({})).await;
    assert!(result.error.as_ref().unwrap().contains("circuit breaker open"));

    // Wait for window to expire
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Should be half-open — allows one attempt
    let result = executor.execute(&tool, serde_json::json!({})).await;
    assert!(!result.error.as_ref().unwrap().contains("circuit breaker open"));
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p zeroclaw circuit_breaker_recovers -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `new_with_circuit_breaker` and `CircuitBreakerConfig` don't exist.

- [ ] **Step 5: Implement the circuit breaker**

In `src/agent/tool_executor.rs`, add the circuit breaker types and modify the executor:

```rust
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ── Circuit Breaker ────────────────────────────────────────────────

/// Per-tool circuit breaker state.
#[derive(Debug)]
struct CircuitBreakerState {
    failures: Vec<Instant>,
}

impl CircuitBreakerState {
    fn new() -> Self {
        Self {
            failures: Vec::new(),
        }
    }

    /// Record a failure and return the count within the window.
    fn record_failure(&mut self, window: Duration) -> usize {
        let now = Instant::now();
        self.failures.push(now);
        self.prune(window, now);
        self.failures.len()
    }

    /// Check if the breaker is open (too many recent failures).
    fn is_open(&mut self, threshold: usize, window: Duration) -> bool {
        self.prune(window, Instant::now());
        self.failures.len() >= threshold
    }

    fn prune(&mut self, window: Duration, now: Instant) {
        self.failures
            .retain(|t| now.duration_since(*t) < window);
    }

    /// Reset on success (half-open → closed).
    fn reset(&mut self) {
        self.failures.clear();
    }
}

/// Circuit breaker configuration.
#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    /// Number of failures within the window to trip the breaker. Default: 5.
    pub failure_threshold: usize,
    /// Time window in seconds. Default: 60.
    pub window_secs: u64,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            window_secs: 60,
        }
    }
}
```

Then modify `ToolExecutor`:

```rust
pub struct ToolExecutor {
    default_timeout: Duration,
    policies: HashMap<String, ToolRetryPolicy>,
    circuit_breakers: Arc<Mutex<HashMap<String, CircuitBreakerState>>>,
    cb_config: CircuitBreakerConfig,
}

impl ToolExecutor {
    pub fn new() -> Self {
        Self::new_with_circuit_breaker(CircuitBreakerConfig::default())
    }

    pub fn new_with_circuit_breaker(cb_config: CircuitBreakerConfig) -> Self {
        let mut policies = HashMap::new();
        // ... existing policy setup unchanged ...

        Self {
            default_timeout: Duration::from_secs(60),
            policies,
            circuit_breakers: Arc::new(Mutex::new(HashMap::new())),
            cb_config,
        }
    }
}
```

And modify `execute()` to check/update circuit breaker:

```rust
pub async fn execute(&self, tool: &dyn Tool, args: serde_json::Value) -> ToolResult {
    let tool_name = tool.name().to_string();

    // Check circuit breaker
    {
        let mut breakers = self.circuit_breakers.lock().unwrap();
        let state = breakers.entry(tool_name.clone()).or_insert_with(CircuitBreakerState::new);
        if state.is_open(self.cb_config.failure_threshold, Duration::from_secs(self.cb_config.window_secs)) {
            tracing::warn!(tool = %tool_name, "Circuit breaker open — skipping execution");
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Tool '{}' circuit breaker open — {} failures in {}s. Will retry after window expires.",
                    tool_name, self.cb_config.failure_threshold, self.cb_config.window_secs,
                )),
            };
        }
    }

    let policy = self.policies.get(&tool_name).cloned().unwrap_or_default();
    // ... existing retry loop ...

    // After the retry loop, update circuit breaker based on result:
    {
        let mut breakers = self.circuit_breakers.lock().unwrap();
        let state = breakers.entry(tool_name.clone()).or_insert_with(CircuitBreakerState::new);
        if last_result.success {
            state.reset();
        } else {
            let count = state.record_failure(Duration::from_secs(self.cb_config.window_secs));
            if count >= self.cb_config.failure_threshold {
                tracing::warn!(
                    tool = %tool_name,
                    failures = count,
                    window_secs = self.cb_config.window_secs,
                    "Circuit breaker tripped"
                );
            }
        }
    }

    last_result
}
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p zeroclaw tool_executor -- --nocapture 2>&1 | tail -30`
Expected: All tests pass including the two new circuit breaker tests.

- [ ] **Step 7: Write test for circuit breaker reset on success**

```rust
#[tokio::test]
async fn circuit_breaker_resets_on_success() {
    let executor = ToolExecutor::new_with_circuit_breaker(CircuitBreakerConfig {
        failure_threshold: 3,
        window_secs: 60,
    });
    // Fail twice (not enough to trip)
    let tool = FailNThenSucceed::new("web_fetch", 2);
    let _ = executor.execute(&tool, serde_json::json!({})).await;
    let _ = executor.execute(&tool, serde_json::json!({})).await;

    // Third call succeeds — should reset breaker
    let result = executor.execute(&tool, serde_json::json!({})).await;
    assert!(result.success);

    // New failures should start fresh count
    let tool2 = FailNThenSucceed::new("web_fetch", 2);
    let _ = executor.execute(&tool2, serde_json::json!({})).await;
    let _ = executor.execute(&tool2, serde_json::json!({})).await;
    // Only 2 failures after reset — should NOT be open
    let result = executor.execute(&tool2, serde_json::json!({})).await;
    assert!(result.success);
}
```

- [ ] **Step 8: Run all tool_executor tests**

Run: `cargo test -p zeroclaw tool_executor -- --nocapture 2>&1 | tail -30`
Expected: All tests pass.

- [ ] **Step 9: Commit**

```bash
git add src/agent/tool_executor.rs
git commit -m "feat(agent): add circuit breaker to tool executor"
```

---

### Task 2: Enhanced Extended Thinking — Anthropic Provider Wiring

**Files:**
- Modify: `src/providers/anthropic.rs`
- Modify: `src/providers/traits.rs`

Wire thinking levels to Anthropic's API: extract `reasoning_content` from responses and pass `thinking` budget parameters.

- [ ] **Step 1: Write failing test for Anthropic reasoning_content extraction**

Find the test module in `src/providers/anthropic.rs` and add:

```rust
#[test]
fn parses_reasoning_content_from_response() {
    // Anthropic returns thinking in content blocks with type "thinking"
    let response_json = serde_json::json!({
        "id": "msg_123",
        "type": "message",
        "role": "assistant",
        "content": [
            {
                "type": "thinking",
                "thinking": "Let me reason about this step by step..."
            },
            {
                "type": "text",
                "text": "The answer is 42."
            }
        ],
        "model": "claude-sonnet-4-20250514",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 50
        }
    });

    let parsed = parse_anthropic_response(&response_json);
    assert_eq!(parsed.text.as_deref(), Some("The answer is 42."));
    assert_eq!(
        parsed.reasoning_content.as_deref(),
        Some("Let me reason about this step by step...")
    );
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p zeroclaw parses_reasoning_content -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `parse_anthropic_response` doesn't extract reasoning_content.

- [ ] **Step 3: Implement reasoning_content extraction in Anthropic provider**

In `src/providers/anthropic.rs`, find the response parsing function that constructs `ChatResponse`. Modify it to scan content blocks for `type: "thinking"`:

```rust
// When parsing the Anthropic response content array:
let mut text_parts: Vec<String> = Vec::new();
let mut reasoning_parts: Vec<String> = Vec::new();

if let Some(content_array) = response.get("content").and_then(|c| c.as_array()) {
    for block in content_array {
        match block.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    text_parts.push(text.to_string());
                }
            }
            Some("thinking") => {
                if let Some(thinking) = block.get("thinking").and_then(|t| t.as_str()) {
                    reasoning_parts.push(thinking.to_string());
                }
            }
            Some("tool_use") => { /* existing tool_use handling */ }
            _ => {}
        }
    }
}

let reasoning_content = if reasoning_parts.is_empty() {
    None
} else {
    Some(reasoning_parts.join("\n"))
};

ChatResponse {
    text: if text_parts.is_empty() { None } else { Some(text_parts.join("")) },
    tool_calls,
    usage,
    reasoning_content,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p zeroclaw parses_reasoning_content -- --nocapture 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Add ThinkingLevel to ChatRequest**

In `src/providers/traits.rs`, add the thinking level field to `ChatRequest`:

```rust
use crate::agent::thinking::ThinkingLevel;

pub struct ChatRequest<'a> {
    pub messages: &'a [ChatMessage],
    pub tools: Option<&'a [ToolSpec]>,
    /// Optional thinking level for providers that support extended thinking.
    pub thinking_level: Option<ThinkingLevel>,
}
```

Update all `ChatRequest` construction sites to add `thinking_level: None` (search for `ChatRequest {` in the codebase). This is a non-breaking change — existing callers just pass `None`.

- [ ] **Step 6: Run full test suite to verify no breakage**

Run: `cargo test -p zeroclaw 2>&1 | tail -20`
Expected: All tests pass. The `thinking_level: None` additions are backward-compatible.

- [ ] **Step 7: Commit**

```bash
git add src/providers/anthropic.rs src/providers/traits.rs
git commit -m "feat(providers): extract reasoning_content from Anthropic responses and add ThinkingLevel to ChatRequest"
```

---

### Task 3: Enhanced Extended Thinking — MiniMax & Compatible Provider Improvements

**Files:**
- Modify: `src/providers/compatible.rs`

Improve MiniMax's inline `<think>` block handling: extract reasoning into `reasoning_content` field instead of silently stripping it, so the agent loop can use it.

- [ ] **Step 1: Write failing test for MiniMax reasoning extraction**

Add to the test module in `src/providers/compatible.rs`:

```rust
#[test]
fn minimax_think_blocks_extracted_as_reasoning_content() {
    let response_json = serde_json::json!({
        "choices": [{
            "message": {
                "content": "<think>Let me reason carefully about this problem.</think>The answer is 42."
            }
        }]
    });
    let msg: ApiResponse = serde_json::from_value(response_json).unwrap();
    let choice = &msg.choices[0];
    let (text, reasoning) = extract_content_and_reasoning(&choice.message);
    assert_eq!(text, "The answer is 42.");
    assert_eq!(reasoning.as_deref(), Some("Let me reason carefully about this problem."));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p zeroclaw minimax_think_blocks_extracted -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `extract_content_and_reasoning` doesn't exist.

- [ ] **Step 3: Write test for multiple think blocks**

```rust
#[test]
fn minimax_multiple_think_blocks_concatenated() {
    let response_json = serde_json::json!({
        "choices": [{
            "message": {
                "content": "<think>First thought.</think>Part A <think>Second thought.</think>Part B"
            }
        }]
    });
    let msg: ApiResponse = serde_json::from_value(response_json).unwrap();
    let choice = &msg.choices[0];
    let (text, reasoning) = extract_content_and_reasoning(&choice.message);
    assert_eq!(text, "Part A Part B");
    assert_eq!(reasoning.as_deref(), Some("First thought.\nSecond thought."));
}
```

- [ ] **Step 4: Write test for no think blocks (normal model)**

```rust
#[test]
fn normal_model_no_think_blocks_no_reasoning() {
    let response_json = serde_json::json!({
        "choices": [{
            "message": {
                "content": "Just a normal response."
            }
        }]
    });
    let msg: ApiResponse = serde_json::from_value(response_json).unwrap();
    let choice = &msg.choices[0];
    let (text, reasoning) = extract_content_and_reasoning(&choice.message);
    assert_eq!(text, "Just a normal response.");
    assert!(reasoning.is_none());
}
```

- [ ] **Step 5: Implement `extract_content_and_reasoning`**

In `src/providers/compatible.rs`, replace the existing `strip_think_tags()` function with a richer extraction:

```rust
/// Extract visible text and reasoning content from a model response.
///
/// Models like MiniMax embed chain-of-thought in `<think>...</think>` blocks
/// inline in the content field. This function separates them:
/// - Returns (visible_text, Some(reasoning)) if think blocks found
/// - Returns (original_text, None) if no think blocks
fn extract_content_and_reasoning(message: &ResponseMessage) -> (String, Option<String>) {
    let raw = message.effective_content();
    if !raw.contains("<think>") {
        return (raw, None);
    }

    let mut visible_parts: Vec<&str> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut rest = raw.as_str();

    while let Some(start) = rest.find("<think>") {
        // Text before the think tag is visible
        let before = rest[..start].trim();
        if !before.is_empty() {
            visible_parts.push(before);
        }

        let after_open = &rest[start + "<think>".len()..];
        if let Some(end) = after_open.find("</think>") {
            let thinking = &after_open[..end];
            if !thinking.trim().is_empty() {
                reasoning_parts.push(thinking.trim().to_string());
            }
            rest = &after_open[end + "</think>".len()..];
        } else {
            // Unclosed think tag — treat remainder as reasoning
            let thinking = after_open.trim();
            if !thinking.is_empty() {
                reasoning_parts.push(thinking.to_string());
            }
            rest = "";
            break;
        }
    }

    // Remaining text after last think block
    let remaining = rest.trim();
    if !remaining.is_empty() {
        visible_parts.push(remaining);
    }

    let text = visible_parts.join(" ");
    let reasoning = if reasoning_parts.is_empty() {
        None
    } else {
        Some(reasoning_parts.join("\n"))
    };

    (text, reasoning)
}
```

Then update all call sites that currently call `strip_think_tags()` to use `extract_content_and_reasoning()` instead, passing the extracted `reasoning` into `ChatResponse.reasoning_content`.

Find the response construction in the non-streaming path and update:

```rust
// OLD:
// let text = strip_think_tags(&message.effective_content());
// ChatResponse { text: Some(text), ..., reasoning_content: None }

// NEW:
let (text, reasoning) = extract_content_and_reasoning(&message);
ChatResponse { text: Some(text), ..., reasoning_content: reasoning }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p zeroclaw minimax_think_blocks -- --nocapture 2>&1 | tail -20`
Run: `cargo test -p zeroclaw normal_model_no_think -- --nocapture 2>&1 | tail -20`
Expected: All pass.

- [ ] **Step 7: Run full test suite to check for regressions**

Run: `cargo test -p zeroclaw 2>&1 | tail -20`
Expected: All tests pass. Existing `strip_think_tags` tests should be migrated to use the new function.

- [ ] **Step 8: Commit**

```bash
git add src/providers/compatible.rs
git commit -m "feat(providers): extract MiniMax think blocks into reasoning_content instead of stripping"
```

---

### Task 4: Conversation Checkpointing — Storage Layer

**Files:**
- Create: `src/agent/checkpoint.rs`
- Modify: `src/agent/mod.rs`

Add the checkpoint storage layer with SQLite persistence. This is the foundation for conversation branching.

- [ ] **Step 1: Write the failing test for checkpoint storage**

Create `src/agent/checkpoint.rs` with tests first:

```rust
//! Conversation checkpoint storage for multi-step workflows.
//!
//! Provides snapshot/restore of conversation state at configurable points.
//! Used by channel pipelines to enable branching back to earlier states
//! (e.g., Naseyma's 6-step approval workflow).

use crate::providers::traits::ChatMessage;
use serde::{Deserialize, Serialize};

/// A snapshot of conversation state at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique checkpoint ID (UUID).
    pub id: String,
    /// Session identifier (e.g., channel_sender_key).
    pub session_id: String,
    /// Human-readable label (e.g., "step-3-design-approved").
    pub label: Option<String>,
    /// The full conversation history at this point.
    pub history: Vec<ChatMessage>,
    /// Number of turns in the history.
    pub turn_count: usize,
    /// Optional metadata (JSON object).
    pub metadata: Option<serde_json::Value>,
    /// Creation timestamp (RFC 3339).
    pub created_at: String,
}

/// Storage backend for checkpoints.
#[async_trait::async_trait]
pub trait CheckpointStore: Send + Sync {
    /// Save a checkpoint. Returns the checkpoint ID.
    async fn save(&self, checkpoint: &Checkpoint) -> anyhow::Result<String>;
    /// Load a checkpoint by ID.
    async fn load(&self, id: &str) -> anyhow::Result<Option<Checkpoint>>;
    /// List checkpoints for a session, ordered by creation time (newest first).
    async fn list(&self, session_id: &str) -> anyhow::Result<Vec<Checkpoint>>;
    /// Delete a checkpoint by ID.
    async fn delete(&self, id: &str) -> anyhow::Result<bool>;
    /// Delete all checkpoints for a session.
    async fn clear_session(&self, session_id: &str) -> anyhow::Result<usize>;
}

/// In-memory checkpoint store for testing.
pub struct InMemoryCheckpointStore {
    checkpoints: std::sync::Mutex<Vec<Checkpoint>>,
}

impl InMemoryCheckpointStore {
    pub fn new() -> Self {
        Self {
            checkpoints: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl CheckpointStore for InMemoryCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> anyhow::Result<String> {
        let id = checkpoint.id.clone();
        let mut store = self.checkpoints.lock().unwrap();
        // Replace if same ID exists
        store.retain(|c| c.id != id);
        store.push(checkpoint.clone());
        Ok(id)
    }

    async fn load(&self, id: &str) -> anyhow::Result<Option<Checkpoint>> {
        let store = self.checkpoints.lock().unwrap();
        Ok(store.iter().find(|c| c.id == id).cloned())
    }

    async fn list(&self, session_id: &str) -> anyhow::Result<Vec<Checkpoint>> {
        let store = self.checkpoints.lock().unwrap();
        let mut results: Vec<Checkpoint> = store
            .iter()
            .filter(|c| c.session_id == session_id)
            .cloned()
            .collect();
        results.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(results)
    }

    async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let mut store = self.checkpoints.lock().unwrap();
        let len_before = store.len();
        store.retain(|c| c.id != id);
        Ok(store.len() < len_before)
    }

    async fn clear_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let mut store = self.checkpoints.lock().unwrap();
        let len_before = store.len();
        store.retain(|c| c.session_id != session_id);
        Ok(len_before - store.len())
    }
}

/// Create a new checkpoint from the current conversation state.
pub fn create_checkpoint(
    session_id: &str,
    label: Option<&str>,
    history: &[ChatMessage],
    metadata: Option<serde_json::Value>,
) -> Checkpoint {
    Checkpoint {
        id: uuid::Uuid::new_v4().to_string(),
        session_id: session_id.to_string(),
        label: label.map(|s| s.to_string()),
        history: history.to_vec(),
        turn_count: history.iter().filter(|m| m.role == "user").count(),
        metadata,
        created_at: chrono::Utc::now().to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_history() -> Vec<ChatMessage> {
        vec![
            ChatMessage::system("You are helpful."),
            ChatMessage::user("Hello"),
            ChatMessage::assistant("Hi there!"),
        ]
    }

    #[tokio::test]
    async fn save_and_load_checkpoint() {
        let store = InMemoryCheckpointStore::new();
        let cp = create_checkpoint("session-1", Some("step-1"), &sample_history(), None);
        let id = store.save(&cp).await.unwrap();

        let loaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.session_id, "session-1");
        assert_eq!(loaded.label.as_deref(), Some("step-1"));
        assert_eq!(loaded.history.len(), 3);
        assert_eq!(loaded.turn_count, 1); // 1 user message
    }

    #[tokio::test]
    async fn list_checkpoints_ordered_newest_first() {
        let store = InMemoryCheckpointStore::new();
        let h = sample_history();

        let cp1 = Checkpoint {
            id: "a".to_string(),
            session_id: "s1".to_string(),
            label: Some("step-1".to_string()),
            history: h.clone(),
            turn_count: 1,
            metadata: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let cp2 = Checkpoint {
            id: "b".to_string(),
            session_id: "s1".to_string(),
            label: Some("step-2".to_string()),
            history: h.clone(),
            turn_count: 1,
            metadata: None,
            created_at: "2026-01-02T00:00:00Z".to_string(),
        };
        store.save(&cp1).await.unwrap();
        store.save(&cp2).await.unwrap();

        let list = store.list("s1").await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "b"); // newest first
        assert_eq!(list[1].id, "a");
    }

    #[tokio::test]
    async fn delete_checkpoint() {
        let store = InMemoryCheckpointStore::new();
        let cp = create_checkpoint("s1", None, &sample_history(), None);
        let id = store.save(&cp).await.unwrap();
        assert!(store.delete(&id).await.unwrap());
        assert!(store.load(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn clear_session_removes_all() {
        let store = InMemoryCheckpointStore::new();
        for i in 0..5 {
            let cp = create_checkpoint("s1", Some(&format!("step-{i}")), &sample_history(), None);
            store.save(&cp).await.unwrap();
        }
        let cp_other = create_checkpoint("s2", None, &sample_history(), None);
        store.save(&cp_other).await.unwrap();

        let removed = store.clear_session("s1").await.unwrap();
        assert_eq!(removed, 5);
        assert_eq!(store.list("s1").await.unwrap().len(), 0);
        assert_eq!(store.list("s2").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_nonexistent_returns_false() {
        let store = InMemoryCheckpointStore::new();
        assert!(!store.delete("nonexistent").await.unwrap());
    }
}
```

- [ ] **Step 2: Register the module**

In `src/agent/mod.rs`, add:
```rust
pub mod checkpoint;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p zeroclaw checkpoint -- --nocapture 2>&1 | tail -30`
Expected: All 5 tests pass. The InMemoryCheckpointStore is self-contained.

- [ ] **Step 4: Commit**

```bash
git add src/agent/checkpoint.rs src/agent/mod.rs
git commit -m "feat(agent): add conversation checkpoint storage layer with in-memory backend"
```

---

### Task 5: Conversation Checkpointing — SQLite Backend

**Files:**
- Modify: `src/agent/checkpoint.rs`
- Modify: `src/memory/sqlite.rs`

Add SQLite-backed checkpoint storage that persists across restarts.

- [ ] **Step 1: Write failing test for SQLite checkpoint store**

Add to `src/agent/checkpoint.rs`:

```rust
/// SQLite-backed checkpoint store.
pub struct SqliteCheckpointStore {
    conn: std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>,
}

impl SqliteCheckpointStore {
    pub fn new(conn: std::sync::Arc<std::sync::Mutex<rusqlite::Connection>>) -> anyhow::Result<Self> {
        {
            let db = conn.lock().unwrap();
            db.execute_batch(
                "CREATE TABLE IF NOT EXISTS checkpoints (
                    id          TEXT PRIMARY KEY,
                    session_id  TEXT NOT NULL,
                    label       TEXT,
                    history     TEXT NOT NULL,
                    turn_count  INTEGER NOT NULL,
                    metadata    TEXT,
                    created_at  TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_checkpoints_session
                    ON checkpoints(session_id);
                CREATE INDEX IF NOT EXISTS idx_checkpoints_created
                    ON checkpoints(created_at);",
            )?;
        }
        Ok(Self { conn })
    }
}
```

And the tests:

```rust
#[cfg(test)]
mod sqlite_tests {
    use super::*;

    fn test_db() -> std::sync::Arc<std::sync::Mutex<rusqlite::Connection>> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        std::sync::Arc::new(std::sync::Mutex::new(conn))
    }

    #[tokio::test]
    async fn sqlite_save_and_load() {
        let store = SqliteCheckpointStore::new(test_db()).unwrap();
        let cp = create_checkpoint("s1", Some("step-1"), &sample_history(), None);
        let id = store.save(&cp).await.unwrap();
        let loaded = store.load(&id).await.unwrap().unwrap();
        assert_eq!(loaded.label.as_deref(), Some("step-1"));
        assert_eq!(loaded.history.len(), 3);
    }

    #[tokio::test]
    async fn sqlite_list_ordered() {
        let store = SqliteCheckpointStore::new(test_db()).unwrap();
        let h = sample_history();
        for (i, ts) in ["2026-01-01T00:00:00Z", "2026-01-03T00:00:00Z", "2026-01-02T00:00:00Z"].iter().enumerate() {
            let cp = Checkpoint {
                id: format!("cp-{i}"),
                session_id: "s1".to_string(),
                label: None,
                history: h.clone(),
                turn_count: 1,
                metadata: None,
                created_at: ts.to_string(),
            };
            store.save(&cp).await.unwrap();
        }
        let list = store.list("s1").await.unwrap();
        assert_eq!(list[0].id, "cp-1"); // 2026-01-03 (newest)
        assert_eq!(list[1].id, "cp-2"); // 2026-01-02
        assert_eq!(list[2].id, "cp-0"); // 2026-01-01
    }

    #[tokio::test]
    async fn sqlite_delete_and_clear() {
        let store = SqliteCheckpointStore::new(test_db()).unwrap();
        for i in 0..3 {
            let cp = create_checkpoint("s1", None, &sample_history(), None);
            store.save(&Checkpoint { id: format!("cp-{i}"), ..cp }).await.unwrap();
        }
        assert!(store.delete("cp-1").await.unwrap());
        assert_eq!(store.list("s1").await.unwrap().len(), 2);
        let cleared = store.clear_session("s1").await.unwrap();
        assert_eq!(cleared, 2);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p zeroclaw sqlite_save_and_load -- --nocapture 2>&1 | tail -20`
Expected: FAIL — `SqliteCheckpointStore` trait impl doesn't exist yet.

- [ ] **Step 3: Implement SqliteCheckpointStore**

```rust
#[async_trait::async_trait]
impl CheckpointStore for SqliteCheckpointStore {
    async fn save(&self, checkpoint: &Checkpoint) -> anyhow::Result<String> {
        let id = checkpoint.id.clone();
        let session_id = checkpoint.session_id.clone();
        let label = checkpoint.label.clone();
        let history = serde_json::to_string(&checkpoint.history)?;
        let turn_count = checkpoint.turn_count as i64;
        let metadata = checkpoint
            .metadata
            .as_ref()
            .map(|m| serde_json::to_string(m))
            .transpose()?;
        let created_at = checkpoint.created_at.clone();

        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            db.execute(
                "INSERT OR REPLACE INTO checkpoints
                 (id, session_id, label, history, turn_count, metadata, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![id, session_id, label, history, turn_count, metadata, created_at],
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;

        Ok(checkpoint.id.clone())
    }

    async fn load(&self, id: &str) -> anyhow::Result<Option<Checkpoint>> {
        let id = id.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            let mut stmt = db.prepare(
                "SELECT id, session_id, label, history, turn_count, metadata, created_at
                 FROM checkpoints WHERE id = ?1",
            )?;
            let result = stmt
                .query_row(rusqlite::params![id], |row| {
                    Ok(CheckpointRow {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        label: row.get(2)?,
                        history: row.get(3)?,
                        turn_count: row.get::<_, i64>(4)?,
                        metadata: row.get(5)?,
                        created_at: row.get(6)?,
                    })
                })
                .optional()?;

            match result {
                Some(row) => {
                    let history: Vec<ChatMessage> = serde_json::from_str(&row.history)?;
                    let metadata: Option<serde_json::Value> = row
                        .metadata
                        .as_deref()
                        .map(serde_json::from_str)
                        .transpose()?;
                    Ok(Some(Checkpoint {
                        id: row.id,
                        session_id: row.session_id,
                        label: row.label,
                        history,
                        turn_count: row.turn_count as usize,
                        metadata,
                        created_at: row.created_at,
                    }))
                }
                None => Ok(None),
            }
        })
        .await?
    }

    async fn list(&self, session_id: &str) -> anyhow::Result<Vec<Checkpoint>> {
        let session_id = session_id.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            let mut stmt = db.prepare(
                "SELECT id, session_id, label, history, turn_count, metadata, created_at
                 FROM checkpoints WHERE session_id = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt
                .query_map(rusqlite::params![session_id], |row| {
                    Ok(CheckpointRow {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        label: row.get(2)?,
                        history: row.get(3)?,
                        turn_count: row.get::<_, i64>(4)?,
                        metadata: row.get(5)?,
                        created_at: row.get(6)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let mut checkpoints = Vec::with_capacity(rows.len());
            for row in rows {
                let history: Vec<ChatMessage> = serde_json::from_str(&row.history)?;
                let metadata: Option<serde_json::Value> = row
                    .metadata
                    .as_deref()
                    .map(serde_json::from_str)
                    .transpose()?;
                checkpoints.push(Checkpoint {
                    id: row.id,
                    session_id: row.session_id,
                    label: row.label,
                    history,
                    turn_count: row.turn_count as usize,
                    metadata,
                    created_at: row.created_at,
                });
            }
            Ok(checkpoints)
        })
        .await?
    }

    async fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let id = id.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            let affected = db.execute("DELETE FROM checkpoints WHERE id = ?1", rusqlite::params![id])?;
            Ok(affected > 0)
        })
        .await?
    }

    async fn clear_session(&self, session_id: &str) -> anyhow::Result<usize> {
        let session_id = session_id.to_string();
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || {
            let db = conn.lock().unwrap();
            let affected = db.execute(
                "DELETE FROM checkpoints WHERE session_id = ?1",
                rusqlite::params![session_id],
            )?;
            Ok(affected)
        })
        .await?
    }
}

/// Internal helper for SQLite row mapping.
struct CheckpointRow {
    id: String,
    session_id: String,
    label: Option<String>,
    history: String,
    turn_count: i64,
    metadata: Option<String>,
    created_at: String,
}
```

Add `use rusqlite::OptionalExtension;` at the top of the file.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p zeroclaw checkpoint -- --nocapture 2>&1 | tail -30`
Expected: All tests pass (both InMemory and SQLite).

- [ ] **Step 5: Commit**

```bash
git add src/agent/checkpoint.rs
git commit -m "feat(agent): add SQLite-backed conversation checkpoint storage"
```

---

### Task 6: Conversation Checkpointing — Channel Pipeline Integration

**Files:**
- Modify: `src/channels/mod.rs`
- Modify: `src/config/schema.rs`

Wire checkpoint save/restore into the channel message pipeline. Auto-checkpoint on configurable triggers.

- [ ] **Step 1: Add checkpoint config to schema**

In `src/config/schema.rs`, find `AgentConfig` and add:

```rust
/// Conversation checkpointing configuration.
#[serde(default)]
pub checkpoint: CheckpointConfig,
```

And define the config struct:

```rust
fn default_checkpoint_enabled() -> bool { false }
fn default_checkpoint_auto_interval() -> usize { 0 }
fn default_checkpoint_max_per_session() -> usize { 50 }

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CheckpointConfig {
    /// Enable conversation checkpointing. Default: false.
    #[serde(default = "default_checkpoint_enabled")]
    pub enabled: bool,
    /// Auto-checkpoint every N user messages (0 = disabled). Default: 0.
    #[serde(default = "default_checkpoint_auto_interval")]
    pub auto_interval: usize,
    /// Maximum checkpoints per session before pruning oldest. Default: 50.
    #[serde(default = "default_checkpoint_max_per_session")]
    pub max_per_session: usize,
}

impl Default for CheckpointConfig {
    fn default() -> Self {
        Self {
            enabled: default_checkpoint_enabled(),
            auto_interval: default_checkpoint_auto_interval(),
            max_per_session: default_checkpoint_max_per_session(),
        }
    }
}
```

- [ ] **Step 2: Add `/save` and `/restore` channel commands**

In `src/channels/mod.rs`, find the directive parsing section (where `/new`, `/model`, etc. are handled) and add:

```rust
// /save [label] — checkpoint current conversation
if trimmed_content.starts_with("/save") {
    let label = trimmed_content.strip_prefix("/save").map(|s| s.trim()).filter(|s| !s.is_empty());
    if let Some(ref checkpoint_store) = ctx.checkpoint_store {
        let cp = crate::agent::checkpoint::create_checkpoint(
            &history_key,
            label,
            &cached_turns,
            None,
        );
        match checkpoint_store.save(&cp).await {
            Ok(id) => {
                let label_str = label.unwrap_or("(unnamed)");
                return Some(format!("Checkpoint saved: {label_str} (ID: {id})"));
            }
            Err(e) => {
                tracing::error!("Failed to save checkpoint: {e}");
                return Some("Failed to save checkpoint.".to_string());
            }
        }
    } else {
        return Some("Checkpointing is not enabled.".to_string());
    }
}

// /restore [id_or_label] — restore to a checkpoint
if trimmed_content.starts_with("/restore") {
    let arg = trimmed_content.strip_prefix("/restore").map(|s| s.trim()).filter(|s| !s.is_empty());
    if let Some(ref checkpoint_store) = ctx.checkpoint_store {
        // Find by label or ID
        let checkpoint = if let Some(arg) = arg {
            let by_id = checkpoint_store.load(arg).await.ok().flatten();
            if by_id.is_some() {
                by_id
            } else {
                // Search by label
                let list = checkpoint_store.list(&history_key).await.unwrap_or_default();
                list.into_iter().find(|c| c.label.as_deref() == Some(arg))
            }
        } else {
            // No arg — restore most recent
            checkpoint_store.list(&history_key).await.ok()
                .and_then(|list| list.into_iter().next())
        };

        if let Some(cp) = checkpoint {
            // Replace conversation history
            let mut history_map = ctx.conversation_history.lock().unwrap();
            history_map.put(history_key.clone(), cp.history);
            let label = cp.label.as_deref().unwrap_or("unnamed");
            return Some(format!("Restored checkpoint: {label} (turn {turn_count})", turn_count = cp.turn_count));
        } else {
            return Some("No matching checkpoint found.".to_string());
        }
    } else {
        return Some("Checkpointing is not enabled.".to_string());
    }
}

// /checkpoints — list saved checkpoints
if trimmed_content == "/checkpoints" {
    if let Some(ref checkpoint_store) = ctx.checkpoint_store {
        let list = checkpoint_store.list(&history_key).await.unwrap_or_default();
        if list.is_empty() {
            return Some("No checkpoints saved.".to_string());
        }
        let mut lines = vec!["Saved checkpoints:".to_string()];
        for cp in &list {
            let label = cp.label.as_deref().unwrap_or("(unnamed)");
            lines.push(format!("  {} — {} (turns: {})", cp.id[..8].to_string(), label, cp.turn_count));
        }
        return Some(lines.join("\n"));
    } else {
        return Some("Checkpointing is not enabled.".to_string());
    }
}
```

- [ ] **Step 3: Add checkpoint_store to ChannelRuntimeContext**

In `src/channels/mod.rs`, find the `ChannelRuntimeContext` struct and add:

```rust
pub checkpoint_store: Option<Arc<dyn crate::agent::checkpoint::CheckpointStore>>,
```

Update construction sites to pass `checkpoint_store: None` (or the actual store when configured).

- [ ] **Step 4: Add auto-checkpoint after configurable interval**

In the channel message processing section, after the LLM response and history append:

```rust
// Auto-checkpoint if configured
if let (Some(ref store), interval) = (&ctx.checkpoint_store, ctx.config.agent.checkpoint.auto_interval) {
    if interval > 0 {
        let user_count = cached_turns.iter().filter(|m| m.role == "user").count();
        if user_count > 0 && user_count % interval == 0 {
            let cp = crate::agent::checkpoint::create_checkpoint(
                &history_key,
                Some(&format!("auto-turn-{user_count}")),
                &cached_turns,
                None,
            );
            if let Err(e) = store.save(&cp).await {
                tracing::warn!("Auto-checkpoint failed: {e}");
            } else {
                tracing::debug!(user_count, "Auto-checkpoint saved");
            }
        }
    }
}
```

- [ ] **Step 5: Fix any test struct literals missing the new checkpoint field**

Search for `CheckpointConfig` or `AgentConfig {` in test files and add the missing `checkpoint: CheckpointConfig::default()` field.

Run: `cargo test -p zeroclaw 2>&1 | grep "missing field" | head -20`

Fix all missing field errors.

- [ ] **Step 6: Run full test suite**

Run: `cargo test -p zeroclaw 2>&1 | tail -20`
Expected: All tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/channels/mod.rs src/config/schema.rs src/agent/checkpoint.rs
git commit -m "feat(channels): wire conversation checkpointing with /save, /restore, /checkpoints commands"
```

---

### Task 7: Sub-Task Planning — Task Planner Tool

**Files:**
- Create: `src/tools/task_planner.rs`
- Modify: `src/tools/mod.rs`

Add a `task_plan` tool that decomposes complex prompts into ordered sub-tasks with dependencies, designed for multi-agent delegation.

- [ ] **Step 1: Write failing tests for task plan data structures**

Create `src/tools/task_planner.rs`:

```rust
//! Sub-task planning tool for multi-agent orchestration.
//!
//! Decomposes complex prompts into dependency-ordered sub-tasks that can be
//! dispatched via the delegate tool. Produces a TaskPlan with a DAG of
//! sub-tasks, each assigned to a specific agent.

use serde::{Deserialize, Serialize};

/// A single sub-task in a plan.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SubTask {
    /// Unique task identifier (e.g., "task-1").
    pub id: String,
    /// Human-readable description of what this task does.
    pub description: String,
    /// Which agent should handle this task.
    pub agent: String,
    /// The prompt to send to the agent.
    pub prompt: String,
    /// IDs of tasks that must complete before this one starts.
    pub depends_on: Vec<String>,
    /// Priority (higher = more important). Default: 0.
    pub priority: i32,
}

/// A complete execution plan with dependency ordering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskPlan {
    /// Goal/summary of the overall plan.
    pub goal: String,
    /// Ordered list of sub-tasks.
    pub tasks: Vec<SubTask>,
}

impl TaskPlan {
    /// Validate the plan: check for cycles, missing deps, and empty tasks.
    pub fn validate(&self) -> Result<(), String> {
        if self.tasks.is_empty() {
            return Err("Plan has no tasks".to_string());
        }

        let task_ids: std::collections::HashSet<&str> =
            self.tasks.iter().map(|t| t.id.as_str()).collect();

        // Check for missing dependencies
        for task in &self.tasks {
            for dep in &task.depends_on {
                if !task_ids.contains(dep.as_str()) {
                    return Err(format!(
                        "Task '{}' depends on '{}' which doesn't exist",
                        task.id, dep
                    ));
                }
            }
        }

        // Check for self-dependencies
        for task in &self.tasks {
            if task.depends_on.contains(&task.id) {
                return Err(format!("Task '{}' depends on itself", task.id));
            }
        }

        // Topological sort to detect cycles
        if self.topological_order().is_none() {
            return Err("Plan contains a dependency cycle".to_string());
        }

        Ok(())
    }

    /// Return tasks in topological order (dependencies first). Returns None if cyclic.
    pub fn topological_order(&self) -> Option<Vec<&SubTask>> {
        let mut in_degree: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut adj: std::collections::HashMap<&str, Vec<&str>> = std::collections::HashMap::new();
        let task_map: std::collections::HashMap<&str, &SubTask> =
            self.tasks.iter().map(|t| (t.id.as_str(), t)).collect();

        for task in &self.tasks {
            in_degree.entry(task.id.as_str()).or_insert(0);
            for dep in &task.depends_on {
                adj.entry(dep.as_str())
                    .or_default()
                    .push(task.id.as_str());
                *in_degree.entry(task.id.as_str()).or_insert(0) += 1;
            }
        }

        let mut queue: std::collections::VecDeque<&str> = in_degree
            .iter()
            .filter(|(_, &deg)| deg == 0)
            .map(|(&id, _)| id)
            .collect();

        let mut result: Vec<&SubTask> = Vec::new();

        while let Some(id) = queue.pop_front() {
            if let Some(task) = task_map.get(id) {
                result.push(task);
            }
            if let Some(neighbors) = adj.get(id) {
                for &next in neighbors {
                    let deg = in_degree.get_mut(next).unwrap();
                    *deg -= 1;
                    if *deg == 0 {
                        queue.push_back(next);
                    }
                }
            }
        }

        if result.len() == self.tasks.len() {
            Some(result)
        } else {
            None // cycle detected
        }
    }

    /// Get tasks that are ready to execute (all dependencies satisfied).
    pub fn ready_tasks(&self, completed: &[String]) -> Vec<&SubTask> {
        let completed_set: std::collections::HashSet<&str> =
            completed.iter().map(|s| s.as_str()).collect();
        self.tasks
            .iter()
            .filter(|t| {
                !completed_set.contains(t.id.as_str())
                    && t.depends_on
                        .iter()
                        .all(|dep| completed_set.contains(dep.as_str()))
            })
            .collect()
    }

    /// Get the execution batches (tasks that can run in parallel within each batch).
    pub fn execution_batches(&self) -> Option<Vec<Vec<&SubTask>>> {
        let order = self.topological_order()?;
        let mut completed: Vec<String> = Vec::new();
        let mut batches: Vec<Vec<&SubTask>> = Vec::new();

        while completed.len() < self.tasks.len() {
            let ready = self.ready_tasks(&completed);
            if ready.is_empty() {
                return None; // stuck — shouldn't happen after topo sort
            }
            for task in &ready {
                completed.push(task.id.clone());
            }
            batches.push(ready);
        }

        Some(batches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(id: &str, agent: &str, deps: &[&str]) -> SubTask {
        SubTask {
            id: id.to_string(),
            description: format!("Do {id}"),
            agent: agent.to_string(),
            prompt: format!("Execute {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            priority: 0,
        }
    }

    #[test]
    fn valid_linear_plan() {
        let plan = TaskPlan {
            goal: "Build website".to_string(),
            tasks: vec![
                task("research", "samia", &[]),
                task("strategy", "walied", &["research"]),
                task("design", "ruba", &["strategy"]),
            ],
        };
        assert!(plan.validate().is_ok());
    }

    #[test]
    fn valid_parallel_plan() {
        let plan = TaskPlan {
            goal: "Build website".to_string(),
            tasks: vec![
                task("research", "samia", &[]),
                task("design", "ruba", &[]),
                task("merge", "danni", &["research", "design"]),
            ],
        };
        assert!(plan.validate().is_ok());
        let batches = plan.execution_batches().unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 2); // research + design in parallel
        assert_eq!(batches[1].len(), 1); // merge after both
    }

    #[test]
    fn rejects_cycle() {
        let plan = TaskPlan {
            goal: "cycle".to_string(),
            tasks: vec![
                task("a", "x", &["b"]),
                task("b", "x", &["a"]),
            ],
        };
        assert!(plan.validate().is_err());
        assert!(plan.validate().unwrap_err().contains("cycle"));
    }

    #[test]
    fn rejects_missing_dep() {
        let plan = TaskPlan {
            goal: "missing".to_string(),
            tasks: vec![task("a", "x", &["nonexistent"])],
        };
        assert!(plan.validate().is_err());
        assert!(plan.validate().unwrap_err().contains("doesn't exist"));
    }

    #[test]
    fn rejects_self_dep() {
        let plan = TaskPlan {
            goal: "self".to_string(),
            tasks: vec![task("a", "x", &["a"])],
        };
        assert!(plan.validate().is_err());
        assert!(plan.validate().unwrap_err().contains("depends on itself"));
    }

    #[test]
    fn rejects_empty_plan() {
        let plan = TaskPlan {
            goal: "empty".to_string(),
            tasks: vec![],
        };
        assert!(plan.validate().is_err());
    }

    #[test]
    fn ready_tasks_respects_completed() {
        let plan = TaskPlan {
            goal: "test".to_string(),
            tasks: vec![
                task("a", "x", &[]),
                task("b", "x", &["a"]),
                task("c", "x", &["a"]),
                task("d", "x", &["b", "c"]),
            ],
        };
        let ready = plan.ready_tasks(&[]);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "a");

        let ready = plan.ready_tasks(&["a".to_string()]);
        assert_eq!(ready.len(), 2); // b and c
        let ids: Vec<&str> = ready.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"b"));
        assert!(ids.contains(&"c"));

        let ready = plan.ready_tasks(&["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "d");
    }

    #[test]
    fn topological_order_correct() {
        let plan = TaskPlan {
            goal: "test".to_string(),
            tasks: vec![
                task("c", "x", &["a", "b"]),
                task("a", "x", &[]),
                task("b", "x", &["a"]),
            ],
        };
        let order = plan.topological_order().unwrap();
        let ids: Vec<&str> = order.iter().map(|t| t.id.as_str()).collect();
        // a must come before b and c; b must come before c
        let pos_a = ids.iter().position(|&id| id == "a").unwrap();
        let pos_b = ids.iter().position(|&id| id == "b").unwrap();
        let pos_c = ids.iter().position(|&id| id == "c").unwrap();
        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }

    #[test]
    fn execution_batches_naseyma_workflow() {
        // Simulates Naseyma's 6-step website workflow
        let plan = TaskPlan {
            goal: "Build website for client".to_string(),
            tasks: vec![
                task("brief", "danni", &[]),
                task("research", "samia", &["brief"]),
                task("pitch", "mamoun", &["research"]),
                task("strategy", "walied", &["research"]),
                task("design", "ruba", &["strategy"]),
                task("content", "moe", &["strategy", "design"]),
                task("qa", "rayan", &["design", "content"]),
            ],
        };
        assert!(plan.validate().is_ok());
        let batches = plan.execution_batches().unwrap();
        // Batch 1: brief
        assert_eq!(batches[0].len(), 1);
        // Batch 2: research
        assert_eq!(batches[1].len(), 1);
        // Batch 3: pitch + strategy (parallel!)
        assert_eq!(batches[2].len(), 2);
        // Batch 4: design
        assert_eq!(batches[3].len(), 1);
        // Batch 5: content
        assert_eq!(batches[4].len(), 1);
        // Batch 6: qa
        assert_eq!(batches[5].len(), 1);
    }
}
```

- [ ] **Step 2: Register the module**

In `src/tools/mod.rs`, add:
```rust
pub mod task_planner;
```

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p zeroclaw task_planner -- --nocapture 2>&1 | tail -30`
Expected: All 8 tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/tools/task_planner.rs src/tools/mod.rs
git commit -m "feat(tools): add TaskPlan with dependency DAG, topological ordering, and execution batches"
```

---

### Task 8: Sub-Task Planning — Plan Executor Integration

**Files:**
- Modify: `src/tools/task_planner.rs`
- Modify: `src/tools/delegate.rs`

Add a `PlanExecutor` that runs a `TaskPlan` through the delegation system, dispatching parallel batches concurrently.

- [ ] **Step 1: Write failing test for plan execution**

Add to `src/tools/task_planner.rs`:

```rust
/// Tracks the state of plan execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanExecution {
    pub plan: TaskPlan,
    pub completed: Vec<String>,
    pub failed: Vec<(String, String)>, // (task_id, error)
    pub results: std::collections::HashMap<String, String>, // task_id → output
}

impl PlanExecution {
    pub fn new(plan: TaskPlan) -> Self {
        Self {
            plan,
            completed: Vec::new(),
            failed: Vec::new(),
            results: std::collections::HashMap::new(),
        }
    }

    /// Mark a task as completed with its result.
    pub fn complete(&mut self, task_id: &str, result: String) {
        self.completed.push(task_id.to_string());
        self.results.insert(task_id.to_string(), result);
    }

    /// Mark a task as failed with an error message.
    pub fn fail(&mut self, task_id: &str, error: String) {
        self.failed.push((task_id.to_string(), error));
    }

    /// Check if the plan is finished (all tasks completed or failed).
    pub fn is_finished(&self) -> bool {
        self.completed.len() + self.failed.len() == self.plan.tasks.len()
    }

    /// Check if the plan succeeded (all tasks completed, none failed).
    pub fn is_success(&self) -> bool {
        self.failed.is_empty() && self.completed.len() == self.plan.tasks.len()
    }

    /// Get tasks ready for execution (deps satisfied, not yet started).
    pub fn ready_tasks(&self) -> Vec<&SubTask> {
        let started: std::collections::HashSet<&str> = self
            .completed
            .iter()
            .chain(self.failed.iter().map(|(id, _)| id))
            .map(|s| s.as_str())
            .collect();

        self.plan
            .tasks
            .iter()
            .filter(|t| {
                !started.contains(t.id.as_str())
                    && t.depends_on
                        .iter()
                        .all(|dep| self.completed.contains(&dep.to_string()))
            })
            .collect()
    }

    /// Build a context-enriched prompt for a task, injecting results from dependencies.
    pub fn enriched_prompt(&self, task: &SubTask) -> String {
        let mut parts = Vec::new();
        if !task.depends_on.is_empty() {
            parts.push("Context from completed tasks:".to_string());
            for dep_id in &task.depends_on {
                if let Some(result) = self.results.get(dep_id) {
                    parts.push(format!("--- {} ---\n{}", dep_id, result));
                }
            }
            parts.push("---\n".to_string());
        }
        parts.push(task.prompt.clone());
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod execution_tests {
    use super::*;

    #[test]
    fn plan_execution_lifecycle() {
        let plan = TaskPlan {
            goal: "test".to_string(),
            tasks: vec![
                task("a", "x", &[]),
                task("b", "x", &["a"]),
            ],
        };
        let mut exec = PlanExecution::new(plan);

        assert!(!exec.is_finished());
        assert_eq!(exec.ready_tasks().len(), 1);
        assert_eq!(exec.ready_tasks()[0].id, "a");

        exec.complete("a", "result-a".to_string());
        assert!(!exec.is_finished());

        let ready = exec.ready_tasks();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "b");

        exec.complete("b", "result-b".to_string());
        assert!(exec.is_finished());
        assert!(exec.is_success());
    }

    #[test]
    fn enriched_prompt_includes_dependency_results() {
        let plan = TaskPlan {
            goal: "test".to_string(),
            tasks: vec![
                task("research", "samia", &[]),
                task("strategy", "walied", &["research"]),
            ],
        };
        let mut exec = PlanExecution::new(plan);
        exec.complete("research", "Market analysis shows...".to_string());

        let strategy_task = &exec.plan.tasks[1];
        let prompt = exec.enriched_prompt(strategy_task);
        assert!(prompt.contains("Market analysis shows..."));
        assert!(prompt.contains("Execute strategy"));
    }

    #[test]
    fn failed_task_blocks_dependents() {
        let plan = TaskPlan {
            goal: "test".to_string(),
            tasks: vec![
                task("a", "x", &[]),
                task("b", "x", &["a"]),
            ],
        };
        let mut exec = PlanExecution::new(plan);
        exec.fail("a", "timeout".to_string());

        // b depends on a which failed — should NOT be ready
        assert!(exec.ready_tasks().is_empty());
        assert!(!exec.is_success());
    }
}
```

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p zeroclaw execution_tests -- --nocapture 2>&1 | tail -20`
Expected: All pass.

- [ ] **Step 3: Commit**

```bash
git add src/tools/task_planner.rs
git commit -m "feat(tools): add PlanExecution state tracker with dependency-aware scheduling"
```

---

### Task 9: Sub-Task Planning — Delegate Integration

**Files:**
- Modify: `src/tools/delegate.rs`
- Modify: `src/tools/task_planner.rs`

Add a `plan_and_execute` action to the delegate tool that accepts a goal, generates a plan, and executes it batch-by-batch.

- [ ] **Step 1: Add plan execution format to delegate tool**

In `src/tools/delegate.rs`, add support for a new `action: "plan"` parameter. When called with `action: "plan"`, the delegate tool:

1. Takes a `goal` string and list of available `agents`
2. Uses the coordinator agent to generate a `TaskPlan` (JSON)
3. Validates the plan
4. Executes batches using parallel delegation
5. Returns aggregated results

Add to the `execute()` method's action matching:

```rust
"plan" => {
    let goal = args.get("goal")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("'goal' is required for plan action"))?;

    let available_agents: Vec<&str> = self.agents.keys().map(|s| s.as_str()).collect();

    // Ask the coordinator to generate a plan
    let plan_prompt = format!(
        "You are a task planner. Break down this goal into sub-tasks.\n\n\
         Goal: {goal}\n\n\
         Available agents: {agents}\n\n\
         Respond with a JSON TaskPlan:\n\
         {{\n  \"goal\": \"...\",\n  \"tasks\": [\n    {{\n      \"id\": \"task-1\",\n\
         \"description\": \"...\",\n      \"agent\": \"<one of the available agents>\",\n\
         \"prompt\": \"...\",\n      \"depends_on\": [],\n      \"priority\": 0\n    }}\n  ]\n}}",
        agents = available_agents.join(", ")
    );

    // Use first available agent as the planner (or a dedicated planner agent if configured)
    let planner_agent = args
        .get("planner")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| available_agents.first().copied().unwrap_or("default"));

    let plan_result = self.execute_sync(planner_agent, &plan_prompt, &args).await?;

    // Parse the plan from JSON
    let plan: crate::tools::task_planner::TaskPlan =
        serde_json::from_str(&plan_result.output)
            .map_err(|e| anyhow::anyhow!("Failed to parse plan: {e}"))?;

    plan.validate()
        .map_err(|e| anyhow::anyhow!("Invalid plan: {e}"))?;

    // Execute the plan batch by batch
    let mut execution = crate::tools::task_planner::PlanExecution::new(plan);

    while !execution.is_finished() {
        let ready = execution.ready_tasks();
        if ready.is_empty() {
            break; // stuck due to failures
        }

        // Execute ready tasks in parallel
        let mut handles = Vec::new();
        for task in &ready {
            let enriched = execution.enriched_prompt(task);
            let agent = task.agent.clone();
            let task_id = task.id.clone();
            // ... spawn parallel delegate calls
        }

        // Collect results
        // ... mark completed/failed
    }

    let summary = format!(
        "Plan execution {status}.\nCompleted: {completed}/{total}\nFailed: {failed}",
        status = if execution.is_success() { "succeeded" } else { "completed with failures" },
        completed = execution.completed.len(),
        total = execution.plan.tasks.len(),
        failed = execution.failed.len(),
    );

    Ok(ToolResult {
        success: execution.is_success(),
        output: serde_json::to_string_pretty(&execution)?,
        error: if execution.is_success() { None } else { Some(summary) },
    })
}
```

- [ ] **Step 2: Update delegate tool parameters schema**

Add `plan` to the action enum in the parameters schema:

```rust
"action": {
    "type": "string",
    "enum": ["delegate", "background", "check_result", "cancel_task", "list_results", "plan"],
    "description": "Action to perform. 'plan' generates and executes a task plan."
}
```

And add the `goal` parameter:

```rust
"goal": {
    "type": "string",
    "description": "For 'plan' action: the high-level goal to decompose into sub-tasks."
}
```

- [ ] **Step 3: Run full test suite**

Run: `cargo test -p zeroclaw 2>&1 | tail -20`
Expected: All tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/tools/delegate.rs src/tools/task_planner.rs
git commit -m "feat(tools): integrate TaskPlan execution into delegate tool with parallel batch dispatch"
```

---

### Task 10: Configuration and Integration Tests

**Files:**
- Modify: `src/config/schema.rs`

Ensure all new config fields have defaults and don't break existing test fixtures.

- [ ] **Step 1: Verify all config defaults compile**

Run: `cargo test -p zeroclaw config -- --nocapture 2>&1 | tail -30`

Fix any missing field errors in test struct literals.

- [ ] **Step 2: Run the full test suite**

Run: `cargo test -p zeroclaw 2>&1 | tail -20`
Expected: All tests pass.

- [ ] **Step 3: Run clippy**

Run: `cargo clippy -p zeroclaw 2>&1 | tail -30`
Expected: No new warnings from our changes.

- [ ] **Step 4: Run fmt**

Run: `cargo fmt -p zeroclaw -- --check 2>&1 | tail -20`
Expected: No formatting issues.

- [ ] **Step 5: Final commit if any fixups needed**

```bash
git add -A
git commit -m "fix: config defaults and test fixture updates for agent intelligence features"
```

---

## Files Changed Summary

| File | Change type | Risk |
|------|------------|------|
| `src/agent/tool_executor.rs` | Circuit breaker addition | Medium |
| `src/providers/anthropic.rs` | reasoning_content extraction | Medium |
| `src/providers/traits.rs` | ThinkingLevel on ChatRequest | Low |
| `src/providers/compatible.rs` | MiniMax think→reasoning_content | Medium |
| `src/agent/checkpoint.rs` | New file: checkpoint storage | Low |
| `src/agent/mod.rs` | Module registration | Low |
| `src/channels/mod.rs` | /save, /restore, /checkpoints, auto-checkpoint | High |
| `src/config/schema.rs` | CheckpointConfig | Low |
| `src/tools/task_planner.rs` | New file: TaskPlan DAG + PlanExecution | Low |
| `src/tools/mod.rs` | Module registration | Low |
| `src/tools/delegate.rs` | Plan action integration | Medium |

## Dependencies

No new crates. Uses existing: rusqlite, serde_json, tokio, async-trait, uuid, chrono.

## Testing Strategy

- Circuit breaker: unit tests with mock tools (trip, recover, reset)
- Anthropic reasoning: unit test parsing content blocks
- MiniMax think extraction: unit tests with inline think blocks
- Checkpoint store: unit tests with in-memory and SQLite backends
- Channel commands: integration via existing channel test harness
- Task planner: unit tests for DAG validation, topological sort, batch execution
- Plan execution: unit tests for lifecycle, enriched prompts, failure handling
