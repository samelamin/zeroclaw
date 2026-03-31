//! MCP (Model Context Protocol) server for ZeroClaw.
//!
//! Exposes ZeroClaw's tools over MCP protocol, following Claude Code's
//! `mcp` subcommand pattern. Supports stdio and HTTP+SSE transports.

pub mod handlers;
pub mod transport;

/// MCP server configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub transport: TransportMode,
    pub port: u16,
    pub api_key: Option<String>,
    pub debug: bool,
    pub workspace_dir: std::path::PathBuf,
}

#[derive(Debug, Clone)]
pub enum TransportMode {
    Stdio,
    Http,
}

/// Start the MCP server with the given config.
pub async fn serve(config: McpServerConfig) -> anyhow::Result<()> {
    tracing::info!(
        transport = ?config.transport,
        "Starting ZeroClaw MCP server v{}",
        env!("CARGO_PKG_VERSION"),
    );

    match config.transport {
        TransportMode::Stdio => transport::serve_stdio(&config).await,
        TransportMode::Http => transport::serve_http(&config).await,
    }
}
