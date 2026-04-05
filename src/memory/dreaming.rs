//! Background memory dreaming — cross-session consolidation.
//!
//! Implements a three-gate dream engine that periodically consolidates Daily
//! memories into patterns, core facts, and contradiction flags via an LLM pass.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Configuration ────────────────────────────────────────────────

/// Configuration for the dream engine.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct DreamConfig {
    /// Whether the dream engine is enabled.
    pub enabled: bool,
    /// Minimum hours that must elapse between dream runs.
    pub min_hours_between_dreams: u64,
    /// Minimum sessions that must pass since the last dream.
    pub min_sessions_between_dreams: u64,
}

impl Default for DreamConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_hours_between_dreams: 24,
            min_sessions_between_dreams: 5,
        }
    }
}

// ── Dream state ──────────────────────────────────────────────────

/// Runtime state used to evaluate whether a dream pass should run.
pub struct DreamState {
    /// When the last dream was completed, or `None` if never.
    pub last_dream_at: Option<DateTime<Utc>>,
    /// How many sessions have started since the last dream.
    pub sessions_since_dream: u64,
    /// Whether a dream is already in progress (lock gate).
    pub dreaming: bool,
}

// ── Gate evaluation ──────────────────────────────────────────────

/// Check whether all three gates pass and a dream should be triggered.
///
/// Gate 1 — **Time**: at least `min_hours_between_dreams` have elapsed since
///   the last dream, or the agent has never dreamed before.
/// Gate 2 — **Sessions**: at least `min_sessions_between_dreams` have started
///   since the last dream.
/// Gate 3 — **Lock**: no dream is currently in progress.
pub fn should_dream(state: &DreamState, config: &DreamConfig) -> bool {
    if !config.enabled {
        return false;
    }

    // Gate 3: lock
    if state.dreaming {
        return false;
    }

    // Gate 2: sessions
    if state.sessions_since_dream < config.min_sessions_between_dreams {
        return false;
    }

    // Gate 1: time
    match state.last_dream_at {
        None => true, // Never dreamed — always pass the time gate.
        Some(last) => {
            let elapsed = Utc::now().signed_duration_since(last);
            let min_duration =
                chrono::Duration::hours(config.min_hours_between_dreams as i64);
            elapsed >= min_duration
        }
    }
}

// ── Dream result ─────────────────────────────────────────────────

/// Result of a dream consolidation run.
#[derive(Debug, Deserialize)]
pub struct DreamResult {
    /// Cross-session patterns identified by the LLM.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// Core facts worth promoting to the Core memory category.
    #[serde(default)]
    pub core_memories: Vec<String>,
    /// Contradictions detected between existing memories.
    #[serde(default)]
    pub contradictions: Vec<String>,
    /// Keys of stale entries that should be archived / forgotten.
    #[serde(default)]
    pub stale_entry_keys: Vec<String>,
}

// ── System prompt ────────────────────────────────────────────────

pub const DREAM_SYSTEM_PROMPT: &str = r#"You are a memory consolidation engine performing a "dream" pass over recent conversation history.

Your task:
1. Identify recurring PATTERNS across multiple sessions (behavioural tendencies, repeated topics, habitual workflows).
2. Extract CORE FACTS — stable, long-term truths about the user or their environment that should survive beyond daily logs.
3. Note CONTRADICTIONS — pairs or groups of memories that conflict with each other and need human review.
4. Flag STALE ENTRIES — memory keys whose information is likely outdated or superseded.

Guidelines:
- Be concise. Each item should be a single clear sentence.
- Only emit items you are confident about.
- For stale_entry_keys, return the exact key strings as they appear in the memory list.
- Do not invent information that is not present in the provided memories.

Respond ONLY with valid JSON in exactly this shape:
{"patterns":[],"core_memories":[],"contradictions":[],"stale_entry_keys":[]}"#;

// ── Dream pipeline ───────────────────────────────────────────────

