//! The `remember` tool — append a durable fact to project memory.
//!
//! Backed by `Arc<dyn Memory>`, which in production is `MarkdownMemory`
//! (appends to `MEMORY.md` + dated daily files under the workspace).
//! No embeddings, no background writer, no consolidation.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

use crate::memory::traits::{Memory, MemoryCategory};
use crate::tools::traits::{Tool, ToolResult};

#[derive(Debug, Deserialize)]
struct RememberArgs {
    fact: String,
    #[serde(default)]
    tags: Vec<String>,
}

pub struct RememberTool {
    memory: Arc<dyn Memory>,
}

impl RememberTool {
    pub fn new(memory: Arc<dyn Memory>) -> Self {
        Self { memory }
    }
}

#[async_trait]
impl Tool for RememberTool {
    fn name(&self) -> &str {
        "remember"
    }

    fn description(&self) -> &str {
        "Save a durable fact to project memory. Use for user preferences, \
         architectural decisions, or learned context you want to persist to \
         later sessions. Do NOT use for ephemeral task state — that belongs in \
         the transcript, not in memory."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "fact": {
                    "type": "string",
                    "description": "The fact to remember. One sentence is ideal. \
                                    Past tense or present tense; avoid imperatives."
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional tags for later retrieval (e.g. [\"preference\", \"whatsapp\"])."
                }
            },
            "required": ["fact"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let parsed: RememberArgs = serde_json::from_value(args)
            .map_err(|e| anyhow::anyhow!("invalid arguments for remember: {e}"))?;

        let key = format!("remember-{}", uuid::Uuid::new_v4());
        let content = if parsed.tags.is_empty() {
            parsed.fact.clone()
        } else {
            format!("{} [tags: {}]", parsed.fact, parsed.tags.join(", "))
        };

        self.memory
            .store(&key, &content, MemoryCategory::Core, None)
            .await?;

        Ok(ToolResult {
            success: true,
            output: format!("Remembered: {}", parsed.fact),
            error: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::markdown::MarkdownMemory;
    use tempfile::tempdir;

    #[tokio::test]
    async fn remember_appends_fact() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> = Arc::new(MarkdownMemory::new(tmp.path()));
        let tool = RememberTool::new(mem.clone());

        let r = tool
            .execute(json!({"fact": "user prefers brief responses"}))
            .await
            .unwrap();
        assert!(r.success);
        assert!(r.output.contains("brief"));

        // Roundtrip via recall to prove it landed.
        let hits = mem
            .recall("brief", 10, None, None, None)
            .await
            .unwrap();
        assert!(hits.iter().any(|e| e.content.contains("brief")));
    }

    #[tokio::test]
    async fn remember_with_tags_includes_tag_suffix() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> = Arc::new(MarkdownMemory::new(tmp.path()));
        let tool = RememberTool::new(mem.clone());

        tool.execute(json!({
            "fact": "deploy target is eu-west-1",
            "tags": ["deploy", "region"]
        }))
        .await
        .unwrap();

        let hits = mem
            .recall("eu-west-1", 10, None, None, None)
            .await
            .unwrap();
        let content = &hits.iter().find(|e| e.content.contains("eu-west-1")).unwrap().content;
        assert!(content.contains("[tags: deploy, region]"));
    }

    #[tokio::test]
    async fn remember_rejects_missing_fact() {
        let tmp = tempdir().unwrap();
        let mem: Arc<dyn Memory> = Arc::new(MarkdownMemory::new(tmp.path()));
        let tool = RememberTool::new(mem);

        let r = tool.execute(json!({"tags": ["nope"]})).await;
        assert!(r.is_err(), "missing `fact` must error");
    }
}
