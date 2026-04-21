//! Surgical tool-result microcompaction.
//!
//! Replaces old, large tool-result messages with short placeholders before
//! each LLM call. Runs every turn with zero LLM cost (pure string ops).
//! Acts as a first-pass filter before [`super::context_compressor`].

use crate::providers::traits::ChatMessage;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Config ──────────────────────────────────────────────────────────

fn default_enabled() -> bool {
    true
}
fn default_protect_recent_turns() -> usize {
    6
}
fn default_max_result_chars() -> usize {
    500
}
fn default_preview_chars() -> usize {
    200
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MicrocompactionConfig {
    /// Enable microcompaction. Default: `true`.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Number of recent tool-result messages to protect from clearing. Default: `6`.
    #[serde(default = "default_protect_recent_turns")]
    pub protect_recent_turns: usize,
    /// Tool results exceeding this char count are cleared. Default: `500`.
    #[serde(default = "default_max_result_chars")]
    pub max_result_chars: usize,
    /// Number of chars to keep as an inline preview. Default: `200`.
    #[serde(default = "default_preview_chars")]
    pub preview_chars: usize,
}

impl Default for MicrocompactionConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            protect_recent_turns: default_protect_recent_turns(),
            max_result_chars: default_max_result_chars(),
            preview_chars: default_preview_chars(),
        }
    }
}

// ── Result ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct MicrocompactionResult {
    pub cleared_count: usize,
    pub chars_reclaimed: usize,
}

// ── Sentinel ────────────────────────────────────────────────────────

const CLEARED_PREFIX: &str = "[Tool result cleared";

// ── Core ────────────────────────────────────────────────────────────

