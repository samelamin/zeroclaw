//! Playwright-MCP sidecar client.
//!
//! Communicates with `@playwright/mcp` HTTP server via JSON-RPC 2.0.
//! Start the sidecar with: npx @playwright/mcp --port 3000

use anyhow::Context;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Mutex;
use std::sync::Arc;
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
fn snapshot_ref(selector: &str) -> Option<&str> {
    let trimmed = selector.trim();
    let candidate = trimmed.strip_prefix('@').unwrap_or(trimmed);
    let mut chars = candidate.chars();
    match chars.next() {
        Some('e') if chars.all(|ch| ch.is_ascii_digit()) => Some(candidate),
        _ => None,
    }
}

fn is_snapshot_ref(selector: &str) -> bool {
    snapshot_ref(selector).is_some()
}

fn text_selector(selector: &str) -> Option<&str> {
    selector.strip_prefix("text=").map(str::trim).filter(|value| !value.is_empty())
}

fn js_string(value: &str) -> anyhow::Result<String> {
    serde_json::to_string(value).context("playwright-mcp: failed to encode JavaScript string literal")
}

fn fill_nearby_field_function(value: &str) -> anyhow::Result<String> {
    let value_js = js_string(value)?;
    Ok(format!(
        "(element) => {{ \
            const isField = (node) => !!node && (node.matches?.('input, textarea, select, [contenteditable=\"true\"]') || node.isContentEditable); \
            const findFieldNear = (node) => {{ \
                if (!node) return null; \
                if (isField(node)) return node; \
                if (node instanceof HTMLLabelElement) {{ \
                    if (node.control) return node.control; \
                    if (node.htmlFor) return document.getElementById(node.htmlFor); \
                }} \
                const within = node.querySelector?.('input, textarea, select, [contenteditable=\"true\"]'); \
                if (within) return within; \
                const parent = node.closest?.('label, form, section, article, div, fieldset'); \
                const parentField = parent?.querySelector?.('input, textarea, select, [contenteditable=\"true\"]'); \
                if (parentField) return parentField; \
                let sibling = node.nextElementSibling; \
                while (sibling) {{ \
                    const nested = isField(sibling) ? sibling : sibling.querySelector?.('input, textarea, select, [contenteditable=\"true\"]'); \
                    if (nested) return nested; \
                    sibling = sibling.nextElementSibling; \
                }} \
                sibling = node.previousElementSibling; \
                while (sibling) {{ \
                    const nested = isField(sibling) ? sibling : sibling.querySelector?.('input, textarea, select, [contenteditable=\"true\"]'); \
                    if (nested) return nested; \
                    sibling = sibling.previousElementSibling; \
                }} \
                return null; \
            }}; \
            const field = findFieldNear(element); \
            if (!field) return false; \
            field.focus?.(); \
            if ('value' in field) {{ \
                field.value = {value}; \
            }} else {{ \
                field.textContent = {value}; \
            }} \
            field.dispatchEvent(new Event('input', {{ bubbles: true }})); \
            field.dispatchEvent(new Event('change', {{ bubbles: true }})); \
            return true; \
        }}",
        value = value_js,
    ))
}

fn click_args(selector: String) -> anyhow::Result<(&'static str, Value)> {
    if let Some(reference) = snapshot_ref(&selector) {
        return Ok(("browser_click", json!({ "ref": reference })));
    }

    let selector_js = js_string(&selector)?;
    Ok((
        "browser_run_code",
        json!({
            "code": format!(
                "async (page) => {{ await page.locator({selector}).first().click(); return page.url(); }}",
                selector = selector_js,
            ),
        }),
    ))
}

fn fill_args(selector: String, value: String) -> anyhow::Result<(&'static str, Value)> {
    if let Some(reference) = snapshot_ref(&selector) {
        let function = fill_nearby_field_function(&value)?;
        return Ok((
            "browser_evaluate",
            json!({
                "ref": reference,
                "function": function,
            }),
        ));
    }

    if let Some(label_text) = text_selector(&selector) {
        let label_js = js_string(label_text)?;
        let fill_fn = fill_nearby_field_function(&value)?;
        return Ok((
            "browser_evaluate",
            json!({
                "function": format!(
                    "() => {{ \
                        const target = {label}; \
                        const normalize = (input) => String(input ?? '').replace(/\\s+/g, ' ').trim().toLowerCase(); \
                        const targetText = normalize(target); \
                        const isField = (node) => !!node && (node.matches?.('input, textarea, select, [contenteditable=\"true\"]') || node.isContentEditable); \
                        const findFieldNear = (node) => {{ \
                            if (!node) return null; \
                            if (isField(node)) return node; \
                            if (node instanceof HTMLLabelElement) {{ \
                                if (node.control) return node.control; \
                                if (node.htmlFor) return document.getElementById(node.htmlFor); \
                            }} \
                            const within = node.querySelector?.('input, textarea, select, [contenteditable=\"true\"]'); \
                            if (within) return within; \
                            const parent = node.closest?.('label, form, section, article, div, fieldset'); \
                            const siblingField = parent?.querySelector?.('input, textarea, select, [contenteditable=\"true\"]'); \
                            if (siblingField) return siblingField; \
                            let sibling = node.nextElementSibling; \
                            while (sibling) {{ \
                                const nested = isField(sibling) ? sibling : sibling.querySelector?.('input, textarea, select, [contenteditable=\"true\"]'); \
                                if (nested) return nested; \
                                sibling = sibling.nextElementSibling; \
                            }} \
                            return null; \
                        }}; \
                        const candidates = Array.from(document.querySelectorAll('label, [aria-label], input, textarea, select, button, div, span, p')); \
                        const match = candidates.find((node) => {{ \
                            const ariaLabel = node.getAttribute?.('aria-label'); \
                            const placeholder = 'placeholder' in node ? node.placeholder : ''; \
                            return [ariaLabel, placeholder, node.textContent].some((entry) => normalize(entry).includes(targetText)); \
                        }}); \
                        const field = findFieldNear(match); \
                        if (!field) return false; \
                        return ({fill_fn})(field); \
                    }}",
                    label = label_js,
                    fill_fn = fill_fn,
                ),
            }),
        ));
    }

    let selector_js = js_string(&selector)?;
    let value_js = js_string(&value)?;
    Ok((
        "browser_run_code",
        json!({
            "code": format!(
                "async (page) => {{ \
                    await page.locator({selector}).first().fill({value}); \
                    return true; \
                }}",
                selector = selector_js,
                value = value_js,
            ),
        }),
    ))
}

