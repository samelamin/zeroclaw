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

    let mut cut = content.len() - MAX_TOOL_OUTPUT_BYTES;

    while cut < content.len() && !content.is_char_boundary(cut) {
        cut += 1;
    }

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
        let head = "é".repeat(20_000);
        let tail = "TAIL";
        let input = format!("{head}{tail}");
        let r = format_tool_output(&input, false);
        assert!(r.ends_with(tail));
        assert!(r.len() <= MAX_TOOL_OUTPUT_BYTES + 128);
    }
}
