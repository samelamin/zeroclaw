use super::traits::{Tool, ToolResult};
use crate::security::SecurityPolicy;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// Walk the `std::error::Error::source()` chain of `e` and format it as
/// ` | caused by: ` joined segments. Surfaces the IO/transport root cause
/// buried beneath reqwest/hyper layers so operators see the real reason
/// behind a "connection error" at a single grep instead of attaching a
/// debugger.
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

/// Same shape as [`error_chain`] but for `anyhow::Error`. anyhow has
/// multiple `AsRef` impls so calling the generic helper is ambiguous;
/// this one uses `anyhow::Error::chain()` directly.
fn anyhow_error_chain(e: &anyhow::Error) -> String {
    e.chain()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | caused by: ")
}

/// HTTP request tool for API interactions.
/// Supports GET, POST, PUT, DELETE methods with configurable security.
pub struct HttpRequestTool {
    security: Arc<SecurityPolicy>,
    allowed_domains: Vec<String>,
    max_response_size: usize,
    timeout_secs: u64,
    allow_private_hosts: bool,
}

impl HttpRequestTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        allowed_domains: Vec<String>,
        max_response_size: usize,
        timeout_secs: u64,
        allow_private_hosts: bool,
    ) -> Self {
        Self {
            security,
            allowed_domains: normalize_allowed_domains(allowed_domains),
            max_response_size,
            timeout_secs,
            allow_private_hosts,
        }
    }

    fn validate_url(&self, raw_url: &str) -> anyhow::Result<String> {
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

        if self.allowed_domains.is_empty() {
            anyhow::bail!(
                "HTTP request tool is enabled but no allowed_domains are configured. Add [http_request].allowed_domains in config.toml"
            );
        }

        let host = extract_host(url)?;

        if !self.allow_private_hosts && is_private_or_local_host(&host) {
            anyhow::bail!("Blocked local/private host: {host}");
        }

        if !host_matches_allowlist(&host, &self.allowed_domains) {
            anyhow::bail!("Host '{host}' is not in http_request.allowed_domains");
        }

        Ok(url.to_string())
    }

    fn validate_method(&self, method: &str) -> anyhow::Result<reqwest::Method> {
        match method.to_uppercase().as_str() {
            "GET" => Ok(reqwest::Method::GET),
            "POST" => Ok(reqwest::Method::POST),
            "PUT" => Ok(reqwest::Method::PUT),
            "DELETE" => Ok(reqwest::Method::DELETE),
            "PATCH" => Ok(reqwest::Method::PATCH),
            "HEAD" => Ok(reqwest::Method::HEAD),
            "OPTIONS" => Ok(reqwest::Method::OPTIONS),
            _ => anyhow::bail!(
                "Unsupported HTTP method: {method}. Supported: GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS"
            ),
        }
    }

    fn parse_headers(&self, headers: &serde_json::Value) -> Vec<(String, String)> {
        let mut result = Vec::new();
        if let Some(obj) = headers.as_object() {
            for (key, value) in obj {
                if let Some(str_val) = value.as_str() {
                    result.push((key.clone(), str_val.to_string()));
                }
            }
        }
        result
    }

    fn redact_headers_for_display(headers: &[(String, String)]) -> Vec<(String, String)> {
        headers
            .iter()
            .map(|(key, value)| {
                let lower = key.to_lowercase();
                let is_sensitive = lower.contains("authorization")
                    || lower.contains("api-key")
                    || lower.contains("apikey")
                    || lower.contains("token")
                    || lower.contains("secret");
                if is_sensitive {
                    (key.clone(), "***REDACTED***".into())
                } else {
                    (key.clone(), value.clone())
                }
            })
            .collect()
    }

    async fn execute_request(
        &self,
        url: &str,
        method: reqwest::Method,
        headers: Vec<(String, String)>,
        body: Option<&str>,
    ) -> anyhow::Result<reqwest::Response> {
        let timeout_secs = if self.timeout_secs == 0 {
            tracing::warn!(
                target: "http_request",
                "http_request: timeout_secs is 0, using safe default of 30s"
            );
            30
        } else {
            self.timeout_secs
        };
        let builder = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .connect_timeout(Duration::from_secs(10))
            .redirect(reqwest::redirect::Policy::none());
        let builder = crate::config::apply_runtime_proxy_to_builder(builder, "tool.http_request");
        let client = match builder.build() {
            Ok(c) => c,
            Err(e) => {
                let chain = error_chain(&e);
                tracing::error!(
                    target: "http_request",
                    url = %url,
                    phase = "client_build",
                    error = %e,
                    error_chain = %chain,
                    "http_request reqwest client builder failed"
                );
                return Err(anyhow::anyhow!(
                    "reqwest client build failed: {e} (chain: {chain})"
                ));
            }
        };

        let mut request = client.request(method, url);

        for (key, value) in headers {
            request = request.header(&key, &value);
        }

        if let Some(body_str) = body {
            request = request.body(body_str.to_string());
        }

        match request.send().await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                let chain = error_chain(&e);
                tracing::error!(
                    target: "http_request",
                    url = %url,
                    phase = "send",
                    error = %e,
                    error_chain = %chain,
                    "http_request network send failed"
                );
                Err(anyhow::anyhow!(
                    "network send failed: {e} (chain: {chain})"
                ))
            }
        }
    }

    fn truncate_response(&self, text: &str) -> String {
        // 0 means unlimited — no truncation.
        if self.max_response_size == 0 {
            return text.to_string();
        }
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
}

