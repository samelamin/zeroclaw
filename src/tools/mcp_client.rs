//! MCP (Model Context Protocol) client — connects to external tool servers.
//!
//! Supports multiple transports: stdio (spawn local process), HTTP, and SSE.

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(not(target_has_atomic = "64"))]
use std::sync::atomic::AtomicU32;
#[cfg(target_has_atomic = "64")]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::json;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};

use crate::config::schema::McpServerConfig;
use crate::tools::mcp_protocol::{
    JsonRpcRequest, MCP_PROTOCOL_VERSION, McpToolDef, McpToolsListResult,
};
use crate::tools::mcp_transport::{McpTransportConn, create_transport};

/// Timeout for receiving a response from an MCP server during init/list.
/// Prevents a hung server from blocking the daemon indefinitely.
const RECV_TIMEOUT_SECS: u64 = 30;

/// Default timeout for tool calls (seconds) when not configured per-server.
const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 180;

/// Maximum allowed tool call timeout (seconds) — hard safety ceiling.
const MAX_TOOL_TIMEOUT_SECS: u64 = 600;

// ── Circuit breaker ───────────────────────────────────────────────────────

/// Circuit breaker for MCP server health tracking.
/// Tracks consecutive failures and backs off when a server is unhealthy.
pub(crate) struct CircuitBreaker {
    consecutive_failures: AtomicU32,
    #[cfg(target_has_atomic = "64")]
    last_failure_epoch_ms: AtomicU64,
    #[cfg(not(target_has_atomic = "64"))]
    last_failure_epoch_ms: AtomicU32,
    /// Number of consecutive failures before the circuit opens.
    failure_threshold: u32,
    /// Backoff duration in milliseconds after circuit opens.
    backoff_ms: u64,
}

impl CircuitBreaker {
    pub fn new(failure_threshold: u32, backoff_ms: u64) -> Self {
        Self {
            consecutive_failures: AtomicU32::new(0),
            #[cfg(target_has_atomic = "64")]
            last_failure_epoch_ms: AtomicU64::new(0),
            #[cfg(not(target_has_atomic = "64"))]
            last_failure_epoch_ms: AtomicU32::new(0),
            failure_threshold,
            backoff_ms,
        }
    }

    /// Record a successful call — resets the failure counter.
    pub fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
    }

    /// Record a failed call — increments failure counter and updates timestamp.
    pub fn record_failure(&self) {
        self.consecutive_failures.fetch_add(1, Ordering::Relaxed);
        #[allow(clippy::cast_possible_truncation)] // millis since epoch fits u64 for ~584M years
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        #[cfg(target_has_atomic = "64")]
        self.last_failure_epoch_ms.store(now, Ordering::Relaxed);
        #[cfg(not(target_has_atomic = "64"))]
        self.last_failure_epoch_ms
            .store(now as u32, Ordering::Relaxed);
    }

    /// Check if the circuit is open (server considered unhealthy).
    /// Returns true if we should skip this server.
    pub fn is_open(&self) -> bool {
        let failures = self.consecutive_failures.load(Ordering::Relaxed);
        if failures < self.failure_threshold {
            return false;
        }
        // Check if backoff period has elapsed
        #[cfg(target_has_atomic = "64")]
        let last = self.last_failure_epoch_ms.load(Ordering::Relaxed);
        #[cfg(not(target_has_atomic = "64"))]
        let last = self.last_failure_epoch_ms.load(Ordering::Relaxed) as u64;
        #[allow(clippy::cast_possible_truncation)]
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        // If backoff hasn't elapsed, circuit is still open
        now.saturating_sub(last) < self.backoff_ms
    }

    /// Get the number of consecutive failures.
    pub fn failure_count(&self) -> u32 {
        self.consecutive_failures.load(Ordering::Relaxed)
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new(3, 30_000) // 3 failures, 30s backoff
    }
}

// ── Internal server state ──────────────────────────────────────────────────

struct McpServerInner {
    config: McpServerConfig,
    transport: Box<dyn McpTransportConn>,
    #[cfg(target_has_atomic = "64")]
    next_id: AtomicU64,
    #[cfg(not(target_has_atomic = "64"))]
    next_id: AtomicU32,
    tools: Vec<McpToolDef>,
}