fn type_args(selector: String, text: String) -> anyhow::Result<(&'static str, Value)> {
    if let Some(reference) = snapshot_ref(&selector) {
        return Ok(("browser_type", json!({ "ref": reference, "text": text })));
    }

    let selector_js = js_string(&selector)?;
    let text_js = js_string(&text)?;
    Ok((
        "browser_run_code",
        json!({
            "code": format!(
                "async (page) => {{ \
                    await page.locator({selector}).first().type({text}); \
                    return true; \
                }}",
                selector = selector_js,
                text = text_js,
            ),
        }),
    ))
}

fn text_args(selector: String) -> anyhow::Result<(&'static str, Value)> {
    if let Some(reference) = snapshot_ref(&selector) {
        return Ok((
            "browser_evaluate",
            json!({
                "ref": reference,
                "function": "(element) => element?.textContent ?? ''",
            }),
        ));
    }

    let selector_js = js_string(&selector)?;
    Ok((
        "browser_run_code",
        json!({
            "code": format!(
                "async (page) => await page.locator({selector}).first().textContent()",
                selector = selector_js,
            ),
        }),
    ))
}

fn hover_args(selector: String) -> anyhow::Result<(&'static str, Value)> {
    if let Some(reference) = snapshot_ref(&selector) {
        return Ok(("browser_hover", json!({ "ref": reference })));
    }

    let selector_js = js_string(&selector)?;
    Ok((
        "browser_run_code",
        json!({
            "code": format!(
                "async (page) => {{ \
                    await page.locator({selector}).first().hover(); \
                    return true; \
                }}",
                selector = selector_js,
            ),
        }),
    ))
}

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
        BrowserAction::Click { selector } => click_args(selector)?,
        BrowserAction::Fill { selector, value } => fill_args(selector, value)?,
        BrowserAction::Type { selector, text } => type_args(selector, text)?,
        BrowserAction::GetText { selector } => text_args(selector)?,
        BrowserAction::GetTitle => (
            "browser_evaluate",
            json!({ "function": "() => document.title" }),
        ),
        BrowserAction::GetUrl => (
            "browser_evaluate",
            json!({ "function": "() => window.location.href" }),
        ),
        BrowserAction::Screenshot { path, full_page } => {
            let mut args = json!({ "type": "png" });
            if full_page {
                args["fullPage"] = Value::Bool(true);
            }
            if let Some(path) = path {
                if let Some(name) = Path::new(&path).file_name().and_then(|entry| entry.to_str()) {
                    args["filename"] = Value::String(name.to_string());
                }
            }
            ("browser_take_screenshot", args)
        }
        BrowserAction::Wait { selector, ms, text } => {
            let mut args = json!({});
            if let Some(t) = ms {
                args["time"] = serde_json::Number::from_f64((t as f64) / 1000.0)
                    .map(Value::Number)
                    .unwrap_or_else(|| Value::Number(1.into()));
            }
            if let Some(txt) = text {
                args["text"] = Value::String(txt);
            }
            if args != json!({}) {
                ("browser_wait_for", args)
            } else if let Some(sel) = selector {
                text_args(sel)?
            } else {
                ("browser_wait_for", json!({ "time": 1 }))
            }
        }
        BrowserAction::Press { key } => (
            "browser_press_key",
            json!({ "key": key }),
        ),
        BrowserAction::Hover { selector } => hover_args(selector)?,
        BrowserAction::Scroll { direction, pixels } => {
            let distance = pixels.unwrap_or(400);
            let delta = match direction.as_str() {
                "up" => format!("-{}", distance),
                "left" => format!("-{}", distance),
                _ => distance.to_string(),
            };
            let (x, y) = match direction.as_str() {
                "left" | "right" => (delta, "0".to_string()),
                _ => ("0".to_string(), delta),
            };
            (
                "browser_evaluate",
                json!({
                    "function": format!("() => {{ window.scrollBy({{ left: {x}, top: {y}, behavior: 'instant' }}); return {{ x: window.scrollX, y: window.scrollY }}; }}", x = x, y = y),
                }),
            )
        }
        BrowserAction::IsVisible { selector } => {
            if let Some(reference) = snapshot_ref(&selector) {
                (
                    "browser_evaluate",
                    json!({
                        "ref": reference,
                        "function": "(element) => !!element",
                    }),
                )
            } else {
                let selector_js = js_string(&selector)?;
                (
                    "browser_run_code",
                    json!({
                        "code": format!(
                            "async (page) => await page.locator({selector}).first().isVisible().catch(() => false)",
                            selector = selector_js
                        ),
                    }),
                )
            }
        }
        BrowserAction::Close => (
            "browser_close",
            json!({}),
        ),
        BrowserAction::Find { by, value, action, fill_value } => {
            let selector = match by.as_str() {
                "role"        => format!("[role=\"{value}\"]"),
                "text"        => format!("text={value}"),
                "label"       => format!("text={value}"),
                "placeholder" => format!("[placeholder=\"{value}\"]"),
                "testid"      => format!("[data-testid=\"{value}\"]"),
                other => anyhow::bail!("Unknown Find locator type: {other}"),
            };
            match action.as_str() {
                "click"  => click_args(selector)?,
                "fill"   => fill_args(selector, fill_value.unwrap_or_default())?,
                "hover"  => hover_args(selector)?,
                "text"   => text_args(selector)?,
                other => anyhow::bail!("Unknown Find action: {other}"),
            }
        }
    })
}

