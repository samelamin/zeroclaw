//! Generic tool wrappers for crosscutting concerns.
//!
//! Each wrapper implements [`Tool`] by delegating to an inner tool while
//! applying one crosscutting concern around the `execute` call.  Wrappers
//! compose: stack them at construction time in `tools/mod.rs` rather than
//! repeating the same guard blocks inside every tool's `execute` method.
//!
//! # Composition order (outermost first)
//!
//! ```text
//! RateLimitedTool
//!   └─ PathGuardedTool
//!        └─ <concrete tool>
//! ```
//!
//! # Example
//!
//! ```rust,ignore
//! let tool = RateLimitedTool::new(
//!     PathGuardedTool::new(ShellTool::new(security.clone(), runtime), security.clone()),
//!     security.clone(),
//! );
//! ```

use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use std::sync::Arc;

/// Type alias for a path-extraction closure used by [`PathGuardedTool`].
type PathExtractor = dyn Fn(&serde_json::Value) -> Option<String> + Send + Sync;

// ── RateLimitedTool ───────────────────────────────────────────────────────────

/// Wraps any [`Tool`] and enforces the [`SecurityPolicy`] rate limit.
///
/// Replaces the repeated `is_rate_limited()` / `record_action()` guard blocks
/// previously inlined in every tool's `execute` method (~30 files, ~50 call
/// sites).  The inner tool receives the call only when the rate limit allows it.
pub struct RateLimitedTool<T: Tool> {
    inner: T,
    security: Arc<SecurityPolicy>,
}

impl<T: Tool> RateLimitedTool<T> {
    pub fn new(inner: T, security: Arc<SecurityPolicy>) -> Self {
        Self { inner, security }
    }
}

#[async_trait]
impl<T: Tool> Tool for RateLimitedTool<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if self.security.is_rate_limited() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: too many actions in the last hour".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Rate limit exceeded: action budget exhausted".into()),
            });
        }

        self.inner.execute(args).await
    }
}

// ── PathGuardedTool ───────────────────────────────────────────────────────────

/// Wraps any [`Tool`] and blocks calls whose arguments contain a forbidden path.
///
/// Replaces the `forbidden_path_argument()` guard blocks previously inlined in
/// tools that accept a path-like argument (`shell`, `file_read`, `file_write`,
/// `file_edit`, `pdf_read`, `content_search`, `glob_search`, `image_info`).
///
/// Path extraction is argument-name-driven: the wrapper inspects the `"path"`,
/// `"command"`, `"pattern"`, and `"query"` fields of the JSON argument object.
/// Tools whose path argument uses a different field name can pass a custom
/// extractor at construction via [`PathGuardedTool::with_extractor`].
pub struct PathGuardedTool<T: Tool> {
    inner: T,
    security: Arc<SecurityPolicy>,
    /// Optional override: extract a path string from the args JSON.
    extractor: Option<Box<PathExtractor>>,
}

impl<T: Tool> PathGuardedTool<T> {
    pub fn new(inner: T, security: Arc<SecurityPolicy>) -> Self {
        Self {
            inner,
            security,
            extractor: None,
        }
    }

    /// Supply a custom path-extraction closure for tools with non-standard arg names.
    pub fn with_extractor<F>(mut self, f: F) -> Self
    where
        F: Fn(&serde_json::Value) -> Option<String> + Send + Sync + 'static,
    {
        self.extractor = Some(Box::new(f));
        self
    }

    fn extract_path_string(&self, args: &serde_json::Value) -> Option<String> {
        if let Some(ref f) = self.extractor {
            return f(args);
        }
        // Default: check common argument names used across ZeroClaw tools.
        for field in &["path", "command", "pattern", "query", "file"] {
            if let Some(s) = args.get(field).and_then(|v| v.as_str()) {
                return Some(s.to_string());
            }
        }
        None
    }
}

#[async_trait]
impl<T: Tool> Tool for PathGuardedTool<T> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        if let Some(arg) = self.extract_path_string(&args) {
            // For shell command arguments, use the full token-aware scanner.
            // For plain path values (e.g. "path" or custom extractor), fall back
            // to the direct path check.
            let blocked = if self.extractor.is_none()
                && args.get("command").and_then(|v| v.as_str()).is_some()
            {
                self.security.forbidden_path_argument(&arg)
            } else if !self.security.is_path_allowed(&arg) {
                Some(arg.clone())
            } else {
                None
            };