// ── McpServer ──────────────────────────────────────────────────────────────

/// A live connection to one MCP server (any transport).
#[derive(Clone)]
pub struct McpServer {
    inner: Arc<Mutex<McpServerInner>>,
    /// Server name cached outside the mutex for circuit breaker logging.
    server_name: Arc<String>,
    circuit_breaker: Arc<CircuitBreaker>,
}

impl McpServer {
    /// Connect to the server, perform the initialize handshake, and fetch the tool list.
    pub async fn connect(config: McpServerConfig) -> Result<Self> {
        // Create transport based on config
        let mut transport = create_transport(&config).with_context(|| {
            format!(
                "failed to create transport for MCP server `{}`",
                config.name
            )
        })?;

        // Initialize handshake
        let id = 1u64;
        let init_req = JsonRpcRequest::new(
            id,
            "initialize",
            json!({
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "zeroclaw",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
        );

        let init_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&init_req),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out after {}s waiting for initialize response",
                config.name, RECV_TIMEOUT_SECS
            )
        })??;

        if init_resp.error.is_some() {
            bail!(
                "MCP server `{}` rejected initialize: {:?}",
                config.name,
                init_resp.error
            );
        }

        // Notify server that client is initialized (no response expected for notifications)
        // For notifications, we send but don't wait for response
        let notif = JsonRpcRequest::notification("notifications/initialized", json!({}));
        // Best effort - ignore errors for notifications
        let _ = transport.send_and_recv(&notif).await;

        // Fetch available tools
        let id = 2u64;
        let list_req = JsonRpcRequest::new(id, "tools/list", json!({}));

        let list_resp = timeout(
            Duration::from_secs(RECV_TIMEOUT_SECS),
            transport.send_and_recv(&list_req),
        )
        .await
        .with_context(|| {
            format!(
                "MCP server `{}` timed out after {}s waiting for tools/list response",
                config.name, RECV_TIMEOUT_SECS
            )
        })??;

        let result = list_resp
            .result
            .ok_or_else(|| anyhow!("tools/list returned no result from `{}`", config.name))?;
        let tool_list: McpToolsListResult = serde_json::from_value(result)
            .with_context(|| format!("failed to parse tools/list from `{}`", config.name))?;

        let tool_count = tool_list.tools.len();

        let inner = McpServerInner {
            config,
            transport,
            #[cfg(target_has_atomic = "64")]
            next_id: AtomicU64::new(3), // Start at 3 since we used 1 and 2
            #[cfg(not(target_has_atomic = "64"))]
            next_id: AtomicU32::new(3), // Start at 3 since we used 1 and 2
            tools: tool_list.tools,
        };

        let name = inner.config.name.clone();
        tracing::info!(
            "MCP server `{}` connected — {} tool(s) available",
            name,
            tool_count
        );

        Ok(Self {
            inner: Arc::new(Mutex::new(inner)),
            server_name: Arc::new(name),
            circuit_breaker: Arc::new(CircuitBreaker::default()),
        })
    }

    /// Tools advertised by this server.
    pub async fn tools(&self) -> Vec<McpToolDef> {
        self.inner.lock().await.tools.clone()
    }

    /// Server display name.
    pub async fn name(&self) -> String {
        self.inner.lock().await.config.name.clone()
    }

    /// Call a tool on this server. Returns the raw JSON result.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value> {
        // Check circuit breaker before acquiring the lock.
        if self.circuit_breaker.is_open() {
            let failures = self.circuit_breaker.failure_count();
            tracing::warn!(
                server = %self.server_name,
                failures,
                "MCP server circuit open — skipping call"
            );
            return Err(anyhow!(
                "MCP server '{}' is temporarily unavailable \
                 (circuit open after {} consecutive failures)",
                self.server_name,
                failures
            ));
        }

        let mut inner = self.inner.lock().await;
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed) as u64;
        let req = JsonRpcRequest::new(
            id,
            "tools/call",
            json!({ "name": tool_name, "arguments": arguments }),
        );

        // Use per-server tool timeout if configured, otherwise default.
        // Cap at MAX_TOOL_TIMEOUT_SECS for safety.
        let tool_timeout = inner
            .config
            .tool_timeout_secs
            .unwrap_or(DEFAULT_TOOL_TIMEOUT_SECS)
            .min(MAX_TOOL_TIMEOUT_SECS);

        let result = timeout(
            Duration::from_secs(tool_timeout),
            inner.transport.send_and_recv(&req),
        )
        .await
        .map_err(|_| {
            anyhow!(
                "MCP server `{}` timed out after {}s during tool call `{tool_name}`",
                inner.config.name,
                tool_timeout
            )
        })?
        .with_context(|| {
            format!(
                "MCP server `{}` error during tool call `{tool_name}`",
                inner.config.name
            )
        });

        match &result {
            Ok(resp) if resp.error.is_some() => {
                self.circuit_breaker.record_failure();
                let err = resp.error.as_ref().unwrap();
                bail!("MCP tool `{tool_name}` error {}: {}", err.code, err.message);
            }
            Ok(_) => {
                self.circuit_breaker.record_success();
            }
            Err(_) => {
                self.circuit_breaker.record_failure();
            }
        }

        let resp = result?;
        Ok(resp.result.unwrap_or(serde_json::Value::Null))
    }
}

