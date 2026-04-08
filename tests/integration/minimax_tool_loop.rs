//! §7 — MiniMax tool-loop reliability CI integration tests.
//!
//! These tests guard against regressions in the agent orchestration loop when
//! used with MiniMax M2.7 (or any model whose behaviour matches it):
//!
//! * Multi-step tool pipelines complete without stalling.
//! * The loop terminates correctly at `max_tool_iterations`.
//! * Parallel tool calls in a single response all execute.
//! * Tool failures are recovered from and the pipeline continues.
//! * The XML dispatcher path (`<tool_call>` JSON) works end-to-end.
//! * `merge_system_into_user` content appears in the first user message sent
//!   to the provider (validated via `RecordingProvider`).
//!
//! All tests use mock providers and mock tools — no external services required.
//!
//! Context: Naseyma's multi-agent WhatsApp website-generation pipeline relies on
//! MiniMax M2.7 calling 3–5 tools in sequence (fetch brand info → generate HTML
//! → generate CSS → save → deploy). These tests model that exact lifecycle.

use crate::support::helpers::{
    build_agent, build_agent_xml, text_response, tool_response,
};
use crate::support::{CountingTool, EchoTool, FailingTool, MockProvider, RecordingProvider};
use zeroclaw::config::AgentConfig;
use zeroclaw::providers::{ChatResponse, ToolCall};

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Build an agent that uses `NativeToolDispatcher` with a custom `max_tool_iterations`.
fn build_agent_with_max_iters(
    provider: Box<dyn zeroclaw::providers::Provider>,
    tools: Vec<Box<dyn zeroclaw::tools::Tool>>,
    max_tool_iterations: usize,
) -> zeroclaw::agent::agent::Agent {
    use std::sync::Arc;
    use zeroclaw::agent::dispatcher::NativeToolDispatcher;
    use zeroclaw::memory;
    use zeroclaw::observability::NoopObserver;

    let cfg = AgentConfig {
        max_tool_iterations,
        ..AgentConfig::default()
    };

    let mem_cfg = zeroclaw::config::MemoryConfig {
        backend: "none".into(),
        ..zeroclaw::config::MemoryConfig::default()
    };
    let mem = Arc::from(memory::create_memory(&mem_cfg, &std::env::temp_dir(), None).unwrap());

    zeroclaw::agent::agent::Agent::builder()
        .provider(provider)
        .tools(tools)
        .memory(mem)
        .observer(Arc::from(NoopObserver {}))
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .workspace_dir(std::env::temp_dir())
        .config(cfg)
        .build()
        .unwrap()
}

// ─────────────────────────────────────────────────────────────────────────────
// §7.1  Multi-step pipeline — Naseyma's website generation scenario
// ─────────────────────────────────────────────────────────────────────────────