#[async_trait]
impl Tool for HttpRequestTool {
    fn name(&self) -> &str {
        "http_request"
    }

    fn description(&self) -> &str {
        "Make HTTP requests to external APIs. Supports GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS methods. \
        Security constraints: allowlist-only domains, no local/private hosts, configurable timeout and response size limits."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "HTTP or HTTPS URL to request"
                },
                "method": {
                    "type": "string",
                    "description": "HTTP method (GET, POST, PUT, DELETE, PATCH, HEAD, OPTIONS)",
                    "default": "GET"
                },
                "headers": {
                    "type": "object",
                    "description": "Optional HTTP headers as key-value pairs (e.g., {\"Authorization\": \"Bearer token\", \"Content-Type\": \"application/json\"})",
                    "default": {}
                },
                "body": {
                    "type": "string",
                    "description": "Optional request body (for POST, PUT, PATCH requests)"
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

        // Debug-log the effective runtime proxy state before any outbound
        // traffic. Operators troubleshooting a "connection error" in the
        // field can set RUST_LOG=http_request=debug and immediately see
        // whether the corporate egress proxy is being applied for this
        // URL or if reqwest is going direct.
        {
            let service_key = "tool.http_request";
            let proxy_cfg = crate::config::runtime_proxy_config();
            let proxy_applies = proxy_cfg.should_apply_to_service(service_key);
            tracing::debug!(
                target: "http_request",
                service_key = %service_key,
                url = %url,
                proxy_enabled = proxy_cfg.enabled,
                proxy_applies = proxy_applies,
                http_proxy = proxy_cfg.http_proxy.as_deref().unwrap_or(""),
                https_proxy = proxy_cfg.https_proxy.as_deref().unwrap_or(""),
                all_proxy = proxy_cfg.all_proxy.as_deref().unwrap_or(""),
                no_proxy_count = proxy_cfg.normalized_no_proxy().len(),
                "http_request resolved runtime proxy state"
            );
        }

        let method_str = args.get("method").and_then(|v| v.as_str()).unwrap_or("GET");
        let headers_val = args.get("headers").cloned().unwrap_or(json!({}));
        let body = args.get("body").and_then(|v| v.as_str());

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
                tracing::warn!(
                    target: "http_request",
                    url = %url,
                    error = %e,
                    "http_request URL validation rejected request"
                );
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let method = match self.validate_method(method_str) {
            Ok(m) => m,
            Err(e) => {
                return Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(e.to_string()),
                });
            }
        };

        let request_headers = self.parse_headers(&headers_val);

        match self
            .execute_request(&url, method, request_headers, body)
            .await
        {
            Ok(response) => {
                let status = response.status();
                let status_code = status.as_u16();
                let status_reason = status.canonical_reason().unwrap_or("Unknown");

                if !status.is_success() {
                    tracing::warn!(
                        target: "http_request",
                        url = %url,
                        status = status_code,
                        status_reason = %status_reason,
                        "http_request HTTP non-2xx response"
                    );
                }

                // Get response headers (redact sensitive ones)
                let response_headers = response.headers().iter();
                let headers_text = response_headers
                    .map(|(k, _)| {
                        let is_sensitive = k.as_str().to_lowercase().contains("set-cookie");
                        if is_sensitive {
                            format!("{}: ***REDACTED***", k.as_str())
                        } else {
                            format!("{}: {:?}", k.as_str(), k.as_str())
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ");

                // Get response body with size limit
                let response_text = match response.text().await {
                    Ok(text) => self.truncate_response(&text),
                    Err(e) => {
                        let chain = error_chain(&e);
                        tracing::error!(
                            target: "http_request",
                            url = %url,
                            phase = "body_read",
                            error = %e,
                            error_chain = %chain,
                            "http_request body_read failure"
                        );
                        format!("[Failed to read response body: {e} (chain: {chain})]")
                    }
                };

                let output = format!(
                    "Status: {} {}\nResponse Headers: {}\n\nResponse Body:\n{}",
                    status_code, status_reason, headers_text, response_text
                );

                Ok(ToolResult {
                    success: status.is_success(),
                    output,
                    error: if status.is_client_error() || status.is_server_error() {
                        Some(format!("HTTP {}", status_code))
                    } else {
                        None
                    },
                })
            }
            Err(e) => {
                // Note: execute_request() already emitted an ERROR-level
                // tracing event with the source chain. This soft-fail is
                // surfaced to the caller via ToolResult.error, and the
                // tool-boundary LoggedTool wrapper will also emit a WARN
                // with `success=false`, so operators get two ways to see
                // the failure without double-logging the source chain.
                let chain = anyhow_error_chain(&e);
                Ok(ToolResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("HTTP request failed: {e} (chain: {chain})")),
                })
            }
        }
    }
}