/// Run the dream pipeline:
/// 1. Load recent Daily memories.
/// 2. Send to LLM for pattern extraction.
/// 3. Store identified patterns in the knowledge graph (if available).
/// 4. Promote `core_memories` to the Core memory category.
/// 5. Archive stale entries via `memory.forget`.
pub async fn run_dream(
    provider: &dyn crate::providers::traits::Provider,
    model: &str,
    memory: &dyn crate::memory::traits::Memory,
    knowledge_graph: Option<&crate::memory::knowledge_graph::KnowledgeGraph>,
) -> anyhow::Result<DreamResult> {
    use crate::memory::importance::compute_importance;
    use crate::memory::knowledge_graph::NodeType;
    use crate::memory::traits::MemoryCategory;

    // Step 1: Load recent Daily memories.
    let daily_entries = memory
        .list(Some(&MemoryCategory::Daily), None)
        .await?;

    if daily_entries.is_empty() {
        tracing::debug!("dream: no daily memories found, skipping");
        return Ok(DreamResult {
            patterns: vec![],
            core_memories: vec![],
            contradictions: vec![],
            stale_entry_keys: vec![],
        });
    }

    // Build a compact text block from the daily entries (key + content).
    let memory_text: String = daily_entries
        .iter()
        .take(200) // cap to avoid overflowing context window
        .map(|e| format!("[{}] {}", e.key, e.content))
        .collect::<Vec<_>>()
        .join("\n");

    // Step 2: Send to LLM.
    let raw = provider
        .chat_with_system(Some(DREAM_SYSTEM_PROMPT), &memory_text, model, 0.1)
        .await?;

    let result = parse_dream_response(&raw);

    // Step 3: Store patterns in knowledge graph.
    if let Some(kg) = knowledge_graph {
        for pattern in &result.patterns {
            if let Err(e) = kg.add_node(
                NodeType::Pattern,
                "Dream Pattern",
                pattern,
                &[],
                None,
            ) {
                tracing::warn!("dream: failed to store pattern in knowledge graph: {e}");
            }
        }
    }

    // Step 4: Promote core_memories to Core category.
    for core_mem in &result.core_memories {
        if core_mem.trim().is_empty() {
            continue;
        }
        let key = format!("dream_core_{}", uuid::Uuid::new_v4());
        let importance = compute_importance(core_mem, &MemoryCategory::Core);
        if let Err(e) = memory
            .store_with_metadata(
                &key,
                core_mem,
                MemoryCategory::Core,
                None,
                None,
                Some(importance),
            )
            .await
        {
            tracing::warn!("dream: failed to store core memory '{key}': {e}");
        }
    }

    // Step 5: Archive stale entries.
    for stale_key in &result.stale_entry_keys {
        if let Err(e) = memory.forget(stale_key).await {
            tracing::debug!("dream: could not forget stale entry '{stale_key}': {e}");
        }
    }

    Ok(result)
}

// ── Response parsing ─────────────────────────────────────────────

/// Parse the LLM dream response as JSON, with fallback to an empty `DreamResult`.
fn parse_dream_response(raw: &str) -> DreamResult {
    let cleaned = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    serde_json::from_str(cleaned).unwrap_or_else(|e| {
        tracing::warn!("dream: could not parse LLM response as JSON ({e}), returning empty result");
        DreamResult {
            patterns: vec![],
            core_memories: vec![],
            contradictions: vec![],
            stale_entry_keys: vec![],
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn config_default() -> DreamConfig {
        DreamConfig::default() // enabled=true, hours=24, sessions=5
    }

    // ── Gate tests ───────────────────────────────────────────────

    #[test]
    fn gates_closed_when_recently_dreamed() {
        // Last dream was just now → time gate fails.
        let state = DreamState {
            last_dream_at: Some(Utc::now()),
            sessions_since_dream: 10,
            dreaming: false,
        };
        assert!(!should_dream(&state, &config_default()));
    }

    #[test]
    fn gates_closed_when_too_few_sessions() {
        // 48h ago but only 2 sessions → session gate fails.
        let state = DreamState {
            last_dream_at: Some(Utc::now() - Duration::hours(48)),
            sessions_since_dream: 2,
            dreaming: false,
        };
        assert!(!should_dream(&state, &config_default()));
    }

    #[test]
    fn gates_closed_when_already_dreaming() {
        // All time/session conditions satisfied, but lock is held.
        let state = DreamState {
            last_dream_at: Some(Utc::now() - Duration::hours(48)),
            sessions_since_dream: 10,
            dreaming: true,
        };
        assert!(!should_dream(&state, &config_default()));
    }

    #[test]
    fn gates_open_when_all_conditions_met() {
        // 48h elapsed, 10 sessions, not dreaming → all three gates pass.
        let state = DreamState {
            last_dream_at: Some(Utc::now() - Duration::hours(48)),
            sessions_since_dream: 10,
            dreaming: false,
        };
        assert!(should_dream(&state, &config_default()));
    }

    #[test]
    fn gates_open_on_first_run() {
        // Never dreamed before — time gate passes unconditionally.
        let state = DreamState {
            last_dream_at: None,
            sessions_since_dream: 5,
            dreaming: false,
        };
        assert!(should_dream(&state, &config_default()));
    }

    // ── Parse tests ──────────────────────────────────────────────

    #[test]
    fn parse_dream_response_valid_json() {
        let raw = r#"{"patterns":["User prefers concise answers"],"core_memories":["User uses Rust daily"],"contradictions":["Conflict A"],"stale_entry_keys":["key_abc"]}"#;
        let result = parse_dream_response(raw);
        assert_eq!(result.patterns, vec!["User prefers concise answers"]);
        assert_eq!(result.core_memories, vec!["User uses Rust daily"]);
        assert_eq!(result.contradictions, vec!["Conflict A"]);
        assert_eq!(result.stale_entry_keys, vec!["key_abc"]);
    }

    #[test]
    fn parse_dream_response_fallback() {
        let result = parse_dream_response("this is not json at all!!!!");
        assert!(result.patterns.is_empty());
        assert!(result.core_memories.is_empty());
        assert!(result.contradictions.is_empty());
        assert!(result.stale_entry_keys.is_empty());
    }
}