// ── McpRegistry ───────────────────────────────────────────────────────────

/// Registry of all connected MCP servers, with a flat tool index.
pub struct McpRegistry {
    servers: Vec<McpServer>,
    /// prefixed_name → (server_index, original_tool_name)
    tool_index: HashMap<String, (usize, String)>,
}

impl McpRegistry {
    /// Connect to all configured servers. Non-fatal: failures are logged and skipped.
    pub async fn connect_all(configs: &[McpServerConfig]) -> Result<Self> {
        let mut servers = Vec::new();
        let mut tool_index = HashMap::new();

        for config in configs {
            match McpServer::connect(config.clone()).await {
                Ok(server) => {
                    let server_idx = servers.len();
                    // Collect tools while holding the lock once, then release
                    let tools = server.tools().await;
                    for tool in &tools {
                        // Prefix prevents name collisions across servers
                        let prefixed = format!("{}__{}", config.name, tool.name);
                        tool_index.insert(prefixed, (server_idx, tool.name.clone()));
                    }
                    servers.push(server);
                }
                // Non-fatal — log and continue with remaining servers
                Err(e) => {
                    tracing::error!("Failed to connect to MCP server `{}`: {:#}", config.name, e);
                }
            }
        }

        Ok(Self {
            servers,
            tool_index,
        })
    }

    /// All prefixed tool names across all connected servers.
    pub fn tool_names(&self) -> Vec<String> {
        self.tool_index.keys().cloned().collect()
    }

    /// Tool definition for a given prefixed name (cloned).
    pub async fn get_tool_def(&self, prefixed_name: &str) -> Option<McpToolDef> {
        let (server_idx, original_name) = self.tool_index.get(prefixed_name)?;
        let inner = self.servers[*server_idx].inner.lock().await;
        inner
            .tools
            .iter()
            .find(|t| &t.name == original_name)
            .cloned()
    }

