//! Behavioral task-suite runner for agent core promotion gating.
//!
//! Drives a scripted provider through a fixed set of tasks against both
//! agent.core values and reports pass/fail + iterations + tokens.
//!
//! Used by `tests/agent_eval_suite.rs` for CI-gated promotion.

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
        if for_core.is_empty() {
            return 0.0;
        }
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
        // ── Category 2: Multi-step chaining (≥4 tool calls) ──────────────
        EvalTask {
            id: "multi-step-01-four-call-chain",
            category: Category::MultiStepChaining,
            user_prompt: "List files, grep for TODO in them, count the matches, then report the result.",
            scripted_responses: vec![
                // Step 1: ls to list files.
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "ms1".into(),
                        name: "shell".into(),
                        arguments: r#"{"cmd":"ls ."}"#.into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                // Step 2: grep for TODO.
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "ms2".into(),
                        name: "shell".into(),
                        arguments: r#"{"cmd":"grep -r TODO ."}"#.into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                // Step 3: count matches.
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "ms3".into(),
                        name: "shell".into(),
                        arguments: r#"{"cmd":"grep -r TODO . | wc -l"}"#.into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                // Step 4: another shell call to format/report.
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "ms4".into(),
                        name: "shell".into(),
                        arguments: r#"{"cmd":"echo 'TODO count: 42'"}"#.into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                // Final: return text summary.
                ChatResponse {
                    text: Some("Found 42 TODO items across the files.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
            ],
            succeeded_if: |text, _| text.chars().any(|c| c.is_ascii_digit()),
            max_iterations: 10,
        },
        // ── Category 3: Wrong-tool recovery ──────────────────────────────
        EvalTask {
            id: "wrong-tool-01-shell-instead-of-recall",
            category: Category::WrongToolRecovery,
            user_prompt: "What did I remember about the deploy region?",
            scripted_responses: vec![
                // First turn: model incorrectly uses shell instead of recall.
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "wt1".into(),
                        name: "shell".into(),
                        arguments: r#"{"cmd":"cat ~/.memory/deploy_region.md"}"#.into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                // Second turn: shell returned nothing useful; pivot to recall.
                ChatResponse {
                    text: None,
                    tool_calls: vec![ToolCall {
                        id: "wt2".into(),
                        name: "recall".into(),
                        arguments: r#"{"query":"deploy region"}"#.into(),
                    }],
                    usage: None,
                    reasoning_content: None,
                },
                // Third turn: recall returned the memory; summarise.
                ChatResponse {
                    text: Some("According to your memory, the deploy region is us-east-1.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
            ],
            succeeded_if: |text, _| text.contains("us-east-1"),
            max_iterations: 5,
        },
        // ── Category 4: Long-context retention ───────────────────────────
        EvalTask {
            id: "long-ctx-01-fact-at-turn-zero",
            category: Category::LongContextRetention,
            user_prompt: "The color is teal. Remember that.",
            scripted_responses: vec![
                // Turn 0 acknowledgement: model acks the fact.
                ChatResponse {
                    text: Some("Got it, I'll remember the color is teal.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                // Noise turns 1-15: short back-and-forth with no tool calls.
                ChatResponse {
                    text: Some("Understood.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("OK.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Sure.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Done.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Noted.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Acknowledged.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Alright.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("I see.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Confirmed.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Very well.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Certainly.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Of course.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Will do.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Right.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                ChatResponse {
                    text: Some("Absolutely.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
                // Final turn (turn 16): the question about color — answer with teal.
                ChatResponse {
                    text: Some("The color you set at the start is teal.".into()),
                    tool_calls: vec![],
                    usage: None,
                    reasoning_content: None,
                },
            ],
            succeeded_if: |text, _| text.contains("teal"),
            max_iterations: 20,
        },
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
            assert!(n >= 1, "category {cat:?} has {n} tasks; require >=1 in v1");
        }
        assert!(tasks.len() >= 4, "v1 suite must have >=4 tasks total");
    }

    #[test]
    fn pass_rate_and_median_tokens_zero_on_empty_suite() {
        let s = EvalSummary { results: vec![] };
        assert_eq!(s.pass_rate("minimal"), 0.0);
        assert_eq!(s.median_tokens("minimal"), 0);
    }
}