// ── PlaywrightMcpClient ────────────────────────────────────────────────────

/// Parse a JSON value from an MCP SSE response body.
///
/// MCP HTTP transport returns `text/event-stream` with:
///   event: message\ndata: <json>\n\n
///
/// Extracts the first `data:` line and parses it as JSON.
/// Falls back to direct JSON parse if no SSE framing is present.
fn parse_sse_json(body: &str) -> anyhow::Result<Value> {
    for line in body.lines() {
        let trimmed = line.trim();
        if let Some(data) = trimmed.strip_prefix("data:") {
            let json_str = data.trim();
            if !json_str.is_empty() {
                return serde_json::from_str(json_str)
                    .context("playwright-mcp: failed to parse SSE data as JSON");
            }
        }
    }
    // Fallback: try direct JSON (plain HTTP/JSON response)
    serde_json::from_str(body.trim())
        .context("playwright-mcp: response body is not SSE or valid JSON")
}

/// HTTP client for the `@playwright/mcp` JSON-RPC sidecar.
///
/// Uses the MCP StreamableHTTP transport (`POST {endpoint}/mcp`).
///
/// The session ID is shared via `Arc<tokio::sync::Mutex<Option<String>>>` so that
/// successive browser tool calls within the same agent turn reuse the same Playwright
/// browser session, preserving navigation history, cookies, and form state.
pub struct PlaywrightMcpClient {
    endpoint: String,
    api_key: Option<String>,
    timeout_ms: u64,
    req_id: AtomicU64,
    /// Shared session ID — None means no active session yet.
    session_id: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Last successfully navigated page URL, used to restore page context after session resets.
    last_url: Arc<tokio::sync::Mutex<Option<String>>>,
    /// Safe state-setting actions to replay after session resets so forms and selections survive.
    replayable_actions: Arc<tokio::sync::Mutex<Vec<(String, Value)>>>,
    /// Serialized browser cookies captured from the active session for restoring auth state.
    cookies_json: Arc<tokio::sync::Mutex<Option<String>>>,
}

impl PlaywrightMcpClient {
    pub fn new(
        endpoint: impl Into<String>,
        api_key: Option<String>,
        timeout_ms: u64,
        session_id: Arc<tokio::sync::Mutex<Option<String>>>,
        last_url: Arc<tokio::sync::Mutex<Option<String>>>,
        replayable_actions: Arc<tokio::sync::Mutex<Vec<(String, Value)>>>,
        cookies_json: Arc<tokio::sync::Mutex<Option<String>>>,
    ) -> Self {
        let raw = endpoint.into();
        let endpoint = raw.trim_end_matches('/').to_string();
        Self {
            endpoint,
            api_key,
            timeout_ms,
            req_id: AtomicU64::new(0),
            session_id,
            last_url,
            replayable_actions,
            cookies_json,
        }
    }

    fn mcp_url(&self) -> String {
        format!("{}/mcp", self.endpoint)
    }

    fn build_request(
        &self,
        client: &reqwest::Client,
        session_id: Option<&str>,
        body: &Value,
    ) -> reqwest::RequestBuilder {
        let mut req = client
            .post(self.mcp_url())
            .timeout(Duration::from_millis(self.timeout_ms))
            // MCP StreamableHTTP transport requires both JSON and SSE accept types
            .header("Accept", "application/json, text/event-stream")
            // Force TCP connection close after each request — playwright-mcp @0.0.70
            // has a race condition where sessions get 404 if the same keep-alive
            // connection is reused for multiple sequential requests.
            .header("Connection", "close")
            .json(body);
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        if let Some(sid) = session_id {
            req = req.header("mcp-session-id", sid);
        }
        req
    }