    /// Execute a tool by prefixed name.
    pub async fn call_tool(
        &self,
        prefixed_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String> {
        let (server_idx, original_name) = self
            .tool_index
            .get(prefixed_name)
            .ok_or_else(|| anyhow!("unknown MCP tool `{prefixed_name}`"))?;
        let result = self.servers[*server_idx]
            .call_tool(original_name, arguments)
            .await?;
        serde_json::to_string_pretty(&result)
            .with_context(|| format!("failed to serialize result of MCP tool `{prefixed_name}`"))
    }

    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    pub fn tool_count(&self) -> usize {
        self.tool_index.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::McpTransport;

    #[test]
    fn tool_name_prefix_format() {
        let prefixed = format!("{}__{}", "filesystem", "read_file");
        assert_eq!(prefixed, "filesystem__read_file");
    }

    #[tokio::test]
    async fn connect_nonexistent_command_fails_cleanly() {
        // A command that doesn't exist should fail at spawn, not panic.
        let config = McpServerConfig {
            name: "nonexistent".to_string(),
            command: "/usr/bin/this_binary_does_not_exist_zeroclaw_test".to_string(),
            args: vec![],
            env: std::collections::HashMap::default(),
            tool_timeout_secs: None,
            transport: McpTransport::Stdio,
            url: None,
            headers: std::collections::HashMap::default(),
        };
        let result = McpServer::connect(config).await;
        assert!(result.is_err());
        let msg = result.err().unwrap().to_string();
        assert!(msg.contains("failed to create transport"), "got: {msg}");
    }

    #[tokio::test]
    async fn connect_all_nonfatal_on_single_failure() {
        // If one server config is bad, connect_all should succeed (with 0 servers).
        let configs = vec![McpServerConfig {
            name: "bad".to_string(),
            command: "/usr/bin/does_not_exist_zc_test".to_string(),
            args: vec![],
            env: std::collections::HashMap::default(),
            tool_timeout_secs: None,
            transport: McpTransport::Stdio,
            url: None,
            headers: std::collections::HashMap::default(),
        }];
        let registry = McpRegistry::connect_all(&configs)
            .await
            .expect("connect_all should not fail");
        assert!(registry.is_empty());
        assert_eq!(registry.tool_count(), 0);
    }

    #[test]
    fn http_transport_requires_url() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransport::Http,
            ..Default::default()
        };
        let result = create_transport(&config);
        assert!(result.is_err());
    }

    #[test]
    fn sse_transport_requires_url() {
        let config = McpServerConfig {
            name: "test".into(),
            transport: McpTransport::Sse,
            ..Default::default()
        };
        let result = create_transport(&config);
        assert!(result.is_err());
    }

    // ── Empty registry (no servers) ────────────────────────────────────────

    #[tokio::test]
    async fn empty_registry_is_empty() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all on empty slice should succeed");
        assert!(registry.is_empty());
        assert_eq!(registry.server_count(), 0);
        assert_eq!(registry.tool_count(), 0);
    }

    #[tokio::test]
    async fn empty_registry_tool_names_is_empty() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        assert!(registry.tool_names().is_empty());
    }

    #[tokio::test]
    async fn empty_registry_get_tool_def_returns_none() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        let result = registry.get_tool_def("nonexistent__tool").await;
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn empty_registry_call_tool_unknown_name_returns_error() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        let err = registry
            .call_tool("nonexistent__tool", serde_json::json!({}))
            .await
            .expect_err("should fail for unknown tool");
        assert!(err.to_string().contains("unknown MCP tool"), "got: {err}");
    }

    #[tokio::test]
    async fn connect_all_empty_gives_zero_servers() {
        let registry = McpRegistry::connect_all(&[])
            .await
            .expect("connect_all should succeed");
        // Verify all three count methods agree on zero.
        assert_eq!(registry.server_count(), 0);
        assert_eq!(registry.tool_count(), 0);
        assert!(registry.is_empty());
    }

    // ── Circuit breaker tests ─────────────────────────────────────────────

    #[test]
    fn circuit_breaker_stays_closed_under_threshold() {
        let cb = CircuitBreaker::new(3, 30_000);
        cb.record_failure();
        cb.record_failure();
        assert!(!cb.is_open());
    }

    #[test]
    fn circuit_breaker_opens_at_threshold() {
        let cb = CircuitBreaker::new(3, 60_000); // long backoff so it stays open
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open());
    }

    #[test]
    fn circuit_breaker_resets_on_success() {
        let cb = CircuitBreaker::new(3, 60_000);
        cb.record_failure();
        cb.record_failure();
        cb.record_success();
        assert_eq!(cb.failure_count(), 0);
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        // Now it should be open
        assert!(cb.is_open());
    }

    #[test]
    fn circuit_breaker_closes_after_backoff() {
        let cb = CircuitBreaker::new(3, 1); // 1ms backoff
        cb.record_failure();
        cb.record_failure();
        cb.record_failure();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(!cb.is_open()); // backoff elapsed
    }

    #[test]
    fn circuit_breaker_default_values() {
        let cb = CircuitBreaker::default();
        assert_eq!(cb.failure_threshold, 3);
        assert_eq!(cb.backoff_ms, 30_000);
        assert_eq!(cb.failure_count(), 0);
        assert!(!cb.is_open());
    }
}
