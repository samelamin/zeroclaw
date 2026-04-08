//! Playwright-MCP sidecar client.
//!
//! Communicates with `@playwright/mcp` HTTP server via JSON-RPC 2.0.
//! Start the sidecar with: npx @playwright/mcp --port 3000

use anyhow::Context;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::tools::browser::BrowserAction;

// ── TabIdAllocator ─────────────────────────────────────────────────────────

/// Atomic tab-ID allocator with ID recycling.
///
/// Provides monotonically increasing IDs. Released IDs are recycled
/// (smallest-first) to keep IDs compact during long sessions.
pub struct TabIdAllocator {
    counter: AtomicU64,
    released: Mutex<BTreeSet<u64>>,
}

impl Default for TabIdAllocator {
    fn default() -> Self { Self::new() }
}

impl TabIdAllocator {
    pub fn new() -> Self {
        Self {
            counter: AtomicU64::new(0),
            released: Mutex::new(BTreeSet::new()),
        }
    }

    /// Acquire a new tab ID. Reuses the smallest released ID if available.
    pub fn acquire(&self) -> u64 {
        if let Ok(mut set) = self.released.lock() {
            if let Some(&id) = set.iter().next() {
                set.remove(&id);
                return id;
            }
        }
        self.counter.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Release a tab ID back to the pool.
    pub fn release(&self, id: u64) {
        if let Ok(mut set) = self.released.lock() {
            set.insert(id);
        }
    }
}

// ── BrowserAction → playwright-mcp tool mapping ────────────────────────────

/// Map a `BrowserAction` to a `(playwright_mcp_tool_name, arguments_json)` pair.
///
/// Returns `Err` only for unmappable variants (not expected in practice).
pub fn action_to_mcp_tool(action: BrowserAction) -> anyhow::Result<(&'static str, Value)> {
    Ok(match action {
        BrowserAction::Open { url } => (
            "browser_navigate",
            json!({ "url": url }),
        ),
        BrowserAction::Snapshot { .. } => (
            "browser_snapshot",
            json!({}),
        ),
        BrowserAction::Click { selector } => (
            "browser_click",
            json!({ "selector": selector }),
        ),
        BrowserAction::Fill { selector, value } => (
            "browser_fill",
            json!({ "selector": selector, "value": value }),
        ),
        BrowserAction::Type { selector, text } => (
            "browser_type",
            json!({ "selector": selector, "text": text }),
        ),
        BrowserAction::GetText { selector } => (
            "browser_evaluate",
            json!({ "expression": format!("document.querySelector('{sel}')?.textContent ?? ''", sel = selector.replace('\'', "\\'")) }),
        ),
        BrowserAction::GetTitle => (
            "browser_evaluate",
            json!({ "expression": "document.title" }),
        ),
        BrowserAction::GetUrl => (
            "browser_evaluate",
            json!({ "expression": "window.location.href" }),
        ),
        BrowserAction::Screenshot { .. } => (
            "browser_screenshot",
            json!({}),
        ),
        BrowserAction::Wait { selector, ms, text } => {
            let mut args = json!({});
            if let Some(sel) = selector {
                args["selector"] = Value::String(sel);
            }
            if let Some(t) = ms {
                args["timeout"] = Value::Number(t.into());
            }
            if let Some(txt) = text {
                args["text"] = Value::String(txt);
            }
            ("browser_wait_for", args)
        }
        BrowserAction::Press { key } => (
            "browser_press_key",
            json!({ "key": key }),
        ),
        BrowserAction::Hover { selector } => (
            "browser_hover",
            json!({ "selector": selector }),
        ),
        BrowserAction::Scroll { direction, pixels } => {
            let mut args = json!({ "direction": direction });
            if let Some(px) = pixels {
                args["distance"] = Value::Number(px.into());
            }
            ("browser_scroll", args)
        }
        BrowserAction::IsVisible { selector } => (
            "browser_evaluate",
            json!({ "expression": format!("!!document.querySelector('{}')", selector.replace('\'', "\\'")) }),
        ),
        BrowserAction::Close => (
            "browser_close",
            json!({}),
        ),
        BrowserAction::Find { by, value, action, fill_value } => {
            let selector = match by.as_str() {
                "role"        => format!("[role=\"{value}\"]"),
                "text"        => format!("text={value}"),
                "label"       => format!("[aria-label=\"{value}\"]"),
                "placeholder" => format!("[placeholder=\"{value}\"]"),
                "testid"      => format!("[data-testid=\"{value}\"]"),
                other => anyhow::bail!("Unknown Find locator type: {other}"),
            };
            match action.as_str() {
                "click"  => ("browser_click",  json!({ "selector": selector })),
                "fill"   => ("browser_fill",   json!({ "selector": selector, "value": fill_value.unwrap_or_default() })),
                "hover"  => ("browser_hover",  json!({ "selector": selector })),
                "text"   => ("browser_evaluate", json!({ "expression": format!("document.querySelector('{sel}')?.textContent", sel = selector.replace('\'', "\\'")) })),
                other => anyhow::bail!("Unknown Find action: {other}"),
            }
        }
    })
}

// ── PlaywrightMcpClient ────────────────────────────────────────────────────

/// HTTP client for the `@playwright/mcp` JSON-RPC sidecar.
///
/// Posts MCP call-tool requests to `{endpoint}/message`.
pub struct PlaywrightMcpClient {
    endpoint: String,
    api_key: Option<String>,
    timeout_ms: u64,
    req_id: AtomicU64,
}

impl PlaywrightMcpClient {
    pub fn new(endpoint: impl Into<String>, api_key: Option<String>, timeout_ms: u64) -> Self {
        let raw = endpoint.into();
        let endpoint = raw.trim_end_matches('/').to_string();
        Self {
            endpoint,
            api_key,
            timeout_ms,
            req_id: AtomicU64::new(0),
        }
    }

