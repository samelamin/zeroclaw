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
        let mem: Arc<dyn Memory> = Arc::new(MarkdownMemory::new(tmp.path()));
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
        let mem: Arc<dyn Memory> = Arc::new(MarkdownMemory::new(tmp.path()));
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
        let mem: Arc<dyn Memory> = Arc::new(MarkdownMemory::new(tmp.path()));
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
