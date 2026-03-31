//! Tool executor with per-tool retry policies, timeouts, and circuit breakers.
//!
//! Wraps the raw `Tool::execute()` call with configurable retry logic.
//! Side-effecting tools (shell, file_write, file_edit) default to zero retries.
//! Read-only and network tools can opt into retry for transient failures.

use crate::tools::{Tool, ToolResult};
use std::collections::HashMap;
use std::time::Duration;

// ── Retry Policy ────────────────────────────────────────────────────

/// Per-tool retry configuration.
#[derive(Debug, Clone)]
pub struct ToolRetryPolicy {
    /// Maximum retry attempts (0 = no retry). Default: 0.
    pub max_retries: u32,
    /// Base backoff in milliseconds. Default: 500.
    pub backoff_base_ms: u64,
    /// Maximum backoff cap in milliseconds. Default: 5000.
    pub backoff_max_ms: u64,
}

impl Default for ToolRetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            backoff_base_ms: 500,
            backoff_max_ms: 5_000,
        }
    }
}

// ── Retry Classification ────────────────────────────────────────────

/// Determine if a failed tool result is retryable.
///
/// Non-retryable patterns: security violations, parameter errors, permission denials.
/// Everything else (timeouts, transient IO, network errors) is retryable.
fn is_retryable(result: &ToolResult) -> bool {
    if result.success {
        return false;
    }
    let error_text = result
        .error
        .as_deref()
        .unwrap_or(&result.output)
        .to_lowercase();

    let non_retryable = [
        "not allowed",
        "security policy",
        "read-only",
        "rate limit exceeded",
        "action blocked",
        "autonomy",
        "must not be empty",
        "missing",
        "parameter",
        "permission denied",
        "workspace",
        "forbidden",
        "symlink",
        "runtime config",
    ];

    !non_retryable.iter().any(|pat| error_text.contains(pat))
}

// ── Executor ────────────────────────────────────────────────────────

/// Tool executor wrapping `Tool::execute()` with retry and timeout.
pub struct ToolExecutor {
    default_timeout: Duration,
    policies: HashMap<String, ToolRetryPolicy>,
}

impl ToolExecutor {
    /// Create a new executor with default policies for known tools.
    pub fn new() -> Self {
        let mut policies = HashMap::new();

        for name in &["file_read", "content_search", "glob_search"] {
            policies.insert(
                (*name).to_string(),
                ToolRetryPolicy {
                    max_retries: 1,
                    ..Default::default()
                },
            );
        }

        for name in &["web_fetch", "http_request"] {
            policies.insert(
                (*name).to_string(),
                ToolRetryPolicy {
                    max_retries: 2,
                    backoff_base_ms: 1_000,
                    ..Default::default()
                },
            );
        }

        Self {
            default_timeout: Duration::from_secs(60),
            policies,
        }
    }

    /// Execute a tool with retry policy.
    pub async fn execute(&self, tool: &dyn Tool, args: serde_json::Value) -> ToolResult {
        let policy = self.policies.get(tool.name()).cloned().unwrap_or_default();

        let mut last_result = ToolResult {
            success: false,
            output: String::new(),
            error: Some("Tool did not execute".into()),
        };

        let max_attempts = 1 + policy.max_retries;
        for attempt in 0..max_attempts {
            let execute_future = tool.execute(args.clone());
            let timed = tokio::time::timeout(self.default_timeout, execute_future).await;

            last_result = match timed {
                Ok(Ok(result)) => result,
                Ok(Err(e)) => ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Tool execution error: {e}")),
                },
                Err(_) => ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!(
                        "Tool timed out after {}s",
                        self.default_timeout.as_secs()
                    )),
                },
            };

            if last_result.success || !is_retryable(&last_result) {
                return last_result;
            }

            if attempt + 1 >= max_attempts {
                break;
            }

            let backoff_ms =
                (policy.backoff_base_ms * 2u64.pow(attempt)).min(policy.backoff_max_ms);
            tracing::debug!(
                tool = tool.name(),
                attempt = attempt + 1,
                max_attempts,
                backoff_ms,
                error = ?last_result.error,
                "Tool failed, retrying"
            );
            tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
        }

        last_result
    }
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FailNThenSucceed {
        name: String,
        fail_count: AtomicU32,
        fails_remaining: AtomicU32,
    }

    impl FailNThenSucceed {
        fn new(name: &str, fail_n: u32) -> Self {
            Self {
                name: name.to_string(),
                fail_count: AtomicU32::new(0),
                fails_remaining: AtomicU32::new(fail_n),
            }
        }
    }

    #[async_trait]
    impl Tool for FailNThenSucceed {
        fn name(&self) -> &str {
            &self.name
        }
        fn description(&self) -> &str {
            "test tool"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            let remaining = self.fails_remaining.fetch_sub(1, Ordering::Relaxed);
            if remaining > 0 {
                self.fail_count.fetch_add(1, Ordering::Relaxed);
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some("connection reset".into()),
                })
            } else {
                Ok(ToolResult {
                    success: true,
                    output: "ok".into(),
                    error: None,
                })
            }
        }
    }

    struct SecurityDeniedTool;

    #[async_trait]
    impl Tool for SecurityDeniedTool {
        fn name(&self) -> &str {
            "denied_tool"
        }
        fn description(&self) -> &str {
            "test"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Path not allowed by security policy".into()),
            })
        }
    }

    #[tokio::test]
    async fn no_retry_for_default_tools() {
        let executor = ToolExecutor::new();
        let tool = FailNThenSucceed::new("shell", 3);
        let result = executor.execute(&tool, serde_json::json!({})).await;
        assert!(!result.success);
        assert_eq!(tool.fail_count.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn retries_read_tools() {
        let executor = ToolExecutor::new();
        let tool = FailNThenSucceed::new("file_read", 1);
        let result = executor.execute(&tool, serde_json::json!({})).await;
        assert!(result.success, "should succeed after retry");
    }

    #[tokio::test]
    async fn retries_network_tools() {
        let executor = ToolExecutor::new();
        let tool = FailNThenSucceed::new("web_fetch", 2);
        let result = executor.execute(&tool, serde_json::json!({})).await;
        assert!(result.success, "should succeed after 2 retries");
    }

    #[tokio::test]
    async fn no_retry_on_security_error() {
        let executor = ToolExecutor::new();
        let tool = SecurityDeniedTool;
        let result = executor.execute(&tool, serde_json::json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("security policy"));
    }

    #[tokio::test]
    async fn exhausts_retries_then_returns_last_error() {
        let executor = ToolExecutor::new();
        let tool = FailNThenSucceed::new("web_fetch", 5);
        let result = executor.execute(&tool, serde_json::json!({})).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("connection reset"));
    }

    #[test]
    fn retryable_classification() {
        assert!(!is_retryable(&ToolResult {
            success: true,
            output: "ok".into(),
            error: None,
        }));
        assert!(!is_retryable(&ToolResult {
            success: false,
            output: String::new(),
            error: Some("Path not allowed by security policy".into()),
        }));
        assert!(!is_retryable(&ToolResult {
            success: false,
            output: String::new(),
            error: Some("Action blocked: autonomy is read-only".into()),
        }));
        assert!(is_retryable(&ToolResult {
            success: false,
            output: String::new(),
            error: Some("connection reset by peer".into()),
        }));
        assert!(is_retryable(&ToolResult {
            success: false,
            output: String::new(),
            error: Some("Tool timed out after 60s".into()),
        }));
    }
}
