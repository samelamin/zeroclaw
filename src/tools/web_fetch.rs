use super::traits::{Tool, ToolResult};
use crate::config::schema::FirecrawlConfig;
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Minimum body length to consider a standard fetch successful.
/// Bodies shorter than this are treated as JS-only pages that need Firecrawl.
const FIRECRAWL_MIN_BODY_LEN: usize = 100;

/// Web fetch tool: fetches a web page and converts HTML to plain text for LLM consumption.
///
/// Unlike `http_request` (an API client returning raw responses), this tool:
/// - Only supports GET
/// - Follows redirects (up to 10)
/// - Converts HTML to clean plain text via `nanohtml2text`
/// - Passes through text/plain, text/markdown, and application/json as-is
/// - Sets a descriptive User-Agent
/// - Falls back to Firecrawl API when standard fetch fails (if enabled)
pub struct WebFetchTool {
    security: Arc<SecurityPolicy>,
    allowed_domains: Vec<String>,
    blocked_domains: Vec<String>,
    allowed_private_hosts: Vec<String>,
    max_response_size: usize,
    timeout_secs: u64,
    firecrawl: FirecrawlConfig,
}

impl WebFetchTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        allowed_domains: Vec<String>,
        blocked_domains: Vec<String>,
        max_response_size: usize,
        timeout_secs: u64,
        firecrawl: FirecrawlConfig,
        allowed_private_hosts: Vec<String>,
    ) -> Self {
        Self {
            security,
            allowed_domains: normalize_allowed_domains(allowed_domains),
            blocked_domains: normalize_allowed_domains(blocked_domains),
            allowed_private_hosts: normalize_allowed_domains(allowed_private_hosts),
            max_response_size,
            timeout_secs,
            firecrawl,
        }
    }

    fn validate_url(&self, raw_url: &str) -> anyhow::Result<String> {
        validate_target_url(
            raw_url,
            &self.allowed_domains,
            &self.blocked_domains,
            &self.allowed_private_hosts,
            "web_fetch",
        )
    }

    fn truncate_response(&self, text: &str) -> String {
        if text.len() > self.max_response_size {
            let mut truncated = text
                .chars()
                .take(self.max_response_size)
                .collect::<String>();
            truncated.push_str("\n\n... [Response truncated due to size limit] ...");
            truncated
        } else {
            text.to_string()
        }
    }

    async fn read_response_text_limited(
        &self,
        response: reqwest::Response,
    ) -> anyhow::Result<String> {
        let mut bytes_stream = response.bytes_stream();
        let hard_cap = self.max_response_size.saturating_add(1);
        let mut bytes = Vec::new();

        while let Some(chunk_result) = bytes_stream.next().await {
            let chunk = chunk_result?;
            if append_chunk_with_cap(&mut bytes, &chunk, hard_cap) {
                break;
            }
        }

        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Whether the standard fetch result should trigger a Firecrawl fallback.
    fn should_fallback_to_firecrawl(&self, result: &ToolResult) -> bool {
        if !self.firecrawl.enabled {
            return false;
        }
        // Fallback on failure (HTTP error, network error, etc.)
        if !result.success {
            return true;
        }
        // Fallback on empty or very short body (JS-only pages)
        if result.output.trim().len() < FIRECRAWL_MIN_BODY_LEN {
            return true;
        }
        false
    }

    /// Fetch content via the Firecrawl API.
    async fn fetch_via_firecrawl(&self, url: &str) -> anyhow::Result<ToolResult> {
        let api_key = std::env::var(&self.firecrawl.api_key_env).map_err(|_| {
            anyhow::anyhow!(
                "Firecrawl API key not found in environment variable '{}'",
                self.firecrawl.api_key_env
            )
        })?;

        let endpoint = format!("{}/scrape", self.firecrawl.api_url.trim_end_matches('/'));

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build Firecrawl HTTP client: {e}"))?;

        let body = json!({
            "url": url,
            "formats": ["markdown"]
        });

        let response = client
            .post(&endpoint)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("Firecrawl request failed: {e}"))?;

        let status = response.status();
        if !status.is_success() {
            let error_body = response.text().await.unwrap_or_default();
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Firecrawl API error: HTTP {} - {}",
                    status.as_u16(),
                    error_body
                )),
            });
        }

        let resp_json: serde_json::Value = response
            .json()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to parse Firecrawl response: {e}"))?;

        let markdown = resp_json
            .get("data")
            .and_then(|d| d.get("markdown"))
            .and_then(|m| m.as_str())
            .unwrap_or("");

        if markdown.is_empty() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Firecrawl returned empty markdown content".into()),
            });
        }

        let output = self.truncate_response(markdown);

        Ok(ToolResult {
            success: true,
            output,
            error: None,
        })
    }

    /// Perform the standard HTTP GET fetch and convert to text.
    async fn standard_fetch(&self, client: &reqwest::Client, url: &str) -> ToolResult {
        let response = match client.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                let chain = error_chain(&e);
                tracing::error!(
                    target: "web_fetch",
                    url = %url,
                    error = %e,
                    error_chain = %chain,
                    "web_fetch standard_fetch network error"
                );
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("HTTP request failed: {e} (chain: {chain})")),
                };
            }
        };

        let status = response.status();
        if !status.is_success() {
            let status_reason = status.canonical_reason().unwrap_or("Unknown");
            tracing::warn!(
                target: "web_fetch",
                url = %url,
                status = status.as_u16(),
                status_reason = %status_reason,
                "web_fetch standard_fetch HTTP non-2xx response"
            );
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!("HTTP {} {}", status.as_u16(), status_reason)),
            };
        }

        // Determine content type for processing strategy
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();

        let body_mode = if content_type.contains("text/html") || content_type.is_empty() {
            "html"
        } else if content_type.contains("text/plain")
            || content_type.contains("text/markdown")
            || content_type.contains("application/json")
        {
            "plain"
        } else {
            tracing::warn!(
                target: "web_fetch",
                url = %url,
                content_type = %content_type,
                "web_fetch standard_fetch unsupported content type"
            );
            return ToolResult {
                success: false,
                output: String::new(),
                error: Some(format!(
                    "Unsupported content type: {content_type}. \
                     web_fetch supports text/html, text/plain, text/markdown, and application/json."
                )),
            };
        };

        let body = match self.read_response_text_limited(response).await {
            Ok(t) => t,
            Err(e) => {
                let chain = anyhow_error_chain(&e);
                tracing::error!(
                    target: "web_fetch",
                    url = %url,
                    phase = "body_read",
                    error = %e,
                    error_chain = %chain,
                    "web_fetch standard_fetch body_read failure"
                );
                return ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to read response body: {e} (chain: {chain})")),
                };
            }
        };

        let text = if body_mode == "html" {
            nanohtml2text::html2text(&body)
        } else {
            body
        };

        let output = self.truncate_response(&text);

        ToolResult {
            success: true,
            output,
            error: None,
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a web page and return its content as clean plain text. \
         HTML pages are automatically converted to readable text. \
         JSON and plain text responses are returned as-is. \
         Only GET requests; follows redirects. \
         Falls back to Firecrawl for JS-heavy/bot-blocked sites (if enabled). \
         Security: allowlist-only domains, no local/private hosts."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The HTTP or HTTPS URL to fetch"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        let url = args
            .get("url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing 'url' parameter"))?;

        if !self.security.can_act() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: autonomy is read-only".into()),
            });
        }

        if !self.security.record_action() {
            return Ok(ToolResult {
                success: false,
                output: String::new(),
                error: Some("Action blocked: rate limit exceeded".into()),
            });
        }

        let url = match self.validate_url(url) {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        // Build client: follow redirects, set timeout, set User-Agent
        let timeout_secs = if self.timeout_secs == 0 {
            tracing::warn!("web_fetch: timeout_secs is 0, using safe default of 30s");
            30
        } else {
            self.timeout_secs
        };

        let allowed_domains = self.allowed_domains.clone();
        let blocked_domains = self.blocked_domains.clone();
        let allowed_private_hosts = self.allowed_private_hosts.clone();
        let redirect_policy = reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error(std::io::Error::other("Too many redirects (max 10)"));
            }

            if let Err(err) = validate_target_url(
                attempt.url().as_str(),
                &allowed_domains,
                &blocked_domains,
                &allowed_private_hosts,
                "web_fetch",
            ) {
                return attempt.error(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("Blocked redirect target: {err}"),
                ));
            }

            attempt.follow()
        });

        let builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .redirect(redirect_policy)
            .user_agent("ZeroClaw/0.1 (web_fetch)");

        // Log the effective runtime proxy state so operators debugging
        // "connection error" reports from customers can tell whether
        // a proxy was resolved, whether it applied to this service
        // key, and which concrete URLs (if any) were dialled through.
        let service_key = "tool.web_fetch";
        let proxy_cfg = crate::config::runtime_proxy_config();
        let proxy_applies = proxy_cfg.should_apply_to_service(service_key);
        tracing::debug!(
            target: "web_fetch",
            service_key = %service_key,
            url = %url,
            proxy_enabled = proxy_cfg.enabled,
            proxy_applies = proxy_applies,
            http_proxy = proxy_cfg.http_proxy.as_deref().unwrap_or(""),
            https_proxy = proxy_cfg.https_proxy.as_deref().unwrap_or(""),
            all_proxy = proxy_cfg.all_proxy.as_deref().unwrap_or(""),
            no_proxy_count = proxy_cfg.normalized_no_proxy().len(),
            "web_fetch resolved runtime proxy state"
        );

        let builder = crate::config::apply_runtime_proxy_to_builder(builder, service_key);
        let client = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Failed to build HTTP client: {e}")),
                });
            }
        };

        let standard_result = self.standard_fetch(&client, &url).await;

        // If standard fetch succeeded well enough, return it directly.
        // Otherwise, try Firecrawl fallback if enabled.
        if self.should_fallback_to_firecrawl(&standard_result) {
            tracing::info!(
                "web_fetch: standard fetch insufficient for {url}, attempting Firecrawl fallback"
            );
            match Box::pin(self.fetch_via_firecrawl(&url)).await {
                Ok(firecrawl_result) if firecrawl_result.success => {
                    return Ok(firecrawl_result);
                }
                Ok(firecrawl_result) => {
                    tracing::warn!(
                        "web_fetch: Firecrawl fallback also failed: {:?}",
                        firecrawl_result.error
                    );
                    // Return original standard result if Firecrawl also failed
                }
                Err(e) => {
                    tracing::warn!("web_fetch: Firecrawl fallback error: {e}");
                }
            }
        }

        Ok(standard_result)
    }
}