    /// Ensure an active MCP session exists, initializing one if needed.
    /// Returns the session ID (existing or newly created).
    async fn ensure_session(&self, client: &reqwest::Client) -> anyhow::Result<String> {
        {
            let guard = self.session_id.lock().await;
            if let Some(existing) = guard.as_ref() {
                return Ok(existing.clone());
            }
        }

        // Initialize a new session
        let id = self.req_id.fetch_add(1, Ordering::Relaxed) + 1;
        let init_body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "zeroclaw", "version": "0.7.0" }
            }
        });

        let resp = self.build_request(client, None, &init_body)
            .send()
            .await
            .context("playwright-mcp: initialize request failed")?;

        let status = resp.status();
        let new_session_id = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("playwright-mcp: initialize response missing mcp-session-id header"))?;

        let body_text = resp.text().await.context("playwright-mcp: failed to read initialize response")?;

        if !status.is_success() {
            anyhow::bail!("playwright-mcp: initialize returned HTTP {status}: {body_text}");
        }

        let json_resp = parse_sse_json(&body_text)?;
        if let Some(err) = json_resp.get("error") {
            anyhow::bail!("playwright-mcp: initialize error: {err}");
        }

        // Send notifications/initialized and drain the response body so the
        // HTTP connection is cleanly returned to the pool for reuse.
        let notif_body = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        if let Ok(resp) = self.build_request(client, Some(&new_session_id), &notif_body)
            .send()
            .await
        {
            let _ = resp.bytes().await; // drain body to free the connection
        }

        // Persist the session ID for reuse
        *self.session_id.lock().await = Some(new_session_id.clone());
        tracing::debug!(target: "playwright_mcp", session_id = %new_session_id, "MCP session initialized");

        Ok(new_session_id)
    }

    /// Single attempt: reuse (or create) an MCP session and call the given tool.
    /// On session-not-found errors (HTTP 404), clears the stale session and retries once.
    pub async fn call_tool(
        &self,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        // Use a no-pool client for the MCP sidecar — playwright-mcp @0.0.70 has
        // a race condition with connection reuse: sessions get 404 when subsequent
        // requests arrive on a keep-alive connection before the previous response
        // has been fully processed server-side. Using pool_max_idle_per_host(0)
        // forces a fresh TCP connection per request, matching how the Node.js
        // http module behaves by default (which is confirmed to work).
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(self.timeout_ms + 5_000))
            .connect_timeout(Duration::from_secs(5))
            .pool_max_idle_per_host(0) // no connection reuse — each request gets a fresh TCP conn
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        self.call_tool_with_client(&client, tool_name, arguments).await
    }

    async fn call_tool_with_client(
        &self,
        client: &reqwest::Client,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        let requested_url = if tool_name == "browser_navigate" {
            arguments.get("url").and_then(Value::as_str).map(ToOwned::to_owned)
        } else {
            None
        };
        let session_id = self.ensure_session(client).await?;

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

        let resp = self.build_request(client, Some(&session_id), &body)
            .send()
            .await
            .with_context(|| format!("playwright-mcp: tools/call request failed for '{tool_name}'"))?;

        let status = resp.status();

        // Session expired — clear it and retry with a fresh one
        if status == reqwest::StatusCode::NOT_FOUND {
            let _ = resp.bytes().await;
            tracing::warn!(
                target: "playwright_mcp",
                session_id = %session_id,
                "MCP session not found (404), resetting and retrying"
            );
            *self.session_id.lock().await = None;
            let new_session_id = self.ensure_session(client).await?;
            if tool_name != "browser_navigate" {
                self.restore_after_session_reset(client, &new_session_id).await?;
            }
            let retry_id = self.req_id.fetch_add(1, Ordering::Relaxed) + 1;
            let retry_body = json!({
                "jsonrpc": "2.0",
                "id": retry_id,
                "method": "tools/call",
                "params": { "name": tool_name, "arguments": arguments },
            });
            let retry_resp = self.build_request(client, Some(&new_session_id), &retry_body)
                .send()
                .await
                .with_context(|| format!("playwright-mcp: tools/call retry failed for '{tool_name}'"))?;
            let retry_status = retry_resp.status();
            let retry_text = retry_resp.text().await.context("playwright-mcp: failed to read retry response")?;
            if !retry_status.is_success() {
                anyhow::bail!("playwright-mcp: sidecar returned HTTP {retry_status} for tool '{tool_name}' (after session reset)");
            }
            let json_resp = parse_sse_json(&retry_text)?;
            if let Some(err) = json_resp.get("error") {
                anyhow::bail!("playwright-mcp: tool '{}' error (after session reset): {}", tool_name, err);
            }
            let result = json_resp.get("result").cloned().unwrap_or(Value::Null);
            let current_url = requested_url.or_else(|| extract_page_url_from_result(&result));
            self.record_successful_tool_call(tool_name, &arguments, current_url.clone()).await;
            if current_url.is_some() {
                self.capture_cookies_json(client, &new_session_id).await;
            }
            return Ok(result);
        }

        let body_text = resp.text().await.context("playwright-mcp: failed to read tools/call response")?;

        if !status.is_success() {
            anyhow::bail!("playwright-mcp: sidecar returned HTTP {status} for tool '{tool_name}'");
        }

        let json_resp = parse_sse_json(&body_text)?;

        if let Some(err) = json_resp.get("error") {
            anyhow::bail!("playwright-mcp: tool '{}' error: {}", tool_name, err);
        }

        let result = json_resp.get("result").cloned().unwrap_or(Value::Null);
        let current_url = requested_url.or_else(|| extract_page_url_from_result(&result));
        self.record_successful_tool_call(tool_name, &arguments, current_url.clone()).await;
        if current_url.is_some() {
            self.capture_cookies_json(client, &session_id).await;
        }

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
                    tokio::time::sleep(Duration::from_millis(*delay_ms)).await;
                }
            }
        }
        // Final attempt (no sleep after)
        self.call_tool(tool_name, arguments).await.map_err(|e| {
            anyhow::anyhow!("playwright-mcp: all 3 attempts failed. Last error: {e}")
        })
    }

    async fn restore_after_session_reset(
        &self,
        client: &reqwest::Client,
        session_id: &str,
    ) -> anyhow::Result<()> {
        if let Some(cookies_json) = self.cookies_json.lock().await.clone() {
            let cookies_literal = js_string(&cookies_json)?;
            self.send_tool_call(
                client,
                session_id,
                "browser_run_code",
                json!({
                    "code": format!(
                        "async (page) => {{ await page.context().addCookies(JSON.parse({cookies})); return true; }}",
                        cookies = cookies_literal,
                    ),
                }),
            )
            .await
            .context("playwright-mcp: failed to restore cookies after session reset")?;
        }

        if let Some(last_url) = self.last_url.lock().await.clone() {
            self.send_tool_call(client, session_id, "browser_navigate", json!({ "url": last_url }))
                .await
                .context("playwright-mcp: failed to restore browser page after session reset")?;
        }

        let replay_actions = self.replayable_actions.lock().await.clone();
        for (tool_name, arguments) in replay_actions {
            self.send_tool_call(client, session_id, &tool_name, arguments)
                .await
                .with_context(|| format!("playwright-mcp: failed to replay '{tool_name}' after session reset"))?;
        }

        Ok(())
    }

    async fn send_tool_call(
        &self,
        client: &reqwest::Client,
        session_id: &str,
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

        let resp = self.build_request(client, Some(session_id), &body)
            .send()
            .await
            .with_context(|| format!("playwright-mcp: tools/call request failed for '{tool_name}'"))?;
        let status = resp.status();
        let body_text = resp.text().await.context("playwright-mcp: failed to read tools/call response")?;
        if !status.is_success() {
            anyhow::bail!("playwright-mcp: sidecar returned HTTP {status} for tool '{tool_name}'");
        }

        let json_resp = parse_sse_json(&body_text)?;
        if let Some(err) = json_resp.get("error") {
            anyhow::bail!("playwright-mcp: tool '{}' error: {}", tool_name, err);
        }

        Ok(json_resp.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn record_successful_tool_call(
        &self,
        tool_name: &str,
        arguments: &Value,
        current_url: Option<String>,
    ) {
        if tool_name == "browser_close" {
            *self.last_url.lock().await = None;
            *self.cookies_json.lock().await = None;
            self.replayable_actions.lock().await.clear();
            return;
        }

        if let Some(url) = current_url {
            *self.last_url.lock().await = Some(url);
            if tool_name == "browser_navigate" {
                self.replayable_actions.lock().await.clear();
                return;
            }
        }

        if is_replayable_tool_call(tool_name, arguments) {
            self.replayable_actions
                .lock()
                .await
                .push((tool_name.to_string(), arguments.clone()));
        }
    }

    async fn capture_cookies_json(
        &self,
        client: &reqwest::Client,
        session_id: &str,
    ) {
        let result = match self.send_tool_call(
            client,
            session_id,
            "browser_run_code",
            json!({
                "code": "async (page) => JSON.stringify(await page.context().cookies())",
            }),
        ).await {
            Ok(result) => result,
            Err(error) => {
                tracing::debug!(
                    target: "playwright_mcp",
                    error = %error,
                    "playwright-mcp: failed to capture cookies"
                );
                return;
            }
        };

        if let Some(cookies_json) = extract_string_result(&result) {
            *self.cookies_json.lock().await = Some(cookies_json);
        }
    }
}

fn is_replayable_tool_call(tool_name: &str, arguments: &Value) -> bool {
    match tool_name {
        "browser_fill_form" | "browser_type" | "browser_select_option" => true,
        "browser_evaluate" => arguments
            .get("function")
            .and_then(Value::as_str)
            .map(|function| {
                function.contains("dispatchEvent(new Event('input'")
                    || function.contains("dispatchEvent(new InputEvent('input'")
                    || function.contains("field.value =")
                    || function.contains("textContent =")
            })
            .unwrap_or(false),
        "browser_run_code" => arguments
            .get("code")
            .and_then(Value::as_str)
            .map(|code| {
                code.contains(".fill(")
                    || code.contains(".type(")
                    || code.contains(".selectOption(")
            })
            .unwrap_or(false),
        _ => false,
    }
}

fn extract_page_url_from_result(result: &Value) -> Option<String> {
    result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .find_map(|text| {
            let page_url = text.lines()
                .find_map(|line| line.trim().strip_prefix("- Page URL: ").map(str::trim))
                .filter(|url| !url.is_empty())
                .map(str::to_string);
            if page_url.is_some() {
                return page_url;
            }

            let mut saw_result_header = false;
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed == "### Result" {
                    saw_result_header = true;
                    continue;
                }
                if !saw_result_header {
                    continue;
                }
                if trimmed.starts_with("### ") {
                    break;
                }
                let candidate = trimmed.trim_matches('"');
                if candidate.starts_with("http://") || candidate.starts_with("https://") {
                    return Some(candidate.to_string());
                }
            }
            None
        })
}