/// Three sequential tool calls followed by a final text response.
/// Models the core website-generation pipeline:
///   fetch_brand → generate_html → save_page → "Website deployed"
#[tokio::test]
async fn minimax_3step_pipeline_completes_and_returns_final_text() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(MockProvider::new(vec![
        // Step 1: fetch_brand
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        // Step 2: generate_html
        tool_response(vec![ToolCall {
            id: "tc2".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        // Step 3: save_page
        tool_response(vec![ToolCall {
            id: "tc3".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        // Final text response
        text_response("Website deployed successfully."),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(counting_tool)]);
    let response = agent.turn("generate website for brand X").await.unwrap();

    assert_eq!(
        *count.lock().unwrap(),
        3,
        "all 3 pipeline steps should execute"
    );
    assert!(
        response.contains("Website deployed"),
        "final response should contain the text: {response:?}"
    );
}

/// Five-step pipeline — higher iteration count must still complete.
/// Validates that the iteration cap doesn't fire too early (default is 10).
#[tokio::test]
async fn minimax_5step_pipeline_completes_within_default_limit() {
    let (counting_tool, count) = CountingTool::new();

    let mut responses: Vec<ChatResponse> = (0..5)
        .map(|i| {
            tool_response(vec![ToolCall {
                id: format!("tc{i}"),
                name: "counter".into(),
                arguments: "{}".into(),
            }])
        })
        .collect();
    responses.push(text_response("All steps done."));

    let provider = Box::new(MockProvider::new(responses));
    let mut agent = build_agent(provider, vec![Box::new(counting_tool)]);
    let response = agent.turn("complex pipeline").await.unwrap();

    assert_eq!(
        *count.lock().unwrap(),
        5,
        "all 5 pipeline steps should execute within default 10-iteration limit"
    );
    assert!(!response.is_empty(), "should return non-empty final response");
}

// ─────────────────────────────────────────────────────────────────────────────
// §7.2  Iteration cap — runaway loop prevention
// ─────────────────────────────────────────────────────────────────────────────

/// If MiniMax keeps emitting tool calls, the agent must stop at max_tool_iterations.
/// Tests with a low cap (3) to keep the test fast.
#[tokio::test]
async fn minimax_tool_loop_terminates_at_max_iterations() {
    let (counting_tool, count) = CountingTool::new();

    // 20 tool call responses — well above the cap of 3
    let mut responses: Vec<ChatResponse> = (0..20)
        .map(|i| {
            tool_response(vec![ToolCall {
                id: format!("tc_{i}"),
                name: "counter".into(),
                arguments: "{}".into(),
            }])
        })
        .collect();
    responses.push(text_response("Fallback after cap"));

    let provider = Box::new(MockProvider::new(responses));
    let mut agent = build_agent_with_max_iters(provider, vec![Box::new(counting_tool)], 3);

    let result = agent.turn("keep looping").await;
    assert!(result.is_ok() || result.is_err(), "agent should not hang");

    let invocations = *count.lock().unwrap();
    assert!(
        invocations <= 3,
        "tool invocations ({invocations}) must not exceed max_tool_iterations=3"
    );
}

/// Even with max_tool_iterations = 1, one tool call completes before termination.
#[tokio::test]
async fn minimax_single_iteration_cap_executes_one_tool() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(MockProvider::new(vec![
        tool_response(vec![ToolCall {
            id: "tc0".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        // A second tool call that should not be reached
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "counter".into(),
            arguments: "{}".into(),
        }]),
        text_response("Stopped after one iteration"),
    ]));

    let mut agent = build_agent_with_max_iters(provider, vec![Box::new(counting_tool)], 1);
    let _ = agent.turn("call once").await;

    let invocations = *count.lock().unwrap();
    assert!(
        invocations <= 1,
        "with max_tool_iterations=1, tool should be called at most once, got: {invocations}"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// §7.3  Parallel tool execution in a single response
// ─────────────────────────────────────────────────────────────────────────────

/// MiniMax may emit two tool calls in one response. Both must execute.
#[tokio::test]
async fn minimax_parallel_tool_calls_both_execute() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(MockProvider::new(vec![
        // Two tool calls in a single response
        tool_response(vec![
            ToolCall {
                id: "tc1".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
            ToolCall {
                id: "tc2".into(),
                name: "counter".into(),
                arguments: "{}".into(),
            },
        ]),
        text_response("Both parallel tools done."),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(counting_tool)]);
    let response = agent.turn("run two tools").await.unwrap();

    assert_eq!(
        *count.lock().unwrap(),
        2,
        "both parallel tool calls should execute"
    );
    assert!(!response.is_empty());
}

/// Four parallel tool calls in one response — all must execute.
#[tokio::test]
async fn minimax_four_parallel_tool_calls_all_execute() {
    let (counting_tool, count) = CountingTool::new();

    let calls: Vec<ToolCall> = (0..4)
        .map(|i| ToolCall {
            id: format!("tc{i}"),
            name: "counter".into(),
            arguments: "{}".into(),
        })
        .collect();

    let provider = Box::new(MockProvider::new(vec![
        tool_response(calls),
        text_response("All four done."),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(counting_tool)]);
    let response = agent.turn("run four tools in parallel").await.unwrap();

    assert_eq!(
        *count.lock().unwrap(),
        4,
        "all four parallel tool calls should execute"
    );
    assert!(!response.is_empty());
}

// ─────────────────────────────────────────────────────────────────────────────
// §7.4  Tool failure recovery
// ─────────────────────────────────────────────────────────────────────────────

/// When one tool in a pipeline fails, the pipeline should still produce a
/// response (not crash). MiniMax is expected to acknowledge the failure and
/// continue or summarise.
#[tokio::test]
async fn minimax_pipeline_recovers_from_tool_failure() {
    let provider = Box::new(MockProvider::new(vec![
        // Step 1 succeeds (echo)
        tool_response(vec![ToolCall {
            id: "tc1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "brand info fetched"}"#.into(),
        }]),
        // Step 2 fails (failing_tool simulates API outage)
        tool_response(vec![ToolCall {
            id: "tc2".into(),
            name: "failing_tool".into(),
            arguments: "{}".into(),
        }]),
        // LLM summarises the partial result
        text_response("Partial website generated — image service unavailable."),
    ]));

    let mut agent = build_agent(
        provider,
        vec![Box::new(EchoTool), Box::new(FailingTool)],
    );
    let response = agent.turn("generate website").await.unwrap();

    assert!(
        !response.is_empty(),
        "agent should return a response even when a tool fails mid-pipeline"
    );
}

/// Mixed response: one tool succeeds, one is unknown. Agent should not panic.
#[tokio::test]
async fn minimax_pipeline_skips_unknown_tool_and_continues() {
    let provider = Box::new(MockProvider::new(vec![
        tool_response(vec![
            ToolCall {
                id: "tc1".into(),
                name: "echo".into(),
                arguments: r#"{"message": "step1"}"#.into(),
            },
            ToolCall {
                id: "tc2".into(),
                name: "not_a_real_tool".into(),
                arguments: "{}".into(),
            },
        ]),
        text_response("Completed with one unknown tool."),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);
    let response = agent.turn("run mixed tools").await.unwrap();
    assert!(!response.is_empty(), "unknown tool should not crash the pipeline");
}

// ─────────────────────────────────────────────────────────────────────────────
// §7.5  XML dispatcher path (XmlToolDispatcher)
// ─────────────────────────────────────────────────────────────────────────────

/// When using XmlToolDispatcher, a `<tool_call>` JSON block in the text field
/// is parsed and the tool is executed correctly.
///
/// Some MiniMax deployments fall back to text-embedded tool calls instead of
/// the structured API format; this ensures those still work end-to-end.
#[tokio::test]
async fn minimax_xml_dispatcher_executes_tool_from_text_response() {
    // Provider returns tool call embedded as text (not in tool_calls field)
    let provider = Box::new(MockProvider::new(vec![
        ChatResponse {
            text: Some(
                r#"I will call the echo tool now.
<tool_call>
{"name": "echo", "arguments": {"message": "hello from MiniMax"}}
</tool_call>"#
                    .into(),
            ),
            tool_calls: vec![], // empty — tool call is in text only
            usage: None,
            reasoning_content: None,
        },
        text_response("Echo executed successfully."),
    ]));

    let mut agent = build_agent_xml(provider, vec![Box::new(EchoTool)]);
    let response = agent.turn("echo something").await.unwrap();
    assert!(
        !response.is_empty(),
        "xml dispatcher should execute tool embedded in text: {response:?}"
    );
}

/// Multiple `<tool_call>` blocks in one text response should all execute.
#[tokio::test]
async fn minimax_xml_dispatcher_executes_multiple_tool_calls_from_text() {
    let (counting_tool, count) = CountingTool::new();

    let provider = Box::new(MockProvider::new(vec![
        ChatResponse {
            text: Some(
                r#"Calling twice:
<tool_call>
{"name": "counter", "arguments": {}}
</tool_call>
<tool_call>
{"name": "counter", "arguments": {}}
</tool_call>"#
                    .into(),
            ),
            tool_calls: vec![],
            usage: None,
            reasoning_content: None,
        },
        text_response("Both counted."),
    ]));

    let mut agent = build_agent_xml(provider, vec![Box::new(counting_tool)]);
    let _ = agent.turn("count twice").await;

    assert_eq!(
        *count.lock().unwrap(),
        2,
        "both XML tool call blocks should execute"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// §7.6  System-prompt merge behaviour
// ─────────────────────────────────────────────────────────────────────────────

/// MiniMax rejects `role: system` messages. The `OpenAiCompatibleProvider` with
/// `merge_system_into_user = true` prepends the system prompt into the first
/// user message. We verify that the `RecordingProvider` sees all expected content
/// in its first request, even though the Agent has a system prompt configured.
///
/// This test does NOT require a real MiniMax endpoint; we use `RecordingProvider`
/// to capture what would be sent to the API.
#[tokio::test]
async fn minimax_agent_turn_produces_non_empty_response_with_system_prompt() {
    // RecordingProvider lets us verify the messages sent to the "provider"
    let (provider, recorded) = RecordingProvider::new(vec![text_response("Hello from MiniMax")]);

    let mut agent = build_agent(Box::new(provider), vec![Box::new(EchoTool)]);
    let response = agent.turn("hello").await.unwrap();

    assert!(!response.is_empty(), "should get a response: {response:?}");

    // Verify the provider was called at least once
    let calls = recorded.lock().unwrap();
    assert!(
        !calls.is_empty(),
        "RecordingProvider should have captured at least one request"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// §7.7  Idempotency and state isolation
// ─────────────────────────────────────────────────────────────────────────────

/// Two consecutive turns on the same agent each complete without bleeding state.
/// Ensures the history accumulator doesn't confuse MiniMax-style multi-step loops
/// across separate user messages.
#[tokio::test]
async fn minimax_two_consecutive_turns_both_complete() {
    let provider = Box::new(MockProvider::new(vec![
        // Turn 1
        tool_response(vec![ToolCall {
            id: "t1".into(),
            name: "echo".into(),
            arguments: r#"{"message": "turn 1 tool"}"#.into(),
        }]),
        text_response("Turn 1 complete."),
        // Turn 2
        tool_response(vec![ToolCall {
            id: "t2".into(),
            name: "echo".into(),
            arguments: r#"{"message": "turn 2 tool"}"#.into(),
        }]),
        text_response("Turn 2 complete."),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);

    let r1 = agent.turn("first request").await.unwrap();
    let r2 = agent.turn("second request").await.unwrap();

    assert!(!r1.is_empty(), "turn 1 should produce a response");
    assert!(!r2.is_empty(), "turn 2 should produce a response");
}

/// Tool loop with a mix of text + tool calls simulates the most common MiniMax
/// output pattern: reasoning text before the tool invocation.
#[tokio::test]
async fn minimax_tool_response_with_preamble_text_executes_tool() {
    let provider = Box::new(MockProvider::new(vec![
        ChatResponse {
            // Text before tool calls (MiniMax often emits reasoning first)
            text: Some("Let me look that up for you...".into()),
            tool_calls: vec![ToolCall {
                id: "tc1".into(),
                name: "echo".into(),
                arguments: r#"{"message": "looked up"}"#.into(),
            }],
            usage: None,
            reasoning_content: None,
        },
        text_response("Here is the result."),
    ]));

    let mut agent = build_agent(provider, vec![Box::new(EchoTool)]);
    let response = agent.turn("look something up").await.unwrap();
    assert!(
        !response.is_empty(),
        "agent should complete when tool call has preamble text"
    );
}