/// Surgically replace old, large tool-result messages with short placeholders.
///
/// Scans `messages` for role `"tool"` entries older than the protected tail.
/// Those exceeding `max_result_chars` have their content replaced with a
/// short preview + cleared marker. Already-cleared messages are skipped.
///
/// This is idempotent and has zero LLM cost.
pub fn microcompact(
    messages: &mut [ChatMessage],
    config: &MicrocompactionConfig,
) -> MicrocompactionResult {
    if !config.enabled {
        return MicrocompactionResult {
            cleared_count: 0,
            chars_reclaimed: 0,
        };
    }

    // Count tool messages from the end to find the protection boundary.
    let total = messages.len();
    let mut tool_count_from_end: usize = 0;
    let mut protection_boundary: usize = total; // index; messages at/after this are protected

    for i in (0..total).rev() {
        if messages[i].role == "tool" {
            tool_count_from_end += 1;
            if tool_count_from_end == config.protect_recent_turns {
                protection_boundary = i;
                break;
            }
        }
    }

    let mut cleared_count: usize = 0;
    let mut chars_reclaimed: usize = 0;

    for msg in &mut messages[..protection_boundary] {
        if msg.role != "tool" {
            continue;
        }
        if msg.content.starts_with(CLEARED_PREFIX) {
            continue; // already cleared
        }
        if msg.content.len() <= config.max_result_chars {
            continue;
        }

        let original_len = msg.content.len();
        let preview_end = config.preview_chars.min(original_len);
        // Find safe char boundary
        let mut safe_end = preview_end;
        while safe_end > 0 && !msg.content.is_char_boundary(safe_end) {
            safe_end -= 1;
        }
        let preview = &msg.content[..safe_end];

        let replacement = format!(
            "{CLEARED_PREFIX} \u{2014} was {} chars]\n{preview}...",
            original_len
        );

        let reclaimed = original_len.saturating_sub(replacement.len());
        chars_reclaimed += reclaimed;
        cleared_count += 1;

        *msg = ChatMessage::tool(replacement);
    }

    MicrocompactionResult {
        cleared_count,
        chars_reclaimed,
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }

    fn default_config() -> MicrocompactionConfig {
        MicrocompactionConfig::default()
    }

    #[test]
    fn noop_when_disabled() {
        let mut messages = vec![
            msg("system", "sys"),
            msg("user", "hello"),
            msg("tool", &"x".repeat(1000)),
        ];
        let config = MicrocompactionConfig {
            enabled: false,
            ..default_config()
        };
        let result = microcompact(&mut messages, &config);
        assert_eq!(result.cleared_count, 0);
        assert_eq!(messages[2].content.len(), 1000);
    }

    #[test]
    fn skips_small_tool_results() {
        let mut messages = vec![
            msg("system", "sys"),
            msg("tool", "short result"),
            msg("user", "next"),
        ];
        let result = microcompact(&mut messages, &default_config());
        assert_eq!(result.cleared_count, 0);
        assert_eq!(messages[1].content, "short result");
    }

    #[test]
    fn clears_old_large_tool_result() {
        let large = "x".repeat(1000);
        let mut messages = vec![
            msg("system", "sys"),
            msg("tool", &large), // old, large — should be cleared
            msg("user", "q1"),
            msg("tool", "small1"), // recent — protected
            msg("tool", "small2"),
            msg("tool", "small3"),
            msg("tool", "small4"),
            msg("tool", "small5"),
            msg("tool", "small6"),
        ];
        let result = microcompact(&mut messages, &default_config());
        assert_eq!(result.cleared_count, 1);
        assert!(result.chars_reclaimed > 0);
        assert!(messages[1].content.starts_with("[Tool result cleared"));
        assert!(messages[1].content.contains("1000 chars"));
    }

    #[test]
    fn protects_recent_tool_results() {
        let large = "y".repeat(1000);
        let config = MicrocompactionConfig {
            protect_recent_turns: 2,
            max_result_chars: 100,
            ..default_config()
        };
        let mut messages = vec![
            msg("system", "sys"),
            msg("tool", &large), // old — should be cleared
            msg("user", "q"),
            msg("tool", &large), // 2nd from end — protected
            msg("tool", &large), // 1st from end — protected
        ];
        let result = microcompact(&mut messages, &config);
        assert_eq!(result.cleared_count, 1);
        assert!(messages[1].content.starts_with("[Tool result cleared"));
        assert_eq!(messages[3].content.len(), 1000);
        assert_eq!(messages[4].content.len(), 1000);
    }

    #[test]
    fn idempotent_skips_already_cleared() {
        let mut messages = vec![
            msg("system", "sys"),
            msg(
                "tool",
                "[Tool result cleared \u{2014} was 5000 chars]\npreview...",
            ),
            msg("user", "next"),
        ];
        let result = microcompact(&mut messages, &default_config());
        assert_eq!(result.cleared_count, 0);
    }

    #[test]
    fn preserves_non_tool_messages() {
        let mut messages = vec![
            msg("system", &"s".repeat(2000)),
            msg("user", &"u".repeat(2000)),
            msg("assistant", &"a".repeat(2000)),
        ];
        let original: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
        microcompact(&mut messages, &default_config());
        for (i, m) in messages.iter().enumerate() {
            assert_eq!(m.content, original[i]);
        }
    }

    #[test]
    fn preview_respects_char_boundary() {
        let content = "\u{1f600}".repeat(200);
        let config = MicrocompactionConfig {
            protect_recent_turns: 0,
            max_result_chars: 100,
            preview_chars: 50,
            ..default_config()
        };
        let mut messages = vec![msg("tool", &content)];
        microcompact(&mut messages, &config);
        assert!(messages[0].content.starts_with("[Tool result cleared"));
    }

    #[test]
    fn config_serde_defaults() {
        let config: MicrocompactionConfig = serde_json::from_str("{}").unwrap();
        assert!(config.enabled);
        assert_eq!(config.protect_recent_turns, 6);
        assert_eq!(config.max_result_chars, 500);
        assert_eq!(config.preview_chars, 200);
    }
}
