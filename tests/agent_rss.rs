//! RSS regression test for agent.core = "minimal".
//!
//! Drives 500 agent.turn() calls and asserts RSS growth stays under 20 MB.
//! This catches the brain.db-class memory snowball before it re-lands.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;
use sysinfo::{ProcessesToUpdate, System, get_current_pid};
use tempfile::TempDir;
use zeroclaw::agent::agent::Agent;
use zeroclaw::agent::dispatcher::NativeToolDispatcher;
use zeroclaw::config::AgentConfig;
use zeroclaw::memory::MarkdownMemory;
use zeroclaw::observability::NoopObserver;
use zeroclaw::providers::{ChatRequest, ChatResponse, Provider};

/// Scripted provider that always returns "done" with no tool calls.
struct ScriptedProvider;

#[async_trait]
impl Provider for ScriptedProvider {
    async fn chat_with_system(
        &self,
        _system_prompt: Option<&str>,
        _message: &str,
        _model: &str,
        _temperature: f64,
    ) -> Result<String> {
        Ok("done".into())
    }

    async fn chat(
        &self,
        _request: ChatRequest<'_>,
        _model: &str,
        _temperature: f64,
    ) -> Result<ChatResponse> {
        Ok(ChatResponse {
            text: Some("done".into()),
            tool_calls: vec![],
            usage: None,
            reasoning_content: None,
        })
    }
}

fn rss_kb() -> u64 {
    let pid = get_current_pid().expect("get current pid");
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::Some(&[pid]), false);
    sys.process(pid).map(|p| p.memory()).unwrap_or(0) / 1024
}

#[tokio::test]
async fn agent_core_minimal_500_turns_rss_stable() {
    let tmp = TempDir::new().expect("tempdir");
    let memory = Arc::new(MarkdownMemory::new(tmp.path()));
    let observer = Arc::new(NoopObserver {});

    let mut config = AgentConfig::default();
    config.core = "minimal".into();

    let mut agent = Agent::builder()
        .provider(Box::new(ScriptedProvider))
        .tools(vec![])
        .memory(memory)
        .observer(observer)
        .tool_dispatcher(Box::new(NativeToolDispatcher))
        .config(config)
        .workspace_dir(tmp.path().to_path_buf())
        .build()
        .expect("agent build");

    // Warm-up turn + clear history to settle allocations.
    let _ = agent.turn("warm-up").await.unwrap();
    agent.clear_history();

    let rss_before = rss_kb();

    for i in 0..500_u32 {
        agent.turn(&format!("turn-{i}")).await.unwrap();
        // Clear history each turn to keep conversation size bounded
        // (mirrors how the minimal-core path is used in production).
        agent.clear_history();
    }

    let rss_after = rss_kb();
    let growth_kb = rss_after.saturating_sub(rss_before);
    let limit_kb: u64 = 20 * 1024; // 20 MB

    println!(
        "RSS before={rss_before} KB  after={rss_after} KB  growth={growth_kb} KB  (limit={limit_kb} KB)"
    );

    assert!(
        growth_kb < limit_kb,
        "RSS grew by {growth_kb} KB over 500 turns — exceeds {limit_kb} KB limit (possible memory snowball)"
    );
}