fn extract_string_result(result: &Value) -> Option<String> {
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .next()?;

    let mut saw_result_header = false;
    let mut result_lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed == "### Result" {
            saw_result_header = true;
            continue;
        }
        if !saw_result_header {
            continue;
        }
        if trimmed.starts_with("### ") {
            break;
        }
        if trimmed.is_empty() {
            continue;
        }
        result_lines.push(trimmed);
    }
    let payload = result_lines.join("\n");
    serde_json::from_str::<String>(&payload).ok()
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
        let (tool, args) = action_to_mcp_tool(BrowserAction::Click { selector: "@e12".into() }).unwrap();
        assert_eq!(tool, "browser_click");
        assert_eq!(args["ref"], "e12");
    }

    #[test]
    fn fill_maps_to_browser_fill_form_with_value() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Fill { selector: "@e30".into(), value: "hello".into() }).unwrap();
        assert_eq!(tool, "browser_evaluate");
        assert_eq!(args["ref"], "e30");
        assert!(args["function"].as_str().unwrap().contains("hello"));
    }

    #[test]
    fn type_maps_to_browser_type() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Type { selector: "@e34".into(), text: "world".into() }).unwrap();
        assert_eq!(tool, "browser_type");
        assert_eq!(args["ref"], "e34");
        assert_eq!(args["text"], "world");
    }

    #[test]
    fn press_maps_to_browser_press_key() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Press { key: "Enter".into() }).unwrap();
        assert_eq!(tool, "browser_press_key");
        assert_eq!(args["key"], "Enter");
    }

    #[test]
    fn screenshot_maps_to_browser_take_screenshot() {
        let (tool, _) = action_to_mcp_tool(BrowserAction::Screenshot { path: None, full_page: false }).unwrap();
        assert_eq!(tool, "browser_take_screenshot");
    }

    #[test]
    fn get_title_maps_to_browser_evaluate_with_title_function() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::GetTitle).unwrap();
        assert_eq!(tool, "browser_evaluate");
        assert!(args["function"].as_str().unwrap().contains("title"));
    }

    #[test]
    fn get_url_maps_to_browser_evaluate_with_href_function() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::GetUrl).unwrap();
        assert_eq!(tool, "browser_evaluate");
        assert!(args["function"].as_str().unwrap().contains("href") || args["function"].as_str().unwrap().contains("location"));
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
        let (tool, args) = action_to_mcp_tool(BrowserAction::Hover { selector: "@e44".into() }).unwrap();
        assert_eq!(tool, "browser_hover");
        assert_eq!(args["ref"], "e44");
    }

    #[test]
    fn scroll_maps_to_browser_scroll() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Scroll { direction: "down".into(), pixels: Some(300) }).unwrap();
        assert_eq!(tool, "browser_evaluate");
        assert!(args["function"].as_str().unwrap().contains("scrollBy"));
    }

    #[test]
    fn wait_with_selector_maps_to_browser_wait_for() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Wait { selector: Some("@e52".into()), ms: None, text: None }).unwrap();
        assert_eq!(tool, "browser_evaluate");
        assert_eq!(args["ref"], "e52");
    }

    #[test]
    fn bare_ref_maps_to_playwright_ref_tools() {
        let (click_tool, click_args) = action_to_mcp_tool(BrowserAction::Click { selector: "e12".into() }).unwrap();
        assert_eq!(click_tool, "browser_click");
        assert_eq!(click_args["ref"], "e12");

        let (fill_tool, fill_args) = action_to_mcp_tool(BrowserAction::Fill { selector: "e30".into(), value: "hello".into() }).unwrap();
        assert_eq!(fill_tool, "browser_evaluate");
        assert_eq!(fill_args["ref"], "e30");
    }

    #[test]
    fn fill_with_text_selector_maps_to_browser_evaluate() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Fill {
            selector: "text=Username".into(),
            value: "admin".into(),
        }).unwrap();

        assert_eq!(tool, "browser_evaluate");
        let function = args["function"].as_str().unwrap();
        assert!(function.contains("Username"));
        assert!(function.contains("input, textarea, select"));
    }

    #[test]
    fn css_fill_maps_to_browser_run_code() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Fill {
            selector: "input#username".into(),
            value: "admin".into(),
        }).unwrap();

        assert_eq!(tool, "browser_run_code");
        let code = args["code"].as_str().unwrap();
        assert!(code.contains("locator"));
        assert!(code.contains("fill"));
        assert!(code.contains("input#username"));
    }

    #[test]
    fn find_fill_by_label_uses_text_locator() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Find {
            by: "label".into(),
            value: "Username".into(),
            action: "fill".into(),
            fill_value: Some("admin".into()),
        }).unwrap();

        assert_eq!(tool, "browser_evaluate");
        let function = args["function"].as_str().unwrap();
        assert!(function.contains("Username"));
        assert!(function.contains("admin"));
    }

    #[test]
    fn find_click_by_role_maps_to_browser_click() {
        let (tool, args) = action_to_mcp_tool(BrowserAction::Find {
            by: "role".into(),
            value: "button".into(),
            action: "click".into(),
            fill_value: None,
        }).unwrap();
        assert_eq!(tool, "browser_run_code");
        assert!(args["code"].as_str().unwrap().contains("locator"));
    }

    // ── PlaywrightMcpClient HTTP tests ─────────────────────────────────────
    //
    // All tests against a mock MCP server must simulate the full StreamableHTTP
    // handshake: initialize (returns mcp-session-id) → notifications/initialized → tools/call.
    // The client sends three POST /mcp requests per fresh session.

    fn make_session_id() -> Arc<tokio::sync::Mutex<Option<String>>> {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    fn make_last_url() -> Arc<tokio::sync::Mutex<Option<String>>> {
        Arc::new(tokio::sync::Mutex::new(None))
    }

    fn make_replayable_actions() -> Arc<tokio::sync::Mutex<Vec<(String, Value)>>> {
        Arc::new(tokio::sync::Mutex::new(Vec::new()))
    }

    fn sse_body(json: &str) -> String {
        format!("event: message\ndata: {json}\n\n")
    }

    fn init_response() -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(200)
            .append_header("mcp-session-id", "test-session")
            .append_header("Content-Type", "text/event-stream")
            .set_body_string(sse_body(r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{},"protocolVersion":"2024-11-05","serverInfo":{"name":"test","version":"1"}}}"#))
    }

    fn notif_response() -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(202)
    }

    fn tool_ok_response(id: u64) -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(200)
            .append_header("Content-Type", "text/event-stream")
            .set_body_string(sse_body(&format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"done"}}]}}}}"#
            )))
    }

    #[tokio::test]
    async fn client_calls_tool_via_mcp_endpoint() {
        use wiremock::{MockServer, Mock, matchers::{method, path}};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let server = MockServer::start().await;
        let req_count = Arc::new(AtomicUsize::new(0));
        let rc = req_count.clone();
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(move |_req: &wiremock::Request| {
                match rc.fetch_add(1, Ordering::SeqCst) {
                    0 => init_response(),                        // initialize
                    1 => notif_response(),                       // notifications/initialized
                    n => tool_ok_response(n as u64),             // tools/call
                }
            })
            .mount(&server)
            .await;
        let client = PlaywrightMcpClient::new(server.uri(), None, 30_000, make_session_id(), make_last_url(), make_replayable_actions());
        let result = client.call_tool("browser_navigate", serde_json::json!({ "url": "https://x.com" })).await;
        assert!(result.is_ok(), "expected ok, got: {result:?}");
    }

    #[tokio::test]
    async fn client_sends_bearer_token_on_every_request() {
        use wiremock::{MockServer, Mock, matchers::{method, path, header}};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let server = MockServer::start().await;
        let req_count = Arc::new(AtomicUsize::new(0));
        let rc = req_count.clone();
        // All requests must include the Authorization header
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(header("authorization", "Bearer tok123"))
            .respond_with(move |_req: &wiremock::Request| {
                match rc.fetch_add(1, Ordering::SeqCst) {
                    0 => init_response(),
                    1 => notif_response(),
                    n => tool_ok_response(n as u64),
                }
            })
            .mount(&server)
            .await;
        let client = PlaywrightMcpClient::new(server.uri(), Some("tok123".into()), 30_000, make_session_id(), make_last_url(), make_replayable_actions());
        let result = client.call_tool("browser_navigate", serde_json::json!({})).await;
        assert!(result.is_ok(), "expected ok, got: {result:?}");
    }

    #[tokio::test]
    async fn client_resets_session_on_404_and_retries() {
        use wiremock::{MockServer, Mock, ResponseTemplate, matchers::{method, path}};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let server = MockServer::start().await;
        let req_count = Arc::new(AtomicUsize::new(0));
        let rc = req_count.clone();
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(move |_req: &wiremock::Request| {
                match rc.fetch_add(1, Ordering::SeqCst) {
                    0 => init_response(),          // 1st initialize
                    1 => notif_response(),          // 1st notifications/initialized
                    2 => ResponseTemplate::new(404), // tools/call → 404: session expired
                    3 => init_response(),           // 2nd initialize (retry)
                    4 => notif_response(),           // 2nd notifications/initialized (retry)
                    n => tool_ok_response(n as u64), // tools/call retry → success
                }
            })
            .mount(&server)
            .await;
        let client = PlaywrightMcpClient::new(server.uri(), None, 30_000, make_session_id(), make_last_url(), make_replayable_actions());
        let result = client.call_tool("browser_navigate", serde_json::json!({ "url": "https://x.com" })).await;
        assert!(result.is_ok(), "expected successful retry after 404, got: {result:?}");
    }

    #[tokio::test]
    async fn client_restores_last_url_before_retrying_snapshot_after_404() {
        use serde_json::Value;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::{method, path}};

        let server = MockServer::start().await;
        let req_count = Arc::new(AtomicUsize::new(0));
        let rc = req_count.clone();
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(move |req: &wiremock::Request| {
                let index = rc.fetch_add(1, Ordering::SeqCst);
                let body: Value = serde_json::from_slice(&req.body).expect("request body should be valid JSON");
                match index {
                    0 => init_response(),
                    1 => notif_response(),
                    2 => {
                        assert_eq!(body["params"]["name"], "browser_navigate");
                        ResponseTemplate::new(200)
                            .append_header("Content-Type", "text/event-stream")
                            .set_body_string(sse_body(r#"{"jsonrpc":"2.0","id":2,"result":{"content":[{"type":"text","text":"navigated"}]}}"#))
                    }
                    3 => {
                        assert_eq!(body["params"]["name"], "browser_snapshot");
                        ResponseTemplate::new(404)
                    }
                    4 => init_response(),
                    5 => notif_response(),
                    6 => {
                        assert_eq!(body["params"]["name"], "browser_navigate");
                        assert_eq!(body["params"]["arguments"]["url"], "https://example.com/login");
                        tool_ok_response(6)
                    }
                    7 => {
                        assert_eq!(body["params"]["name"], "browser_snapshot");
                        tool_ok_response(7)
                    }
                    other => panic!("unexpected request index {other}"),
                }
            })
            .mount(&server)
            .await;

        let client = PlaywrightMcpClient::new(server.uri(), None, 30_000, make_session_id(), make_last_url(), make_replayable_actions());
        client
            .call_tool("browser_navigate", serde_json::json!({ "url": "https://example.com/login" }))
            .await
            .expect("navigate should succeed");
        let result = client.call_tool("browser_snapshot", serde_json::json!({})).await;
        assert!(result.is_ok(), "expected snapshot retry to succeed after restoring last URL, got: {result:?}");
    }

    #[tokio::test]
    async fn client_replays_form_state_before_retrying_click_after_404() {
        use serde_json::Value;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::{Mock, MockServer, ResponseTemplate, matchers::{method, path}};

        let server = MockServer::start().await;
        let req_count = Arc::new(AtomicUsize::new(0));
        let rc = req_count.clone();
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(move |req: &wiremock::Request| {
                let index = rc.fetch_add(1, Ordering::SeqCst);
                let body: Value = serde_json::from_slice(&req.body).expect("request body should be valid JSON");
                match index {
                    0 => init_response(),
                    1 => notif_response(),
                    2 => {
                        assert_eq!(body["params"]["name"], "browser_navigate");
                        tool_ok_response(2)
                    }
                    3 => {
                        assert_eq!(body["params"]["name"], "browser_fill_form");
                        assert_eq!(body["params"]["arguments"]["fields"][0]["ref"], "e30");
                        tool_ok_response(3)
                    }
                    4 => {
                        assert_eq!(body["params"]["name"], "browser_click");
                        ResponseTemplate::new(404)
                    }
                    5 => init_response(),
                    6 => notif_response(),
                    7 => {
                        assert_eq!(body["params"]["name"], "browser_navigate");
                        assert_eq!(body["params"]["arguments"]["url"], "https://example.com/login");
                        tool_ok_response(7)
                    }
                    8 => {
                        assert_eq!(body["params"]["name"], "browser_fill_form");
                        assert_eq!(body["params"]["arguments"]["fields"][0]["ref"], "e30");
                        tool_ok_response(8)
                    }
                    9 => {
                        assert_eq!(body["params"]["name"], "browser_click");
                        tool_ok_response(9)
                    }
                    other => panic!("unexpected request index {other}"),
                }
            })
            .mount(&server)
            .await;

        let client = PlaywrightMcpClient::new(server.uri(), None, 30_000, make_session_id(), make_last_url(), make_replayable_actions());
        client
            .call_tool("browser_navigate", serde_json::json!({ "url": "https://example.com/login" }))
            .await
            .expect("navigate should succeed");
        client
            .call_tool(
                "browser_fill_form",
                serde_json::json!({
                    "fields": [{
                        "name": "Username",
                        "type": "textbox",
                        "ref": "e30",
                        "value": "admin"
                    }]
                }),
            )
            .await
            .expect("fill should succeed");
        let result = client.call_tool("browser_click", serde_json::json!({ "ref": "e39" })).await;
        assert!(result.is_ok(), "expected click retry to succeed after replaying form state, got: {result:?}");
    }

    #[tokio::test]
    async fn client_fails_after_all_retries_exhausted() {
        use wiremock::{MockServer, Mock, ResponseTemplate, matchers::{method, path}};
        let server = MockServer::start().await;
        // All initialize requests fail with 500
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = PlaywrightMcpClient::new(server.uri(), None, 30_000, make_session_id(), make_last_url(), make_replayable_actions());
        let result = client.call_tool_with_retry("browser_navigate", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[test]
    fn detects_replayable_input_mutations() {
        assert!(is_replayable_tool_call(
            "browser_fill_form",
            &json!({ "fields": [{ "ref": "e30", "value": "admin" }] }),
        ));
        assert!(is_replayable_tool_call(
            "browser_evaluate",
            &json!({ "function": "(element) => { field.value = 'x'; field.dispatchEvent(new Event('input', { bubbles: true })); }" }),
        ));
        assert!(is_replayable_tool_call(
            "browser_run_code",
            &json!({ "code": "async (page) => { await page.locator('input').fill('x'); return true; }" }),
        ));
        assert!(!is_replayable_tool_call(
            "browser_evaluate",
            &json!({ "function": "() => window.location.href" }),
        ));
    }

    #[test]
    fn extracts_page_url_from_tool_result() {
        let result = json!({
            "content": [{
                "type": "text",
                "text": "### Page\n- Page URL: https://example.com/dashboard\n- Page Title: Example"
            }]
        });
        assert_eq!(
            extract_page_url_from_result(&result).as_deref(),
            Some("https://example.com/dashboard")
        );

        let run_code_result = json!({
            "content": [{
                "type": "text",
                "text": "### Result\n\"https://example.com/secure\"\n### Ran Playwright code"
            }]
        });
        assert_eq!(
            extract_page_url_from_result(&run_code_result).as_deref(),
            Some("https://example.com/secure")
        );
    }
}