            if let Some(path) = blocked {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Path blocked by security policy: {path}")),
                });
            }
        }

        self.inner.execute(args).await
    }
}

// ── LoggedTool ────────────────────────────────────────────────────────────────

/// Wraps any [`Tool`] and emits structured tracing events on the
/// execute boundary, so no tool failure is ever silent.
///
/// Emission contract:
/// - DEBUG on entry, with `tool` name.
/// - DEBUG on success (`Ok(ToolResult { success: true, .. })`), with
///   `tool` name and `output_len`.
/// - WARN on soft-fail (`Ok(ToolResult { success: false, .. })`),
///   with `tool` name and the `error` string. Soft-fail is the
///   most common failure mode because most tools catch their own
///   errors and return a diagnostic ToolResult. Without this log,
///   these failures are invisible in agent traces.
/// - ERROR on hard-fail (`Err(anyhow::Error)`), with `tool` name,
///   `error` display, and `error_chain` surfacing the full
///   `anyhow::Error::chain()` so the IO/transport root cause is
///   visible at a single `grep ERROR` rather than buried under
///   anyhow context wrapping.
///
/// All events are emitted at `target = "tool_boundary"` so operators
/// can filter them independently from per-tool logs.
///
/// Composition order (outermost first):
///
/// ```text
/// LoggedTool  (outermost — logs soft-fails emitted by inner wrappers too)
///   └─ RateLimitedTool
///        └─ PathGuardedTool
///             └─ <concrete tool>
/// ```
pub struct LoggedTool {
    inner: Box<dyn Tool>,
}