    /// Single attempt: POST to `{endpoint}/message` with MCP call-tool body.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        let id = self.req_id.fetch_add(1, Ordering::Relaxed) + 1;
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            }
        });

        let url = format!("{}/message", self.endpoint);
        let client = crate::config::build_runtime_proxy_client("tool.browser.playwright_mcp");

        let mut req = client
            .post(&url)
            .timeout(Duration::from_millis(self.timeout_ms))
            .json(&body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.with_context(|| {
            format!("playwright-mcp: failed to POST to {url}")
        })?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("playwright-mcp: sidecar returned HTTP {status} for tool '{tool_name}'");
        }

        let json_resp: Value = resp.json().await.context("playwright-mcp: failed to parse JSON-RPC response")?;

        if let Some(err) = json_resp.get("error") {
            anyhow::bail!("playwright-mcp: tool '{}' error: {}", tool_name, err);
        }

        let result = json_resp.get("result").cloned().unwrap_or(Value::Null);
        Ok(result)
    }

    /// Call with exponential-backoff retry: delays 100ms → 200ms → 400ms.
    /// Up to 3 total attempts.
    pub async fn call_tool_with_retry(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        const BACKOFF_MS: [u64; 2] = [100, 200];
        let mut last_err = anyhow::anyhow!("no attempts made");
        for (attempt, delay_ms) in BACKOFF_MS.iter().enumerate() {
            match self.call_tool(tool_name, arguments.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    tracing::warn!(
                        target: "playwright_mcp",
                        attempt = attempt + 1,
                        delay_ms,
                        error = %e,
                        "playwright-mcp call failed, retrying"
                    );
                    last_err = e;
                    tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
                }
            }
        }
        // Final attempt (no sleep after)
        self.call_tool(tool_name, arguments).await.map_err(|e| {
            anyhow::anyhow!("playwright-mcp: all 3 attempts failed. Last error: {e}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::browser::BrowserAction;

    // TabIdAllocator tests
    #[test]
    fn tab_id_first_acquire_returns_one() {
        let alloc = TabIdAllocator::new();
        assert_eq!(alloc.acquire(), 1);
    }

    #[test]
    fn tab_id_second_acquire_returns_two() {
        let alloc = TabIdAllocator::new();
        alloc.acquire();
        assert_eq!(alloc.acquire(), 2);
    }

    #[test]
    fn tab_id_released_id_is_reused() {
        let alloc = TabIdAllocator::new();
        let id = alloc.acquire();
        alloc.release(id);
        assert_eq!(alloc.acquire(), id);
    }

    // action_to_mcp_tool tests
    #[test]
    fn open_maps_to_browser_navigate() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Open { url: "https://x.com".into() }).unwrap();
        assert_eq!(tool, "browser_navigate");
        assert_eq!(args["url"], "https://x.com");
    }

    #[test]
    fn click_maps_to_browser_click() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Click { selector: "#btn".into() }).unwrap();
        assert_eq!(tool, "browser_click");
        assert_eq!(args["selector"], "#btn");
    }

    #[test]
    fn fill_maps_to_browser_fill_with_value() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Fill { selector: "#inp".into(), value: "hello".into() }).unwrap();
        assert_eq!(tool, "browser_fill");
        assert_eq!(args["value"], "hello");
    }

    #[test]
    fn type_maps_to_browser_type() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Type { selector: ".field".into(), text: "world".into() }).unwrap();
        assert_eq!(tool, "browser_type");
        assert_eq!(args["text"], "world");
    }

    #[test]
    fn press_maps_to_browser_press_key() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Press { key: "Enter".into() }).unwrap();
        assert_eq!(tool, "browser_press_key");
        assert_eq!(args["key"], "Enter");
    }

    #[test]
    fn screenshot_maps_to_browser_screenshot() {
        let (tool, _) = action_to_mcp_tool(BrowserAction::Screenshot { path: None, full_page: false }).unwrap();
        assert_eq!(tool, "browser_screenshot");
    }

    #[test]
    fn get_title_maps_to_browser_evaluate_with_title_expr() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::GetTitle).unwrap();
        assert_eq!(tool, "browser_evaluate");
        assert!(args["expression"].as_str().unwrap().contains("title"));
    }

    #[test]
    fn get_url_maps_to_browser_evaluate_with_href_expr() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::GetUrl).unwrap();
        assert_eq!(tool, "browser_evaluate");
        assert!(args["expression"].as_str().unwrap().contains("href") || args["expression"].as_str().unwrap().contains("location"));
    }

    #[test]
    fn close_maps_to_browser_close() {
        let (tool, _) = action_to_mcp_tool(BrowserAction::Close).unwrap();
        assert_eq!(tool, "browser_close");
    }

    #[test]
    fn snapshot_maps_to_browser_snapshot() {
        let (tool, _) = action_to_mcp_tool(BrowserAction::Snapshot { interactive_only: false, compact: false, depth: None }).unwrap();
        assert_eq!(tool, "browser_snapshot");
    }

    #[test]
    fn hover_maps_to_browser_hover() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Hover { selector: ".item".into() }).unwrap();
        assert_eq!(tool, "browser_hover");
        assert_eq!(args["selector"], ".item");
    }

    #[test]
    fn scroll_maps_to_browser_scroll() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Scroll { direction: "down".into(), pixels: Some(300) }).unwrap();
        assert_eq!(tool, "browser_scroll");
        assert_eq!(args["direction"], "down");
    }

    #[test]
    fn wait_with_selector_maps_to_browser_wait_for() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Wait { selector: Some("#loaded".into()), ms: None, text: None }).unwrap();
        assert_eq!(tool, "browser_wait_for");
        assert_eq!(args["selector"], "#loaded");
    }

    #[test]
    fn find_click_by_role_maps_to_browser_click() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Find {
            by: "role".into(),
            value: "button".into(),
            action: "click".into(),
            fill_value: None,
        }).unwrap();
        assert_eq!(tool, "browser_click");
        assert!(args["selector"].as_str().unwrap().contains("button"));
    }

    // PlaywrightMcpClient HTTP tests
    #[tokio::test]
    async fn client_posts_to_message_endpoint() {
        use wiremock::{MockServer, Mock, ResponseTemplate, matchers::{method, path}};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/message"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "content": [{ "type": "text", "text": "done" }] }
            })))
            .mount(&server)
            .await;
        let client = PlaywrightMcpClient::new(server.uri(), None, 30_000);
        let result = client.call_tool("browser_navigate", serde_json::json!({ "url": "https://x.com" })).await;
        assert!(result.is_ok(), "expected ok, got: {result:?}");
    }

    #[tokio::test]
    async fn client_sends_bearer_token() {
        use wiremock::{MockServer, Mock, ResponseTemplate, matchers::{method, path, header}};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/message"))
            .and(header("authorization", "Bearer tok123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "jsonrpc": "2.0", "id": 1,
                "result": { "content": [{ "type": "text", "text": "authed" }] }
            })))
            .mount(&server)
            .await;
        let client = PlaywrightMcpClient::new(server.uri(), Some("tok123".into()), 30_000);
        let result = client.call_tool("browser_navigate", serde_json::json!({})).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn client_retries_and_succeeds_on_second_attempt() {
        use wiremock::{MockServer, Mock, ResponseTemplate, matchers::{method, path}};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let server = MockServer::start().await;
        let count = Arc::new(AtomicUsize::new(0));
        let count2 = count.clone();
        Mock::given(method("POST"))
            .and(path("/message"))
            .respond_with(move |_req: &wiremock::Request| {
                if count2.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(503)
                } else {
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "jsonrpc": "2.0", "id": 1,
                        "result": { "content": [{ "type": "text", "text": "ok" }] }
                    }))
                }
            })
            .mount(&server)
            .await;
        let client = PlaywrightMcpClient::new(server.uri(), None, 30_000);
        let result = client.call_tool_with_retry("browser_navigate", serde_json::json!({})).await;
        assert!(result.is_ok());
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn client_fails_after_all_retries_exhausted() {
        use wiremock::{MockServer, Mock, ResponseTemplate, matchers::{method, path}};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/message"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let client = PlaywrightMcpClient::new(server.uri(), None, 30_000);
        let result = client.call_tool_with_retry("browser_navigate", serde_json::json!({})).await;
        assert!(result.is_err());
    }
}
