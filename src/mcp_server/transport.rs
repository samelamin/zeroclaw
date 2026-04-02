//! MCP transport implementations (stdio and HTTP).

use super::McpServerConfig;
use crate::security::SecurityPolicy;
use crate::tools::mcp_protocol::JsonRpcRequest;
use crate::tools::traits::Tool;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Serve MCP over stdio (stdin/stdout JSON-RPC lines).
pub async fn serve_stdio(config: &McpServerConfig) -> anyhow::Result<()> {
    let security = Arc::new(SecurityPolicy {
        workspace_dir: config.workspace_dir.clone(),
        ..SecurityPolicy::default()
    });
    let tools = crate::tools::default_tools(security);

    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }

        let req: JsonRpcRequest = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                let err_resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {
                        "code": crate::tools::mcp_protocol::PARSE_ERROR,
                        "message": format!("Parse error: {e}"),
                    }
                });
                let out = serde_json::to_string(&err_resp)? + "\n";
                stdout.write_all(out.as_bytes()).await?;
                stdout.flush().await?;
                continue;
            }
        };

        let is_notification = req.id.is_none();
        let resp = super::handlers::handle_request(&req, &tools).await;

        // Don't send responses for notifications
        if !is_notification {
            let out = serde_json::to_string(&resp)? + "\n";
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }
    }

    Ok(())
}

/// Serve MCP over HTTP+SSE.
pub async fn serve_http(config: &McpServerConfig) -> anyhow::Result<()> {
    use axum::{Router, routing::get, routing::post};
    use std::net::SocketAddr;

    let security = Arc::new(SecurityPolicy {
        workspace_dir: config.workspace_dir.clone(),
        ..SecurityPolicy::default()
    });
    let tools: Vec<Box<dyn Tool>> = crate::tools::default_tools(security);
    let tools: Arc<Vec<Box<dyn Tool>>> = Arc::new(tools);

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/mcp", post(mcp_handler))
        .with_state(tools);

    let addr = SocketAddr::from(([127, 0, 0, 1], config.port));
    tracing::info!("MCP HTTP server listening on {addr}");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health_handler() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({
        "status": "ok",
        "server": "zeroclaw",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn mcp_handler(
    axum::extract::State(tools): axum::extract::State<Arc<Vec<Box<dyn Tool>>>>,
    axum::Json(req): axum::Json<JsonRpcRequest>,
) -> axum::Json<crate::tools::mcp_protocol::JsonRpcResponse> {
    let resp = super::handlers::handle_request(&req, &tools).await;
    axum::Json(resp)
}