impl LoggedTool {
    /// Wrap a concrete tool. Accepts any `T: Tool + 'static` so call
    /// sites don't need to pre-box.
    pub fn new<T: Tool + 'static>(inner: T) -> Self {
        Self {
            inner: Box::new(inner),
        }
    }

    /// Wrap an already-boxed tool — used by tool-registry factories
    /// that have a `Vec<Box<dyn Tool>>` and want to blanket-log every
    /// tool without knowing each concrete type.
    pub fn wrap_boxed(inner: Box<dyn Tool>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl Tool for LoggedTool {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn parameters_schema(&self) -> serde_json::Value {
        self.inner.parameters_schema()
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let tool_name = self.inner.name().to_string();
        tracing::debug!(
            target: "tool_boundary",
            tool = %tool_name,
            "tool invocation begin"
        );

        match self.inner.execute(args).await {
            Ok(result) => {
                if result.success {
                    tracing::debug!(
                        target: "tool_boundary",
                        tool = %tool_name,
                        output_len = result.output.len(),
                        "tool invocation ok"
                    );
                } else {
                    tracing::warn!(
                        target: "tool_boundary",
                        tool = %tool_name,
                        error = result.error.as_deref().unwrap_or("<none>"),
                        output_len = result.output.len(),
                        "tool invocation returned success=false"
                    );
                }
                Ok(result)
            }
            Err(e) => {
                let chain = e
                    .chain()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(" | caused by: ");
                tracing::error!(
                    target: "tool_boundary",
                    tool = %tool_name,
                    error = %e,
                    error_chain = %chain,
                    "tool invocation raised error"
                );
                Err(e)
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn policy(autonomy: AutonomyLevel) -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    /// A minimal tool that records how many times `execute` was called.
    struct CountingTool {
        calls: Arc<AtomicUsize>,
    }

    impl CountingTool {
        fn new() -> (Self, Arc<AtomicUsize>) {
            let counter = Arc::new(AtomicUsize::new(0));
            (
                CountingTool {
                    calls: counter.clone(),
                },
                counter,
            )
        }
    }

    #[async_trait]
    impl Tool for CountingTool {
        fn name(&self) -> &str {
            "counting"
        }
        fn description(&self) -> &str {
            "counts calls"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(ToolResult {
                success: true,
                output: "ok".into(),
                error: None,
            })
        }
    }

    // ── RateLimitedTool tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn rate_limited_allows_call_within_budget() {
        let (inner, counter) = CountingTool::new();
        let tool = RateLimitedTool::new(inner, policy(AutonomyLevel::Full));
        let result = tool
            .execute(serde_json::json!({}))
            .await
            .expect("should succeed");
        assert!(result.success);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rate_limited_delegates_name_and_schema() {
        let (inner, _) = CountingTool::new();
        let tool = RateLimitedTool::new(inner, policy(AutonomyLevel::Full));
        assert_eq!(tool.name(), "counting");
        assert_eq!(tool.description(), "counts calls");
        assert!(tool.parameters_schema().is_object());
    }

    #[tokio::test]
    async fn rate_limited_blocks_when_exhausted() {
        // Use a policy with a tiny action budget (1 action per window).
        let sec = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            max_actions_per_hour: 1,
            ..SecurityPolicy::default()
        });
        let (inner, counter) = CountingTool::new();
        let tool = RateLimitedTool::new(inner, sec);

        let r1 = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(r1.success, "first call should succeed");

        let r2 = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(!r2.success, "second call should be rate-limited");
        assert!(r2.error.unwrap().contains("Rate limit exceeded"));
        // Inner tool must NOT have been called on the blocked attempt.
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    // ── PathGuardedTool tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn path_guard_allows_safe_path() {
        let (inner, counter) = CountingTool::new();
        let tool = PathGuardedTool::new(inner, policy(AutonomyLevel::Full));
        let result = tool
            .execute(serde_json::json!({"path": "src/main.rs"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn path_guard_blocks_forbidden_path() {
        let (inner, counter) = CountingTool::new();
        let tool = PathGuardedTool::new(inner, policy(AutonomyLevel::Full));
        let result = tool
            .execute(serde_json::json!({"command": "cat /etc/passwd"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Path blocked"));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "inner must not be called"
        );
    }

    #[tokio::test]
    async fn path_guard_no_path_arg_passes_through() {
        let (inner, counter) = CountingTool::new();
        let tool = PathGuardedTool::new(inner, policy(AutonomyLevel::Full));
        // No recognised path field — wrapper must not block.
        let result = tool
            .execute(serde_json::json!({"value": "hello"}))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn path_guard_custom_extractor() {
        let (inner, counter) = CountingTool::new();
        let tool =
            PathGuardedTool::new(inner, policy(AutonomyLevel::Full)).with_extractor(|args| {
                args.get("target")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            });
        let result = tool
            .execute(serde_json::json!({"target": "/etc/shadow"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Path blocked"));
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    // ── Composition test ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn composed_wrappers_both_enforce() {
        // RateLimited(PathGuarded(CountingTool)) — path check happens inside
        // the rate-limit window, so a forbidden path must still be blocked
        // (and not consume a rate-limit slot).
        let sec = policy(AutonomyLevel::Full);
        let (inner, counter) = CountingTool::new();
        let tool = RateLimitedTool::new(PathGuardedTool::new(inner, sec.clone()), sec);

        let blocked = tool
            .execute(serde_json::json!({"path": "/etc/passwd"}))
            .await
            .unwrap();
        assert!(!blocked.success);
        assert_eq!(counter.load(Ordering::SeqCst), 0);
    }

    // ── LoggedTool tests (§1g: tool-boundary error logger) ──────────────────

    use std::sync::Mutex as StdMutex;

    #[derive(Clone, Default)]
    struct TraceCapture(Arc<StdMutex<Vec<u8>>>);
    struct TraceCaptureWriter(Arc<StdMutex<Vec<u8>>>);

    impl TraceCapture {
        fn as_string(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TraceCapture {
        type Writer = TraceCaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            TraceCaptureWriter(self.0.clone())
        }
    }

    impl std::io::Write for TraceCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn install_trace_capture() -> (TraceCapture, tracing::dispatcher::DefaultGuard) {
        let capture = TraceCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_target(true)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(capture.clone())
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);
        let guard = tracing::dispatcher::set_default(&dispatch);
        (capture, guard)
    }

    /// A tool that always returns Ok(ToolResult { success: false, ... }).
    /// This is the most common failure mode — a tool that handled its
    /// error by returning a diagnostic ToolResult rather than bubbling
    /// up an anyhow::Error. Without a tool-boundary logger, these
    /// silently disappear into the agent loop.
    struct FailingTool;

    #[async_trait]
    impl Tool for FailingTool {
        fn name(&self) -> &str {
            "failing_tool"
        }
        fn description(&self) -> &str {
            "always fails"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("simulated upstream 500".into()),
            })
        }
    }

    /// A tool that always raises an anyhow::Error with a source chain.
    struct PanickingTool;

    #[async_trait]
    impl Tool for PanickingTool {
        fn name(&self) -> &str {
            "panicking_tool"
        }
        fn description(&self) -> &str {
            "always errors"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({})
        }
        async fn execute(&self, _args: serde_json::Value) -> anyhow::Result<ToolResult> {
            let io_err =
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "kernel refused");
            Err(anyhow::Error::new(io_err).context("failed to dial backend"))
        }
    }

    /// RED: wrapping a tool that returns `success=false` in `LoggedTool`
    /// MUST emit a WARN-level tracing event carrying the tool name and
    /// the error string, so operators grepping `grep 'tool=' /var/log`
    /// see every silent failure.
    #[tokio::test]
    async fn logged_tool_emits_warn_on_success_false() {
        let (capture, guard) = install_trace_capture();

        let tool = LoggedTool::new(FailingTool);
        let result = tool
            .execute(serde_json::json!({"any": "input"}))
            .await
            .unwrap();
        assert!(!result.success);

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("WARN"),
            "expected WARN level tracing event, got:\n{logs}"
        );
        assert!(
            logs.contains("tool_boundary"),
            "expected 'tool_boundary' target in logs, got:\n{logs}"
        );
        assert!(
            logs.contains("failing_tool"),
            "expected tool name in logs, got:\n{logs}"
        );
        assert!(
            logs.contains("simulated upstream 500"),
            "expected error text from ToolResult.error, got:\n{logs}"
        );
    }

    /// RED: wrapping a tool that raises `anyhow::Error` in `LoggedTool`
    /// MUST emit an ERROR-level tracing event carrying the tool name,
    /// the error message, and the full source chain (so the ConnectionRefused
    /// from std::io is visible, not just anyhow's outer context).
    #[tokio::test]
    async fn logged_tool_emits_error_with_chain_on_anyhow_err() {
        let (capture, guard) = install_trace_capture();

        let tool = LoggedTool::new(PanickingTool);
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err(), "expected Err from PanickingTool");

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("ERROR"),
            "expected ERROR level tracing event, got:\n{logs}"
        );
        assert!(
            logs.contains("tool_boundary"),
            "expected 'tool_boundary' target, got:\n{logs}"
        );
        assert!(
            logs.contains("panicking_tool"),
            "expected tool name in logs, got:\n{logs}"
        );
        assert!(
            logs.contains("error_chain"),
            "expected 'error_chain' structured field, got:\n{logs}"
        );
        assert!(
            logs.contains("kernel refused"),
            "expected std::io root cause 'kernel refused' in chain, got:\n{logs}"
        );
        assert!(
            logs.contains("failed to dial backend"),
            "expected anyhow context 'failed to dial backend', got:\n{logs}"
        );
    }

    /// RED: wrapping a tool that returns `success=true` in `LoggedTool`
    /// MUST NOT emit a WARN/ERROR event (only DEBUG). Logging every
    /// success at WARN level would drown out real signal.
    #[tokio::test]
    async fn logged_tool_does_not_warn_on_success() {
        let (capture, guard) = install_trace_capture();

        let (inner, counter) = CountingTool::new();
        let tool = LoggedTool::new(inner);
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        assert!(result.success);
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        drop(guard);
        let logs = capture.as_string();

        assert!(
            !logs.contains("WARN"),
            "success path must not emit WARN events, got:\n{logs}"
        );
        assert!(
            !logs.contains("ERROR"),
            "success path must not emit ERROR events, got:\n{logs}"
        );
        // Still expect a DEBUG event so operators can trace call flow
        // at RUST_LOG=debug.
        assert!(
            logs.contains("DEBUG") && logs.contains("counting"),
            "expected DEBUG trace with tool name on success, got:\n{logs}"
        );
    }

    /// Delegation: `LoggedTool` must pass through name/description/schema
    /// without modification.
    #[tokio::test]
    async fn logged_tool_delegates_metadata() {
        let (inner, _) = CountingTool::new();
        let tool = LoggedTool::new(inner);
        assert_eq!(tool.name(), "counting");
        assert_eq!(tool.description(), "counts calls");
        assert!(tool.parameters_schema().is_object());
    }
}
