//! Integration test: drives both agent.core values through the canonical eval
//! task suite and prints per-core pass rate + median tokens.
//!
//! Serves as the promotion gate used by Task 9.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use zeroclaw::agent::agent::Agent;
use zeroclaw::agent::dispatcher::NativeToolDispatcher;
use zeroclaw::agent::eval_suite::{canonical_tasks, run_suite};
use zeroclaw::config::AgentConfig;
use zeroclaw::memory::MarkdownMemory;
use zeroclaw::observability::NoopObserver;
use zeroclaw::providers::traits::{ChatResponse, ToolCall};
use zeroclaw::providers::{ChatRequest, Provider};

/// Scripted provider that pops responses from a queue in FIFO order.
/// When the queue is empty, returns a safe default ("done") rather than panicking.
struct ScriptedProvider {
    queue: Mutex<Vec<ChatResponse>>,
}

impl ScriptedProvider {
    fn new(mut responses: Vec<ChatResponse>) -> Self {
        // Reverse so we can use pop() for FIFO order.
        responses.reverse();
        Self {
            queue: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        // Not used by the agent loop; satisfy the trait.
        Ok("done".into())
    }

    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> Result<ChatResponse> {
        let mut queue = self.queue.lock().expect("scripted provider lock");
        Ok(queue.pop().unwrap_or_else(|| ChatResponse {
            text: Some("done".into()),
            tool_calls: vec![],
            usage: None,
            reasoning_content: None,
        }))
    }
}

/// Build an Agent for the given core string, wired up with the scripted provider.
fn make_agent(core: &str, responses: Vec<ChatResponse>) -> Agent {
    let tmp = TempDir::new().expect("tempdir");
    // Keep the TempDir alive by leaking it — this is a test, memory is fine.
    let tmp_path = tmp.into_path();

    let memory = Arc::new(MarkdownMemory::new(&tmp_path));
    let observer = Arc::new(NoopObserver {});

    let mut config = AgentConfig::default();
    config.core = core.to_string();

    Agent::builder()
        .provider(Box::new(ScriptedProvider::new(responses)))
        .tools(vec![])
        .memory(memory)
        .observer(observer)
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .config(config)
        .workspace_dir(tmp_path)
        .build()
        .expect("agent build")
}

#[tokio::test]
async fn eval_suite_reports_for_both_cores() {
    let tasks = canonical_tasks();

    let legacy = run_suite(&tasks, "legacy", |core, responses| {
        make_agent(core, responses)
    })
    .await;

    let minimal = run_suite(&tasks, "minimal", |core, responses| {
        make_agent(core, responses)
    })
    .await;

    println!(
        "legacy pass rate:  {:.0}%  median tokens: {}",
        legacy.pass_rate("legacy") * 100.0,
        legacy.median_tokens("legacy")
    );
    println!(
        "minimal pass rate: {:.0}%  median tokens: {}",
        minimal.pass_rate("minimal") * 100.0,
        minimal.median_tokens("minimal")
    );

    // v1: only assert the runner completes all tasks for both cores.
    // Promotion thresholds are enforced in Task 9.
    assert_eq!(legacy.results.len(), tasks.len());
    assert_eq!(minimal.results.len(), tasks.len());
}