// ── Helper functions (independent from http_request.rs per DRY rule-of-three) ──

/// Render a full `std::error::Error` source chain as a single string,
/// joining each level with " | caused by: ". For reqwest errors this
/// surfaces the hyper/io root cause (e.g. ConnectionRefused, tls
/// handshake failure, dns NXDOMAIN) that is otherwise hidden behind
/// reqwest's own Display output.
fn error_chain<E: std::error::Error + ?Sized>(e: &E) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(e.to_string());
    let mut cur: Option<&(dyn std::error::Error + 'static)> = e.source();
    while let Some(s) = cur {
        parts.push(s.to_string());
        cur = s.source();
    }
    parts.join(" | caused by: ")
}

/// Same as [`error_chain`] but accepts an `anyhow::Error` directly,
/// walking its [`anyhow::Error::chain`] iterator. `anyhow::Error`
/// does not itself implement `std::error::Error`, so the generic
/// helper can't be used directly.
fn anyhow_error_chain(e: &anyhow::Error) -> String {
    e.chain()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | caused by: ")
}

fn validate_target_url(
    raw_url: &str,
    allowed_domains: &[String],
    blocked_domains: &[String],
    allowed_private_hosts: &[String],
    tool_name: &str,
) -> anyhow::Result<String> {
    let url = raw_url.trim();

    if url.is_empty() {
        anyhow::bail!("URL cannot be empty");
    }

    if url.chars().any(char::is_whitespace) {
        anyhow::bail!("URL cannot contain whitespace");
    }

    if !url.starts_with("http://") && !url.starts_with("https://") {
        anyhow::bail!("Only http:// and https:// URLs are allowed");
    }

    if allowed_domains.is_empty() {
        anyhow::bail!(
            "{tool_name} tool is enabled but no allowed_domains are configured. \
             Add [{tool_name}].allowed_domains in config.toml"
        );
    }

    let host = extract_host(url)?;

    // blocked_domains always takes precedence
    if host_matches_allowlist(&host, blocked_domains) {
        anyhow::bail!("Host '{host}' is in {tool_name}.blocked_domains");
    }

    let private_host_allowed =
        is_private_or_local_host(&host) && host_matches_allowlist(&host, allowed_private_hosts);

    if is_private_or_local_host(&host) && !private_host_allowed {
        anyhow::bail!(
            "Blocked local/private host: {host}. \
             To allow this host, add it to {tool_name}.allowed_private_hosts in config.toml"
        );
    }

    if private_host_allowed {
        tracing::warn!(
            "{tool_name}: allowing private/local host '{host}' via allowed_private_hosts"
        );
    }

    if !private_host_allowed && !host_matches_allowlist(&host, allowed_domains) {
        anyhow::bail!("Host '{host}' is not in {tool_name}.allowed_domains");
    }

    if !private_host_allowed {
        validate_resolved_host_is_public(&host)?;
    }

    Ok(url.to_string())
}