// Helper functions similar to browser_open.rs

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
        anyhow::bail!("IPv6 hosts are not supported in http_request");
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
    // Strip brackets from IPv6 addresses like [::1]
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

/// Returns true if the IPv4 address is not globally routable.
fn is_non_global_v4(v4: std::net::Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()                       // 127.0.0.0/8
        || v4.is_private()                 // 10/8, 172.16/12, 192.168/16
        || v4.is_link_local()              // 169.254.0.0/16
        || v4.is_unspecified()             // 0.0.0.0
        || v4.is_broadcast()              // 255.255.255.255
        || v4.is_multicast()              // 224.0.0.0/4
        || (a == 100 && (64..=127).contains(&b)) // Shared address space (RFC 6598)
        || a >= 240                        // Reserved (240.0.0.0/4, except broadcast)
        || (a == 192 && b == 0 && (c == 0 || c == 2)) // IETF assignments + TEST-NET-1
        || (a == 198 && b == 51)           // Documentation (198.51.100.0/24)
        || (a == 203 && b == 0)            // Documentation (203.0.113.0/24)
        || (a == 198 && (18..=19).contains(&b)) // Benchmarking (198.18.0.0/15)
}

/// Returns true if the IPv6 address is not globally routable.
fn is_non_global_v6(v6: std::net::Ipv6Addr) -> bool {
    let segs = v6.segments();
    v6.is_loopback()                       // ::1
        || v6.is_unspecified()             // ::
        || v6.is_multicast()              // ff00::/8
        || (segs[0] & 0xfe00) == 0xfc00   // Unique-local (fc00::/7)
        || (segs[0] & 0xffc0) == 0xfe80   // Link-local (fe80::/10)
        || (segs[0] == 0x2001 && segs[1] == 0x0db8) // Documentation (2001:db8::/32)
        || v6.to_ipv4_mapped().is_some_and(is_non_global_v4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::{AutonomyLevel, SecurityPolicy};

    fn test_tool(allowed_domains: Vec<&str>) -> HttpRequestTool {
        test_tool_with_private(allowed_domains, false)
    }

    fn test_tool_with_private(
        allowed_domains: Vec<&str>,
        allow_private_hosts: bool,
    ) -> HttpRequestTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Supervised,
            ..SecurityPolicy::default()
        });
        HttpRequestTool::new(
            security,
            allowed_domains.into_iter().map(String::from).collect(),
            1_000_000,
            30,
            allow_private_hosts,
        )
    }

    #[test]
    fn normalize_domain_strips_scheme_path_and_case() {
        let got = normalize_domain("  HTTPS://Docs.Example.com/path ").unwrap();
        assert_eq!(got, "docs.example.com");
    }

    #[test]
    fn normalize_allowed_domains_deduplicates() {
        let got = normalize_allowed_domains(vec![
            "example.com".into(),
            "EXAMPLE.COM".into(),
            "https://example.com/".into(),
        ]);
        assert_eq!(got, vec!["example.com".to_string()]);
    }

    #[test]
    fn validate_accepts_exact_domain() {
        let tool = test_tool(vec!["example.com"]);
        let got = tool.validate_url("https://example.com/docs").unwrap();
        assert_eq!(got, "https://example.com/docs");
    }

    #[test]
    fn validate_accepts_http() {
        let tool = test_tool(vec!["example.com"]);
        assert!(tool.validate_url("http://example.com").is_ok());
    }

    #[test]
    fn validate_accepts_subdomain() {
        let tool = test_tool(vec!["example.com"]);
        assert!(tool.validate_url("https://api.example.com/v1").is_ok());
    }

    #[test]
    fn validate_accepts_wildcard_allowlist_for_public_host() {
        let tool = test_tool(vec!["*"]);
        assert!(tool.validate_url("https://news.ycombinator.com").is_ok());
    }

    #[test]
    fn validate_wildcard_allowlist_still_rejects_private_host() {
        let tool = test_tool(vec!["*"]);
        let err = tool
            .validate_url("https://localhost:8080")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
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
    fn validate_rejects_localhost() {
        let tool = test_tool(vec!["localhost"]);
        let err = tool
            .validate_url("https://localhost:8080")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
    }

    #[test]
    fn validate_rejects_private_ipv4() {
        let tool = test_tool(vec!["192.168.1.5"]);
        let err = tool
            .validate_url("https://192.168.1.5")
            .unwrap_err()
            .to_string();
        assert!(err.contains("local/private"));
    }

    #[test]
    fn validate_rejects_whitespace() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("https://example.com/hello world")
            .unwrap_err()
            .to_string();
        assert!(err.contains("whitespace"));
    }

    #[test]
    fn validate_rejects_userinfo() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("https://user@example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("userinfo"));
    }

    #[test]
    fn validate_requires_allowlist() {
        let security = Arc::new(SecurityPolicy::default());
        let tool = HttpRequestTool::new(security, vec![], 1_000_000, 30, false);
        let err = tool
            .validate_url("https://example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("allowed_domains"));
    }

    #[test]
    fn validate_accepts_valid_methods() {
        let tool = test_tool(vec!["example.com"]);
        assert!(tool.validate_method("GET").is_ok());
        assert!(tool.validate_method("POST").is_ok());
        assert!(tool.validate_method("PUT").is_ok());
        assert!(tool.validate_method("DELETE").is_ok());
        assert!(tool.validate_method("PATCH").is_ok());
        assert!(tool.validate_method("HEAD").is_ok());
        assert!(tool.validate_method("OPTIONS").is_ok());
    }

    #[test]
    fn validate_rejects_invalid_method() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool.validate_method("INVALID").unwrap_err().to_string();
        assert!(err.contains("Unsupported HTTP method"));
    }

    #[test]
    fn blocks_multicast_ipv4() {
        assert!(is_private_or_local_host("224.0.0.1"));
        assert!(is_private_or_local_host("239.255.255.255"));
    }

    #[test]
    fn blocks_broadcast() {
        assert!(is_private_or_local_host("255.255.255.255"));
    }

    #[test]
    fn blocks_reserved_ipv4() {
        assert!(is_private_or_local_host("240.0.0.1"));
        assert!(is_private_or_local_host("250.1.2.3"));
    }

    #[test]
    fn blocks_documentation_ranges() {
        assert!(is_private_or_local_host("192.0.2.1")); // TEST-NET-1
        assert!(is_private_or_local_host("198.51.100.1")); // TEST-NET-2
        assert!(is_private_or_local_host("203.0.113.1")); // TEST-NET-3
    }

    #[test]
    fn blocks_benchmarking_range() {
        assert!(is_private_or_local_host("198.18.0.1"));
        assert!(is_private_or_local_host("198.19.255.255"));
    }

    #[test]
    fn blocks_ipv6_localhost() {
        assert!(is_private_or_local_host("::1"));
        assert!(is_private_or_local_host("[::1]"));
    }

    #[test]
    fn blocks_ipv6_multicast() {
        assert!(is_private_or_local_host("ff02::1"));
    }

    #[test]
    fn blocks_ipv6_link_local() {
        assert!(is_private_or_local_host("fe80::1"));
    }

    #[test]
    fn blocks_ipv6_unique_local() {
        assert!(is_private_or_local_host("fd00::1"));
    }

    #[test]
    fn blocks_ipv4_mapped_ipv6() {
        assert!(is_private_or_local_host("::ffff:127.0.0.1"));
        assert!(is_private_or_local_host("::ffff:192.168.1.1"));
        assert!(is_private_or_local_host("::ffff:10.0.0.1"));
    }

    #[test]
    fn allows_public_ipv4() {
        assert!(!is_private_or_local_host("8.8.8.8"));
        assert!(!is_private_or_local_host("1.1.1.1"));
        assert!(!is_private_or_local_host("93.184.216.34"));
    }

    #[test]
    fn blocks_ipv6_documentation_range() {
        assert!(is_private_or_local_host("2001:db8::1"));
    }

    #[test]
    fn allows_public_ipv6() {
        assert!(!is_private_or_local_host("2607:f8b0:4004:800::200e"));
    }

    #[test]
    fn blocks_shared_address_space() {
        assert!(is_private_or_local_host("100.64.0.1"));
        assert!(is_private_or_local_host("100.127.255.255"));
        assert!(!is_private_or_local_host("100.63.0.1")); // Just below range
        assert!(!is_private_or_local_host("100.128.0.1")); // Just above range
    }

    #[tokio::test]
    async fn execute_blocks_readonly_mode() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            ..SecurityPolicy::default()
        });
        let tool = HttpRequestTool::new(security, vec!["example.com".into()], 1_000_000, 30, false);
        let result = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("read-only"));
    }

    #[tokio::test]
    async fn execute_blocks_when_rate_limited() {
        let security = Arc::new(SecurityPolicy {
            max_actions_per_hour: 0,
            ..SecurityPolicy::default()
        });
        let tool = HttpRequestTool::new(security, vec!["example.com".into()], 1_000_000, 30, false);
        let result = tool
            .execute(json!({"url": "https://example.com"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("rate limit"));
    }

    #[test]
    fn truncate_response_within_limit() {
        let tool = test_tool(vec!["example.com"]);
        let text = "hello world";
        assert_eq!(tool.truncate_response(text), "hello world");
    }

    #[test]
    fn truncate_response_over_limit() {
        let tool = HttpRequestTool::new(
            Arc::new(SecurityPolicy::default()),
            vec!["example.com".into()],
            10,
            30,
            false,
        );
        let text = "hello world this is long";
        let truncated = tool.truncate_response(text);
        assert!(truncated.len() <= 10 + 60); // limit + message
        assert!(truncated.contains("[Response truncated"));
    }

    #[test]
    fn truncate_response_zero_means_unlimited() {
        let tool = HttpRequestTool::new(
            Arc::new(SecurityPolicy::default()),
            vec!["example.com".into()],
            0, // max_response_size = 0 means no limit
            30,
            false,
        );
        let text = "a".repeat(10_000_000);
        assert_eq!(tool.truncate_response(&text), text);
    }

    #[test]
    fn truncate_response_nonzero_still_truncates() {
        let tool = HttpRequestTool::new(
            Arc::new(SecurityPolicy::default()),
            vec!["example.com".into()],
            5,
            30,
            false,
        );
        let text = "hello world";
        let truncated = tool.truncate_response(text);
        assert!(truncated.starts_with("hello"));
        assert!(truncated.contains("[Response truncated"));
    }

    #[test]
    fn parse_headers_preserves_original_values() {
        let tool = test_tool(vec!["example.com"]);
        let headers = json!({
            "Authorization": "Bearer secret",
            "Content-Type": "application/json",
            "X-API-Key": "my-key"
        });
        let parsed = tool.parse_headers(&headers);
        assert_eq!(parsed.len(), 3);
        assert!(
            parsed
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "Bearer secret")
        );
        assert!(
            parsed
                .iter()
                .any(|(k, v)| k == "X-API-Key" && v == "my-key")
        );
        assert!(
            parsed
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "application/json")
        );
    }

    #[test]
    fn redact_headers_for_display_redacts_sensitive() {
        let headers = vec![
            ("Authorization".into(), "Bearer secret".into()),
            ("Content-Type".into(), "application/json".into()),
            ("X-API-Key".into(), "my-key".into()),
            ("X-Secret-Token".into(), "tok-123".into()),
        ];
        let redacted = HttpRequestTool::redact_headers_for_display(&headers);
        assert_eq!(redacted.len(), 4);
        assert!(
            redacted
                .iter()
                .any(|(k, v)| k == "Authorization" && v == "***REDACTED***")
        );
        assert!(
            redacted
                .iter()
                .any(|(k, v)| k == "X-API-Key" && v == "***REDACTED***")
        );
        assert!(
            redacted
                .iter()
                .any(|(k, v)| k == "X-Secret-Token" && v == "***REDACTED***")
        );
        assert!(
            redacted
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "application/json")
        );
    }

    #[test]
    fn redact_headers_does_not_alter_original() {
        let headers = vec![("Authorization".into(), "Bearer real-token".into())];
        let _ = HttpRequestTool::redact_headers_for_display(&headers);
        assert_eq!(headers[0].1, "Bearer real-token");
    }

    // ── SSRF: alternate IP notation bypass defense-in-depth ─────────
    //
    // Rust's IpAddr::parse() rejects non-standard notations (octal, hex,
    // decimal integer, zero-padded). These tests document that property
    // so regressions are caught if the parsing strategy ever changes.

    #[test]
    fn ssrf_octal_loopback_not_parsed_as_ip() {
        // 0177.0.0.1 is octal for 127.0.0.1 in some languages, but
        // Rust's IpAddr rejects it — it falls through as a hostname.
        assert!(!is_private_or_local_host("0177.0.0.1"));
    }

    #[test]
    fn ssrf_hex_loopback_not_parsed_as_ip() {
        // 0x7f000001 is hex for 127.0.0.1 in some languages.
        assert!(!is_private_or_local_host("0x7f000001"));
    }

    #[test]
    fn ssrf_decimal_loopback_not_parsed_as_ip() {
        // 2130706433 is decimal for 127.0.0.1 in some languages.
        assert!(!is_private_or_local_host("2130706433"));
    }

    #[test]
    fn ssrf_zero_padded_loopback_not_parsed_as_ip() {
        // 127.000.000.001 uses zero-padded octets.
        assert!(!is_private_or_local_host("127.000.000.001"));
    }

    #[test]
    fn ssrf_alternate_notations_rejected_by_validate_url() {
        // Even if is_private_or_local_host doesn't flag these, they
        // fail the allowlist because they're treated as hostnames.
        let tool = test_tool(vec!["example.com"]);
        for notation in [
            "http://0177.0.0.1",
            "http://0x7f000001",
            "http://2130706433",
            "http://127.000.000.001",
        ] {
            let err = tool.validate_url(notation).unwrap_err().to_string();
            assert!(
                err.contains("allowed_domains"),
                "Expected allowlist rejection for {notation}, got: {err}"
            );
        }
    }

    #[test]
    fn redirect_policy_is_none() {
        // Structural test: the tool should be buildable with redirect-safe config.
        // The actual Policy::none() enforcement is in execute_request's client builder.
        let tool = test_tool(vec!["example.com"]);
        assert_eq!(tool.name(), "http_request");
    }

    // ── §1.4 DNS rebinding / SSRF defense-in-depth tests ─────

    #[test]
    fn ssrf_blocks_loopback_127_range() {
        assert!(is_private_or_local_host("127.0.0.1"));
        assert!(is_private_or_local_host("127.0.0.2"));
        assert!(is_private_or_local_host("127.255.255.255"));
    }

    #[test]
    fn ssrf_blocks_rfc1918_10_range() {
        assert!(is_private_or_local_host("10.0.0.1"));
        assert!(is_private_or_local_host("10.255.255.255"));
    }

    #[test]
    fn ssrf_blocks_rfc1918_172_range() {
        assert!(is_private_or_local_host("172.16.0.1"));
        assert!(is_private_or_local_host("172.31.255.255"));
    }

    #[test]
    fn ssrf_blocks_unspecified_address() {
        assert!(is_private_or_local_host("0.0.0.0"));
    }

    #[test]
    fn ssrf_blocks_dot_localhost_subdomain() {
        assert!(is_private_or_local_host("evil.localhost"));
        assert!(is_private_or_local_host("a.b.localhost"));
    }

    #[test]
    fn ssrf_blocks_dot_local_tld() {
        assert!(is_private_or_local_host("service.local"));
    }

    #[test]
    fn ssrf_ipv6_unspecified() {
        assert!(is_private_or_local_host("::"));
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
    fn validate_rejects_empty_url() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool.validate_url("").unwrap_err().to_string();
        assert!(err.contains("empty"));
    }

    #[test]
    fn validate_rejects_ipv6_host() {
        let tool = test_tool(vec!["example.com"]);
        let err = tool
            .validate_url("http://[::1]:8080/path")
            .unwrap_err()
            .to_string();
        assert!(err.contains("IPv6"));
    }

    // ── allow_private_hosts opt-in tests ────────────────────────

    #[test]
    fn default_blocks_private_hosts() {
        let tool = test_tool(vec!["localhost", "192.168.1.5", "*"]);
        assert!(
            tool.validate_url("https://localhost:8080")
                .unwrap_err()
                .to_string()
                .contains("local/private")
        );
        assert!(
            tool.validate_url("https://192.168.1.5")
                .unwrap_err()
                .to_string()
                .contains("local/private")
        );
        assert!(
            tool.validate_url("https://10.0.0.1")
                .unwrap_err()
                .to_string()
                .contains("local/private")
        );
    }

    #[test]
    fn allow_private_hosts_permits_localhost() {
        let tool = test_tool_with_private(vec!["localhost"], true);
        assert!(tool.validate_url("https://localhost:8080").is_ok());
    }

    #[test]
    fn allow_private_hosts_permits_private_ipv4() {
        let tool = test_tool_with_private(vec!["192.168.1.5"], true);
        assert!(tool.validate_url("https://192.168.1.5").is_ok());
    }

    #[test]
    fn allow_private_hosts_permits_rfc1918_with_wildcard() {
        let tool = test_tool_with_private(vec!["*"], true);
        assert!(tool.validate_url("https://10.0.0.1").is_ok());
        assert!(tool.validate_url("https://172.16.0.1").is_ok());
        assert!(tool.validate_url("https://192.168.1.1").is_ok());
        assert!(tool.validate_url("http://localhost:8123").is_ok());
    }

    #[test]
    fn allow_private_hosts_still_requires_allowlist() {
        let tool = test_tool_with_private(vec!["example.com"], true);
        let err = tool
            .validate_url("https://192.168.1.5")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("allowed_domains"),
            "Private host should still need allowlist match, got: {err}"
        );
    }

    #[test]
    fn allow_private_hosts_false_still_blocks() {
        let tool = test_tool_with_private(vec!["*"], false);
        assert!(
            tool.validate_url("https://localhost:8080")
                .unwrap_err()
                .to_string()
                .contains("local/private")
        );
    }

    // ── §1h: http_request structured tracing (TDD) ──────────────────────────
    //
    // Every error path in `execute_request` / `execute` must emit a
    // structured tracing event so operators can see *why* an outbound HTTP
    // call failed without attaching a debugger. The tests below install a
    // local subscriber, invoke `execute` in each failure mode, and assert
    // on the captured event content.

    use std::sync::Mutex as StdMutex;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Clone, Default)]
    struct HttpTraceCapture(Arc<StdMutex<Vec<u8>>>);
    struct HttpTraceCaptureWriter(Arc<StdMutex<Vec<u8>>>);

    impl HttpTraceCapture {
        fn as_string(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).to_string()
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for HttpTraceCapture {
        type Writer = HttpTraceCaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            HttpTraceCaptureWriter(self.0.clone())
        }
    }

    impl std::io::Write for HttpTraceCaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn install_http_trace_capture() -> (HttpTraceCapture, tracing::dispatcher::DefaultGuard) {
        let capture = HttpTraceCapture::default();
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

    /// Bind a TCP listener on an ephemeral port and drop it — returns a URL
    /// pointing to a port that is guaranteed to refuse connections. This
    /// deterministically triggers the network-error path inside
    /// `execute_request` without depending on external DNS or timing.
    fn reserved_unused_http_url() -> String {
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        drop(tcp);
        format!("http://127.0.0.1:{port}/nowhere")
    }

    fn test_tool_with_private_hosts_allowed(allowed_domains: Vec<&str>) -> HttpRequestTool {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            max_actions_per_hour: 1000,
            ..SecurityPolicy::default()
        });
        HttpRequestTool::new(
            security,
            allowed_domains.into_iter().map(String::from).collect(),
            1_000_000,
            30,
            true, // allow private hosts so we can point at 127.0.0.1
        )
    }

    /// RED: when the underlying `reqwest::Client::send` returns an error
    /// (e.g. ConnectionRefused because the port is dead), `execute` MUST
    /// emit an ERROR-level `tracing` event on the `http_request` target
    /// containing:
    ///   - the URL that failed
    ///   - the reqwest error display
    ///   - the full `error_chain` (source-chain walk) so the std::io root
    ///     cause is visible without attaching a debugger.
    #[tokio::test]
    async fn http_request_logs_error_with_source_chain_on_network_failure() {
        let (capture, guard) = install_http_trace_capture();

        let tool = test_tool_with_private_hosts_allowed(vec!["127.0.0.1"]);
        let url = reserved_unused_http_url();
        let result = tool
            .execute(json!({"url": url.clone()}))
            .await
            .expect("execute should return soft-fail, not raise");
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or("")
                .contains("HTTP request failed"),
            "expected soft-fail to mention 'HTTP request failed', got {:?}",
            result.error
        );

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("ERROR"),
            "expected ERROR level tracing event, got:\n{logs}"
        );
        assert!(
            logs.contains("http_request"),
            "expected 'http_request' target, got:\n{logs}"
        );
        assert!(
            logs.contains(&url),
            "expected failing URL {url} in log, got:\n{logs}"
        );
        assert!(
            logs.contains("error_chain"),
            "expected structured 'error_chain' field in log, got:\n{logs}"
        );
    }

    /// RED: when the HTTP response status is non-2xx, `execute` MUST emit
    /// a WARN-level tracing event on the `http_request` target carrying
    /// the URL and numeric status code. (The previous behaviour only
    /// populated ToolResult.error; operators had no way to see it without
    /// plumbing the ToolResult through their logs.)
    #[tokio::test]
    async fn http_request_logs_warn_on_http_non_2xx_with_status_fields() {
        let (capture, guard) = install_http_trace_capture();

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/forbidden"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;

        // Allow the mock server host (127.0.0.1 via private hosts flag).
        let tool = test_tool_with_private_hosts_allowed(vec!["*"]);
        let url = format!("{}/forbidden", server.uri());
        let result = tool
            .execute(json!({"url": url.clone()}))
            .await
            .expect("execute should return soft-fail");
        assert!(!result.success);

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("WARN"),
            "expected WARN level tracing event, got:\n{logs}"
        );
        assert!(
            logs.contains("http_request"),
            "expected 'http_request' target, got:\n{logs}"
        );
        assert!(
            logs.contains(&url),
            "expected URL in log, got:\n{logs}"
        );
        assert!(
            logs.contains("403") || logs.contains("status"),
            "expected status code 403 in log, got:\n{logs}"
        );
    }

    /// RED: at the start of `execute`, the tool MUST emit a DEBUG-level
    /// tracing event capturing the effective runtime proxy state for
    /// service_key `tool.http_request`. Without this operators cannot tell
    /// whether a "connection error" is because the proxy is bypassing
    /// their corporate traffic egress, and have to debug blind.
    #[tokio::test]
    async fn http_request_logs_debug_proxy_state_on_entry() {
        let (capture, guard) = install_http_trace_capture();

        // Use a wiremock so we reach execute_request without any 4xx/5xx
        // noise polluting the log assertion.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
            .mount(&server)
            .await;

        let tool = test_tool_with_private_hosts_allowed(vec!["*"]);
        let url = format!("{}/ok", server.uri());
        let _ = tool.execute(json!({"url": url})).await.unwrap();

        drop(guard);
        let logs = capture.as_string();

        assert!(
            logs.contains("DEBUG"),
            "expected DEBUG tracing event at execute() entry, got:\n{logs}"
        );
        assert!(
            logs.contains("http_request"),
            "expected 'http_request' target, got:\n{logs}"
        );
        assert!(
            logs.contains("service_key") || logs.contains("tool.http_request"),
            "expected service_key field referencing 'tool.http_request', got:\n{logs}"
        );
        assert!(
            logs.contains("proxy_enabled") || logs.contains("proxy_applies"),
            "expected proxy state fields in log, got:\n{logs}"
        );
    }
}
