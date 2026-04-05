//! LLM-driven skill refinement after successful use.

/// Build a prompt for the LLM to review a skill definition against an execution trace.
pub fn build_refinement_prompt(slug: &str, current_content: &str, execution_trace: &str) -> String {
    format!(
        r#"You are reviewing the skill definition for "{slug}" after a successful execution.

## Current Skill Definition

```toml
{current_content}
```

## Execution Trace

{execution_trace}

## Task

Based on the execution trace above, decide whether the skill definition should be improved.

Respond with a JSON object in this exact format:
{{"improved_content": "..." or null, "reason": "..."}}

- If the skill is adequate as-is, set `improved_content` to `null`.
- If you propose improvements, set `improved_content` to the full updated TOML content.
- Always provide a `reason` explaining your decision.
- Do not include any text outside the JSON object."#
    )
}

/// Parse the LLM's refinement response.
/// Returns Some((improved_content, reason)) if improvement suggested, None otherwise.
pub fn parse_refinement_response(raw: &str) -> Option<(String, String)> {
    // Strip markdown code blocks if present.
    let stripped = strip_code_blocks(raw);
    let trimmed = stripped.trim();

    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;

    let reason = value.get("reason")?.as_str()?.to_string();

    let improved_content = match value.get("improved_content") {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        _ => return None,
    };

    Some((improved_content, reason))
}

/// Strip surrounding markdown code fences (```...```) from a string.
fn strip_code_blocks(s: &str) -> String {
    let s = s.trim();
    // Handle ```json ... ``` or ``` ... ```
    if let Some(inner) = s.strip_prefix("```") {
        // Strip optional language identifier on first line.
        let after_fence = if let Some(newline_pos) = inner.find('\n') {
            &inner[newline_pos + 1..]
        } else {
            inner
        };
        if let Some(content) = after_fence.strip_suffix("```") {
            return content.trim().to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_refinement_prompt_includes_skill_and_trace() {
        let prompt = build_refinement_prompt(
            "my-skill",
            "[skill]\nname = \"my-skill\"\n",
            "step 1: ran command\nstep 2: success",
        );
        assert!(prompt.contains("my-skill"));
        assert!(prompt.contains("[skill]\nname = \"my-skill\""));
        assert!(prompt.contains("step 1: ran command"));
        assert!(prompt.contains("step 2: success"));
    }

    #[test]
    fn parse_refinement_response_extracts_content() {
        let raw = r#"{"improved_content": "[skill]\nname = \"improved\"\n", "reason": "Better description"}"#;
        let result = parse_refinement_response(raw);
        assert!(result.is_some());
        let (content, reason) = result.unwrap();
        assert!(content.contains("improved"));
        assert_eq!(reason, "Better description");
    }

    #[test]
    fn parse_refinement_response_handles_no_change() {
        let raw = r#"{"improved_content": null, "reason": "Skill is adequate"}"#;
        let result = parse_refinement_response(raw);
        assert!(result.is_none());
    }

    #[test]
    fn parse_refinement_response_handles_empty_content() {
        let raw = r#"{"improved_content": "", "reason": "Nothing to change"}"#;
        let result = parse_refinement_response(raw);
        assert!(result.is_none());
    }

    #[test]
    fn parse_refinement_response_handles_malformed() {
        let result = parse_refinement_response("this is not json at all }{");
        assert!(result.is_none());
    }
}