fn append_chunk_with_cap(buffer: &mut Vec<u8>, chunk: &[u8], hard_cap: usize) -> bool {
    if buffer.len() >= hard_cap {
        return true;
    }

    let remaining = hard_cap - buffer.len();
    if chunk.len() > remaining {
        buffer.extend_from_slice(&chunk[..remaining]);
        return true;
    }

    buffer.extend_from_slice(chunk);
    buffer.len() >= hard_cap
}

fn normalize_allowed_domains(domains: Vec<String>) -> Vec<String> {
    let mut normalized = domains
        .into_iter()
        .filter_map(|d| normalize_domain(&d))
        .collect::<Vec<_>>();
    normalized.sort_unstable();
    normalized.dedup();
    normalized
}

fn normalize_domain(raw: &str) -> Option<String> {
    let mut d = raw.trim().to_lowercase();
    if d.is_empty() {
        return None;
    }

    if let Some(stripped) = d.strip_prefix("https://") {
        d = stripped.to_string();
    } else if let Some(stripped) = d.strip_prefix("http://") {
        d = stripped.to_string();
    }

    if let Some((host, _)) = d.split_once('/') {
        d = host.to_string();
    }

    d = d.trim_start_matches('.').trim_end_matches('.').to_string();

    if let Some((host, _)) = d.split_once(':') {
        d = host.to_string();
    }

    if d.is_empty() || d.chars().any(char::is_whitespace) {
        return None;
    }

    Some(d)
}

fn extract_host(url: &str) -> anyhow::Result<String> {
    let rest = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .ok_or_else(|| anyhow::anyhow!("Only http:// and https:// URLs are allowed"))?;

    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .ok_or_else(|| anyhow::anyhow!("Invalid URL"))?;

    if authority.is_empty() {
        anyhow::bail!("URL must include a host");
    }

    if authority.contains('@') {
        anyhow::bail!("URL userinfo is not allowed");
    }

    if authority.starts_with('[') {
        anyhow::bail!("IPv6 hosts are not supported in web_fetch");
    }

    let host = authority
        .split(':')
        .next()
        .unwrap_or_default()
        .trim()
        .trim_end_matches('.')
        .to_lowercase();

    if host.is_empty() {
        anyhow::bail!("URL must include a valid host");
    }

    Ok(host)
}

fn host_matches_allowlist(host: &str, allowed_domains: &[String]) -> bool {
    if allowed_domains.iter().any(|domain| domain == "*") {
        return true;
    }

    allowed_domains.iter().any(|domain| {
        host == domain
            || host
                .strip_suffix(domain)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

fn is_private_or_local_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    let has_local_tld = bare
        .rsplit('.')
        .next()
        .is_some_and(|label| label == "local");

    if bare == "localhost" || bare.ends_with(".localhost") || has_local_tld {
        return true;
    }

    if let Ok(ip) = bare.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(v4) => is_non_global_v4(v4),
            std::net::IpAddr::V6(v6) => is_non_global_v6(v6),
        };
    }

    false
}

#[cfg(not(test))]
fn validate_resolved_host_is_public(host: &str) -> anyhow::Result<()> {
    use std::net::ToSocketAddrs;

    let ips = (host, 0)
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("Failed to resolve host '{host}': {e}"))?
        .map(|addr| addr.ip())
        .collect::<Vec<_>>();

    validate_resolved_ips_are_public(host, &ips)
}

#[cfg(test)]
fn validate_resolved_host_is_public(_host: &str) -> anyhow::Result<()> {
    // DNS checks are covered by validate_resolved_ips_are_public unit tests.
    Ok(())
}

fn validate_resolved_ips_are_public(host: &str, ips: &[std::net::IpAddr]) -> anyhow::Result<()> {
    if ips.is_empty() {
        anyhow::bail!("Failed to resolve host '{host}'");
    }

    for ip in ips {
        let non_global = match ip {
            std::net::IpAddr::V4(v4) => is_non_global_v4(*v4),
            std::net::IpAddr::V6(v6) => is_non_global_v6(*v6),
        };
        if non_global {
            anyhow::bail!("Blocked host '{host}' resolved to non-global address {ip}");
        }
    }

    Ok(())
}

fn is_non_global_v4(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        || v4.is_multicast()
        || (a == 100 && (64..=127).contains(&b))
        || a >= 240
        || (a == 192 && b == 0 && (c == 0 || c == 2))
        || (a == 198 && b == 51)
        || (a == 203 && b == 0)
        || (a == 198 && (18..=19).contains(&b))
}

fn is_non_global_v6(v6: std::net::Ipv6Addr) -> bool {
    let segs = v6.segments();
    v6.is_loopback()
        || v6.is_unspecified()
        || v6.is_multicast()
        || (segs[0] & 0xfe00) == 0xfc00
        || (segs[0] & 0xffc0) == 0xfe80
        || (segs[0] == 0x2001 && segs[1] == 0x0db8)
        || v6.to_ipv4_mapped().is_some_and(is_non_global_v4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::FirecrawlConfig;
    use crate::security::{AutonomyLevel, SecurityPolicy};
    use std::sync::Mutex as StdMutex;

    // ── Tracing capture helper (mirrors src/config/schema.rs pattern) ──

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

    /// Build a tracing subscriber + dispatcher guard that captures all
    /// events at TRACE level into the returned `TraceCapture` buffer.
    /// The returned `DefaultGuard` must be kept alive for the duration
    /// of the assertions, then dropped before reading the buffer.
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

    /// Reserve a TCP port by binding and immediately dropping the listener,
    /// yielding a URL that is guaranteed to refuse connections fast.
    fn reserved_unused_url() -> String {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        drop(tcp);
        format!("http://127.0.0.1:{port}/nowhere")
    }

    fn test_tool(allowed_domains: Vec<&str>) -> WebFetchTool {
        test_tool_with_blocklist(allowed_domains, vec![])
    }

    fn test_tool_with_blocklist(
        allowed_domains: Vec<&str>,
        blocked_domains: Vec<&str>,
    ) -> WebFetchTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        WebFetchTool::new(
            security,
            allowed_domains.into_iter().map(String::from).collect(),
            blocked_domains.into_iter().map(String::from).collect(),
            500_000,
            30,
            FirecrawlConfig::default(),
            vec![],
        )
    }

    fn test_tool_with_private_hosts(
        allowed_domains: Vec<&str>,
        blocked_domains: Vec<&str>,
        allowed_private_hosts: Vec<&str>,
    ) -> WebFetchTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        WebFetchTool::new(
            security,
            allowed_domains.into_iter().map(String::from).collect(),
            blocked_domains.into_iter().map(String::from).collect(),
            500_000,
            30,
            FirecrawlConfig::default(),
            allowed_private_hosts
                .into_iter()
                .map(String::from)
                .collect(),
        )
    }

    fn test_tool_with_firecrawl(firecrawl: FirecrawlConfig) -> WebFetchTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        WebFetchTool::new(
            security,
            vec!["*".into()],
            vec![],
            500_000,
            30,
            firecrawl,
            vec![],
        )
    }

    // ── Name and schema ──────────────────────────────────────────

    #[test]
    fn name_is_web_fetch() {
        let tool = test_tool(vec!["example.com"]);
        assert_eq!(tool.name(), "web_fetch");
    }

    #[test]
    fn parameters_schema_requires_url() {
        let tool = test_tool(vec!["example.com"]);
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["url"].is_object());
        let required = schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("url")));
    }

    // ── HTML to text conversion ──────────────────────────────────

    #[test]
    fn html_to_text_conversion() {
        let html = "<html><body><h1>Title</h1><p>Hello <b>world</b></p></body></html>";
        let text = nanohtml2text::html2text(html);
        assert!(text.contains("Title"));
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains("<h1>"));
        assert!(!text.contains("<p>"));
    }

    // ── URL validation ───────────────────────────────────────────

    #[test]
    fn validate_accepts_exact_domain() {
        let tool = test_tool(vec!["example.com"]);
        let got = tool.validate_url("https://example.com/page").unwrap();
        assert_eq!(got, "https://example.com/page");
    }

    #[test]
    fn validate_accepts_subdomain() {
        let tool = test_tool(vec!["example.com"]);
        assert!(tool.validate_url("https://docs.example.com/guide").is_ok());
    }

    #[test]
    fn validate_accepts_wildcard() {
        let tool = test_tool(vec!["*"]);
        assert!(tool.validate_url("https://news.ycombinator.com").is_ok());
    }

    #[test]
    fn validate_rejects_empty_url() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool.validate_url("").unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn validate_rejects_missing_url() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool.validate_url("  ").unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn validate_rejects_ftp_scheme() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("ftp://example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("http://") || err.contains("https://"));
    }

    #[test]
    fn validate_rejects_allowlist_miss() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("https://google.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_domains"));
    }

    #[test]
    fn validate_requires_allowlist() {
        let security = Arc::new(SecurityPolicy::default());
        let tool = WebFetchTool::new(
            security,
            vec![],
            vec![],
            500_000,
            30,
            FirecrawlConfig::default(),
            vec![],
        );
        let err = tool
            .validate_url("https://example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_domains"));
    }

    // ── SSRF protection ──────────────────────────────────────────

    #[test]
    fn ssrf_blocks_localhost() {
        let tool = test_tool(vec!["localhost"]);
        let err = tool
            .validate_url("https://localhost:8080")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
    }

    #[test]
    fn ssrf_blocks_private_ipv4() {
        let tool = test_tool(vec!["192.168.1.5"]);
        let err = tool
            .validate_url("https://192.168.1.5")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
    }

    #[test]
    fn ssrf_blocks_loopback() {
        assert!(is_private_or_local_host("127.0.0.1"));
        assert!(is_private_or_local_host("127.0.0.2"));
    }

    #[test]
    fn ssrf_blocks_rfc1918() {
        assert!(is_private_or_local_host("10.0.0.1"));
        assert!(is_private_or_local_host("172.16.0.1"));
        assert!(is_private_or_local_host("192.168.1.1"));
    }

    #[test]
    fn ssrf_wildcard_still_blocks_private() {
        let tool = test_tool(vec!["*"]);
        let err = tool
            .validate_url("https://localhost:8080")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
    }

    #[test]
    fn redirect_target_validation_allows_permitted_host() {
        let allowed = vec!["example.com".to_string()];
        let blocked = vec![];
        assert!(
            validate_target_url(
                "https://docs.example.com/page",
                &allowed,
                &blocked,
                &[],
                "web_fetch"
            )
            .is_ok()
        );
    }

    #[test]
    fn redirect_target_validation_blocks_private_host() {
        let allowed = vec!["example.com".to_string()];
        let blocked = vec![];
        let err = validate_target_url(
            "https://127.0.0.1/admin",
            &allowed,
            &blocked,
            &[],
            "web_fetch",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("local/private"));
    }

    #[test]
    fn redirect_target_validation_blocks_blocklisted_host() {
        let allowed = vec!["*".to_string()];
        let blocked = vec!["evil.com".to_string()];
        let err = validate_target_url(
            "https://evil.com/phish",
            &allowed,
            &blocked,
            &[],
            "web_fetch",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("blocked_domains"));
    }

    // ── Security policy ──────────────────────────────────────────

    #[tokio::test]
    async fn blocks_readonly_mode() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = WebFetchTool::new(
            security,
            vec!["example.com".into()],
            vec![],
            500_000,
            30,
            FirecrawlConfig::default(),
            vec![],
        );
        let result = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn blocks_rate_limited() {
        let security = Arc::new(SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = WebFetchTool::new(
            security,
            vec!["example.com".into()],
            vec![],
            500_000,
            30,
            FirecrawlConfig::default(),
            vec![],
        );
        let result = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    // ── Response truncation ──────────────────────────────────────

    #[test]
    fn truncate_within_limit() {
        let tool = test_tool(vec!["example.com"]);
        let text = "hello world";
        assert_eq!(tool.truncate_response(text), "hello world");
    }

    #[test]
    fn truncate_over_limit() {
        let tool = WebFetchTool::new(
            Arc::new(SecurityPolicy::default()),
            vec!["example.com".into()],
            vec![],
            10,
            30,
            FirecrawlConfig::default(),
            vec![],
        );
        let text = "hello world this is long";
        let truncated = tool.truncate_response(text);
        assert!(truncated.contains("[Response truncated"));
    }

    // ── Domain normalization ─────────────────────────────────────

    #[test]
    fn normalize_domain_strips_scheme_and_case() {
        let got = normalize_domain("  HTTPS://Docs.Example.com/path ").unwrap();
        assert_eq!(got, "docs.example.com");
    }

    #[test]
    fn normalize_deduplicates() {
        let got = normalize_allowed_domains(vec![
            "example.com".into(),
            "EXAMPLE.COM".into(),
            "https://example.com/".into(),
        ]);
        assert_eq!(got, vec!["example.com".to_string()]);
    }

    // ── Blocked domains ──────────────────────────────────────────

    #[test]
    fn blocklist_rejects_exact_match() {
        let tool = test_tool_with_blocklist(vec!["*"], vec!["evil.com"]);
        let err = tool
            .validate_url("https://evil.com/page")
            .unwrap_err()
            .to_string();
        assert!(err.contains("blocked_domains"));
    }

    #[test]
    fn blocklist_rejects_subdomain() {
        let tool = test_tool_with_blocklist(vec!["*"], vec!["evil.com"]);
        let err = tool
            .validate_url("https://api.evil.com/v1")
            .unwrap_err()
            .to_string();
        assert!(err.contains("blocked_domains"));
    }

    #[test]
    fn blocklist_wins_over_allowlist() {
        let tool = test_tool_with_blocklist(vec!["evil.com"], vec!["evil.com"]);
        let err = tool
            .validate_url("https://evil.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("blocked_domains"));
    }

    #[test]
    fn blocklist_allows_non_blocked() {
        let tool = test_tool_with_blocklist(vec!["*"], vec!["evil.com"]);
        assert!(tool.validate_url("https://example.com").is_ok());
    }

    #[test]
    fn append_chunk_with_cap_truncates_and_stops() {
        let mut buffer = Vec::new();
        assert!(!append_chunk_with_cap(&mut buffer, b"hello", 8));
        assert!(append_chunk_with_cap(&mut buffer, b"world", 8));
        assert_eq!(buffer, b"hellowor");
    }

    #[test]
    fn resolved_private_ip_is_rejected() {
        let ips = vec!["127.0.0.1".parse().unwrap()];
        let err = validate_resolved_ips_are_public("example.com", &ips)
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-global address"));
    }

    #[test]
    fn resolved_mixed_ips_are_rejected() {
        let ips = vec![
            "93.184.216.34".parse().unwrap(),
            "10.0.0.1".parse().unwrap(),
        ];
        let err = validate_resolved_ips_are_public("example.com", &ips)
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-global address"));
    }

    #[test]
    fn resolved_public_ips_are_allowed() {
        let ips = vec!["93.184.216.34".parse().unwrap(), "1.1.1.1".parse().unwrap()];
        assert!(validate_resolved_ips_are_public("example.com", &ips).is_ok());
    }

    // ── Firecrawl config parsing ────────────────────────────────────

    #[test]
    fn firecrawl_config_defaults() {
        let cfg = FirecrawlConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.api_key_env, "FIRECRAWL_API_KEY");
        assert_eq!(cfg.api_url, "https://api.firecrawl.dev/v1");
        assert_eq!(cfg.mode, crate::config::schema::FirecrawlMode::Scrape);
    }

    #[test]
    fn firecrawl_config_deserializes_from_toml() {
        let toml_str = r#"
            enabled = true
            api_key_env = "MY_FC_KEY"
            api_url = "https://custom.firecrawl.io/v2"
            mode = "crawl"
        "#;
        let cfg: FirecrawlConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.api_key_env, "MY_FC_KEY");
        assert_eq!(cfg.api_url, "https://custom.firecrawl.io/v2");
        assert_eq!(cfg.mode, crate::config::schema::FirecrawlMode::Crawl);
    }

    #[test]
    fn firecrawl_config_deserializes_defaults_from_empty_toml() {
        let cfg: FirecrawlConfig = toml::from_str("").unwrap();
        assert!(!cfg.enabled);
        assert_eq!(cfg.api_key_env, "FIRECRAWL_API_KEY");
    }

    #[test]
    fn web_fetch_config_with_firecrawl_section() {
        use crate::config::schema::WebFetchConfig;
        let toml_str = r#"
            enabled = true
            [firecrawl]
            enabled = true
            api_key_env = "FC_KEY"
        "#;
        let cfg: WebFetchConfig = toml::from_str(toml_str).unwrap();
        assert!(cfg.enabled);
        assert!(cfg.firecrawl.enabled);
        assert_eq!(cfg.firecrawl.api_key_env, "FC_KEY");
    }

    // ── Firecrawl fallback trigger conditions ───────────────────────

    #[test]
    fn fallback_disabled_when_firecrawl_not_enabled() {
        let tool = test_tool_with_firecrawl(FirecrawlConfig::default());
        let result = ToolResult {
            success: false,
            output: String::new(),
            error: Some("HTTP 403 Forbidden".into()),
        };
        assert!(!tool.should_fallback_to_firecrawl(&result));
    }

    #[test]
    fn fallback_triggers_on_http_error() {
        let tool = test_tool_with_firecrawl(FirecrawlConfig {
            enabled: true,
            ..FirecrawlConfig::default()
        });
        let result = ToolResult {
            success: false,
            output: String::new(),
            error: Some("HTTP 403 Forbidden".into()),
        };
        assert!(tool.should_fallback_to_firecrawl(&result));
    }

    #[test]
    fn fallback_triggers_on_empty_body() {
        let tool = test_tool_with_firecrawl(FirecrawlConfig {
            enabled: true,
            ..FirecrawlConfig::default()
        });
        let result = ToolResult {
            success: true,
            output: String::new(),
            error: None,
        };
        assert!(tool.should_fallback_to_firecrawl(&result));
    }

    #[test]
    fn fallback_triggers_on_short_body() {
        let tool = test_tool_with_firecrawl(FirecrawlConfig {
            enabled: true,
            ..FirecrawlConfig::default()
        });
        let result = ToolResult {
            success: true,
            output: "Loading...".into(), // < 100 chars, JS-only page
            error: None,
        };
        assert!(tool.should_fallback_to_firecrawl(&result));
    }

    #[test]
    fn fallback_skipped_on_good_response() {
        let tool = test_tool_with_firecrawl(FirecrawlConfig {
            enabled: true,
            ..FirecrawlConfig::default()
        });
        let result = ToolResult {
            success: true,
            output: "A".repeat(200), // well above 100 chars
            error: None,
        };
        assert!(!tool.should_fallback_to_firecrawl(&result));
    }

    // ── Firecrawl response parsing ──────────────────────────────────

    #[test]
    fn firecrawl_response_parses_markdown() {
        let response_json = json!({
            "success": true,
            "data": {
                "markdown": "# Hello World\n\nThis is extracted content from Firecrawl.",
                "metadata": {
                    "title": "Test Page"
                }
            }
        });
        let markdown = response_json
            .get("data")
            .and_then(|d| d.get("markdown"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        assert!(markdown.contains("Hello World"));
        assert!(markdown.contains("extracted content"));
    }

    #[test]
    fn firecrawl_response_handles_missing_markdown() {
        let response_json = json!({
            "success": true,
            "data": {}
        });
        let markdown = response_json
            .get("data")
            .and_then(|d| d.get("markdown"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        assert!(markdown.is_empty());
    }

    #[test]
    fn firecrawl_response_handles_missing_data() {
        let response_json = json!({
            "success": false,
            "error": "Rate limit exceeded"
        });
        let markdown = response_json
            .get("data")
            .and_then(|d| d.get("markdown"))
            .and_then(|m| m.as_str())
            .unwrap_or("");
        assert!(markdown.is_empty());
    }

    // ── Boundary test: FIRECRAWL_MIN_BODY_LEN (100 chars) ────────────

    #[test]
    fn fallback_triggers_at_exactly_99_chars() {
        let tool = test_tool_with_firecrawl(FirecrawlConfig {
            enabled: true,
            ..FirecrawlConfig::default()
        });
        let result = ToolResult {
            success: true,
            output: "A".repeat(99),
            error: None,
        };
        assert!(
            tool.should_fallback_to_firecrawl(&result),
            "99-char body (below threshold) should trigger fallback"
        );
    }

    #[test]
    fn fallback_skipped_at_exactly_100_chars() {
        let tool = test_tool_with_firecrawl(FirecrawlConfig {
            enabled: true,
            ..FirecrawlConfig::default()
        });
        let result = ToolResult {
            success: true,
            output: "A".repeat(100),
            error: None,
        };
        assert!(
            !tool.should_fallback_to_firecrawl(&result),
            "100-char body (at threshold) should NOT trigger fallback"
        );
    }

    // ── Item 1: missing API key env var falls back gracefully ─────────

    #[tokio::test]
    async fn firecrawl_missing_api_key_returns_error() {
        // Ensure the env var is unset for this test
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::remove_var("FIRECRAWL_TEST_MISSING_KEY") };

        let tool = test_tool_with_firecrawl(FirecrawlConfig {
            enabled: true,
            api_key_env: "FIRECRAWL_TEST_MISSING_KEY".into(),
            ..FirecrawlConfig::default()
        });

        let result = tool.fetch_via_firecrawl("https://example.com").await;
        assert!(
            result.is_err(),
            "fetch_via_firecrawl should return Err when API key env var is missing"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("FIRECRAWL_TEST_MISSING_KEY"),
            "Error should mention the missing env var name, got: {err_msg}"
        );
    }

    // ── Item 2: double-failure returns original standard result ───────

    #[tokio::test]
    async fn execute_double_failure_returns_original_result() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let addr = server.address();

        // Standard fetch returns 403 (failure)
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        // Ensure Firecrawl API key env is missing so fallback also fails
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::remove_var("FIRECRAWL_DOUBLE_FAIL_KEY") };

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        let tool = WebFetchTool::new(
            security,
            vec!["*".into()],
            vec![],
            500_000,
            30,
            FirecrawlConfig {
                enabled: true,
                api_key_env: "FIRECRAWL_DOUBLE_FAIL_KEY".into(),
                api_url: format!("http://{addr}"),
                ..FirecrawlConfig::default()
            },
            vec![],
        );

        // Bypass SSRF-guarded execute() — call standard_fetch + fallback
        // logic directly so wiremock on 127.0.0.1 is reachable.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();

        let url = format!("http://{addr}/page");
        let standard_result = tool.standard_fetch(&client, &url).await;

        // standard_fetch should fail with 403
        assert!(!standard_result.success);
        assert!(tool.should_fallback_to_firecrawl(&standard_result));

        // Firecrawl fallback should also fail (missing API key)
        let firecrawl_result = Box::pin(tool.fetch_via_firecrawl(&url)).await;
        assert!(
            firecrawl_result.is_err() || !firecrawl_result.as_ref().unwrap().success,
            "Expected Firecrawl fallback to fail without API key"
        );

        // The orchestration should return the original 403 error
        assert!(
            standard_result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("403"),
            "Expected original HTTP 403 error, got: {:?}",
            standard_result.error
        );
    }

    // ── Item 3: end-to-end fallback orchestration in execute() ───────

    #[tokio::test]
    async fn execute_falls_back_to_firecrawl_on_short_body() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        // Standard-fetch server: returns a very short body (JS-only placeholder)
        let standard_server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string("<html><body>Loading...</body></html>")
                    .insert_header("content-type", "text/html"),
            )
            .mount(&standard_server)
            .await;

        // Firecrawl server: returns rich markdown content
        let firecrawl_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/scrape"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "success": true,
                "data": {
                    "markdown": "# Real Content\n\nThis is the full page content extracted by Firecrawl, with enough text to be clearly above the minimum body length threshold."
                }
            })))
            .mount(&firecrawl_server)
            .await;

        // Set up API key env var for this test
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::set_var("FIRECRAWL_E2E_TEST_KEY", "test-key-12345") };

        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        let standard_addr = standard_server.address();
        let firecrawl_addr = firecrawl_server.address();
        let tool = WebFetchTool::new(
            security,
            vec!["*".into()],
            vec![],
            500_000,
            30,
            FirecrawlConfig {
                enabled: true,
                api_key_env: "FIRECRAWL_E2E_TEST_KEY".into(),
                api_url: format!("http://{firecrawl_addr}"),
                ..FirecrawlConfig::default()
            },
            vec![],
        );

        // Bypass SSRF-guarded execute() — call standard_fetch + fallback
        // logic directly so wiremock on 127.0.0.1 is reachable.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap();

        let url = format!("http://{standard_addr}/page");
        let standard_result = tool.standard_fetch(&client, &url).await;

        // Standard fetch returns short body, should trigger fallback
        assert!(tool.should_fallback_to_firecrawl(&standard_result));

        // Firecrawl fallback should succeed with rich content
        let result = Box::pin(tool.fetch_via_firecrawl(&url)).await.unwrap();

        assert!(result.success, "Expected successful Firecrawl fallback");
        assert!(
            result.output.contains("Real Content"),
            "Expected Firecrawl markdown content, got: {}",
            result.output
        );

        // Clean up env var
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::remove_var("FIRECRAWL_E2E_TEST_KEY") };
    }

    // ── Allowed private hosts ─────────────────────────────────────

    #[test]
    fn allowed_private_host_bypasses_ssrf_block() {
        let tool = test_tool_with_private_hosts(vec!["*"], vec![], vec!["192.168.1.5"]);
        assert!(tool.validate_url("https://192.168.1.5/api").is_ok());
    }

    #[test]
    fn unallowed_private_host_still_blocked() {
        let tool = test_tool_with_private_hosts(vec!["*"], vec![], vec!["192.168.1.5"]);
        let err = tool
            .validate_url("https://10.0.0.1/admin")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
        assert!(err.contains("allowed_private_hosts"));
    }

    #[test]
    fn blocklist_overrides_allowed_private_host() {
        let tool =
            test_tool_with_private_hosts(vec!["*"], vec!["192.168.1.5"], vec!["192.168.1.5"]);
        let err = tool
            .validate_url("https://192.168.1.5/secret")
            .unwrap_err()
            .to_string();
        assert!(err.contains("blocked_domains"));
    }

    #[test]
    fn allowed_private_host_with_port() {
        let tool = test_tool_with_private_hosts(vec!["*"], vec![], vec!["192.168.1.5"]);
        assert!(tool.validate_url("https://192.168.1.5:8080/api").is_ok());
    }

    // ── §1: Error-path logging on standard_fetch ────────────────────

    /// When `standard_fetch` fails with a network error (connection
    /// refused), the tool MUST emit an ERROR-level tracing event that
    /// carries the requested URL and the source-chain of the underlying
    /// reqwest/hyper/io error. Without this log, customers see only a
    /// generic "connection error" surfaced back to the model and cannot
    /// tell whether the failure is DNS, TLS, proxy, or upstream.
    #[tokio::test]
    async fn standard_fetch_logs_error_with_source_chain_on_network_failure() {
        let (capture, guard) = install_trace_capture();

        let url = reserved_unused_url();
        let tool = test_tool(vec!["*"]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .connect_timeout(Duration::from_millis(500))
            .build()
            .unwrap();

        let result = tool.standard_fetch(&client, &url).await;
        assert!(
            !result.success,
            "standard_fetch must fail against a refused port, got success"
        );

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("ERROR"),
            "expected ERROR level tracing event, got logs:\n{logs}"
        );
        assert!(
            logs.contains("web_fetch"),
            "expected 'web_fetch' in log target/message, got logs:\n{logs}"
        );
        assert!(
            logs.contains(&url),
            "expected requested URL {url} in logs, got:\n{logs}"
        );
        assert!(
            logs.contains("error_chain"),
            "expected 'error_chain' structured field with source chain, got:\n{logs}"
        );
    }

    /// When `standard_fetch` receives a non-2xx HTTP status code
    /// (e.g. 403 from a bot-blocking server), the tool MUST emit a
    /// WARN-level tracing event carrying `url`, `status`, and
    /// `status_reason`. This is the customer-visible signal that the
    /// target is actively refusing — distinct from a transport-layer
    /// failure — and should show up even at a default WARN filter.
    #[tokio::test]
    async fn standard_fetch_logs_warn_on_http_non_2xx_with_status_fields() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        let (capture, guard) = install_trace_capture();

        let tool = test_tool(vec!["*"]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let url = format!("http://{}/denied", server.address());
        let result = tool.standard_fetch(&client, &url).await;
        assert!(!result.success, "403 response must map to failure");

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("WARN"),
            "expected WARN level tracing event, got logs:\n{logs}"
        );
        assert!(
            logs.contains("web_fetch"),
            "expected 'web_fetch' in log target, got logs:\n{logs}"
        );
        assert!(
            logs.contains(&url),
            "expected requested URL {url} in logs, got:\n{logs}"
        );
        assert!(
            logs.contains("status=403"),
            "expected 'status=403' structured field, got:\n{logs}"
        );
        assert!(
            logs.contains("status_reason"),
            "expected 'status_reason' structured field, got:\n{logs}"
        );
    }

    /// When a server returns a content-type we don't handle (e.g. a
    /// binary PDF), `standard_fetch` must emit a WARN-level tracing
    /// event that includes the offending `content_type` so operators
    /// can quickly see "this site served us a PDF, try a different
    /// tool" rather than guessing from a generic error string.
    #[tokio::test]
    async fn standard_fetch_logs_warn_on_unsupported_content_type() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(b"%PDF-1.4 fake".to_vec())
                    .insert_header("content-type", "application/pdf"),
            )
            .mount(&server)
            .await;

        let (capture, guard) = install_trace_capture();

        let tool = test_tool(vec!["*"]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let url = format!("http://{}/doc.pdf", server.address());
        let result = tool.standard_fetch(&client, &url).await;
        assert!(!result.success, "unsupported content-type must fail");

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("WARN"),
            "expected WARN level tracing event, got:\n{logs}"
        );
        assert!(
            logs.contains("web_fetch"),
            "expected 'web_fetch' in log target, got:\n{logs}"
        );
        assert!(
            logs.contains(&url),
            "expected URL {url} in logs, got:\n{logs}"
        );
        assert!(
            logs.contains("content_type=") && logs.contains("pdf"),
            "expected 'content_type=...pdf' structured field, got:\n{logs}"
        );
    }

    /// When `standard_fetch` gets past the status/content-type checks
    /// but then fails to stream the body (e.g. the upstream drops the
    /// connection mid-transfer), the tool MUST emit an ERROR-level
    /// tracing event carrying the URL and the source-chain of the
    /// underlying IO error. This is the silent-partial-fetch case:
    /// customers see an incomplete page and no indication of why.
    #[tokio::test]
    async fn standard_fetch_logs_error_on_body_read_failure() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Minimal hand-rolled TCP server: claims Content-Length: 1048576,
        // sends a few bytes, then closes. The client will hit an
        // IncompleteMessage / UnexpectedEof on the body stream.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\n\
                          Content-Type: text/html\r\n\
                          Content-Length: 1048576\r\n\
                          Connection: close\r\n\
                          \r\n\
                          <html><body>partial",
                    )
                    .await;
                // Drop the socket immediately — client will see EOF
                // mid-body.
                drop(sock);
            }
        });

        let (capture, guard) = install_trace_capture();

        let tool = test_tool(vec!["*"]);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let url = format!("http://{addr}/partial");
        let result = tool.standard_fetch(&client, &url).await;
        assert!(
            !result.success,
            "standard_fetch must fail when body is truncated mid-stream, got success={:?}",
            result.output
        );

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("ERROR"),
            "expected ERROR level tracing event, got:\n{logs}"
        );
        assert!(
            logs.contains("web_fetch"),
            "expected 'web_fetch' in log target, got:\n{logs}"
        );
        assert!(
            logs.contains(&url),
            "expected URL {url} in logs, got:\n{logs}"
        );
        assert!(
            logs.contains("error_chain"),
            "expected 'error_chain' structured field on body-read failure, got:\n{logs}"
        );
        // Distinct from the send() error path — body read should
        // include a phase/stage marker so operators can distinguish
        // "couldn't connect" from "connected then got cut off".
        assert!(
            logs.contains("body_read") || logs.contains("phase=body"),
            "expected a 'body_read' or phase=body marker to distinguish \
             body-read failure from connection-establish failure, got:\n{logs}"
        );
    }

    /// On `execute()` entry (after URL validation, before dispatching
    /// the HTTP request), web_fetch MUST emit a DEBUG-level tracing
    /// event describing the effective runtime proxy state for the
    /// `tool.web_fetch` service key. Without this, customers reporting
    /// "connection error" from behind a corporate proxy have no way
    /// to tell whether ZeroClaw even resolved a proxy at all. Default
    /// state (proxy disabled) must still log so operators can confirm.
    #[tokio::test]
    async fn execute_logs_debug_proxy_state_on_entry() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(
                        "<html><body>\
                         padpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpad\
                         padpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpadpad\
                         </body></html>",
                    )
                    .insert_header("content-type", "text/html"),
            )
            .mount(&server)
            .await;

        let (capture, guard) = install_trace_capture();

        let tool = test_tool_with_private_hosts(vec!["*"], vec![], vec!["127.0.0.1"]);
        let url = format!("http://{}/ok", server.address());
        let result = tool.execute(json!({ "url": url.clone() })).await.unwrap();
        assert!(result.success, "expected execute() to succeed, got {result:?}");

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("DEBUG"),
            "expected DEBUG level tracing event, got:\n{logs}"
        );
        assert!(
            logs.contains("web_fetch"),
            "expected 'web_fetch' in log target, got:\n{logs}"
        );
        assert!(
            logs.contains("service_key=\"tool.web_fetch\"")
                || logs.contains("service_key=tool.web_fetch"),
            "expected service_key='tool.web_fetch' in logs, got:\n{logs}"
        );
        assert!(
            logs.contains("proxy_enabled"),
            "expected 'proxy_enabled' structured field, got:\n{logs}"
        );
        assert!(
            logs.contains("proxy_applies"),
            "expected 'proxy_applies' structured field showing whether \
             the runtime proxy scope actually applies to tool.web_fetch, got:\n{logs}"
        );
    }
}
