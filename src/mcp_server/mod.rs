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
    /// Whether to expose the full ZeroClaw tool surface (web_fetch,
    /// http_request, calculator, memory_*, cron_*, …) to MCP clients.
    /// When `true`, the transport calls `Config::load_or_init` itself
    /// and builds the full `all_tools_with_runtime` registry. When
    /// `false`, it falls back to the 6-tool `default_tools` subset for
    /// backward compatibility with older call sites and tests.
    ///
    /// We let the transport load its own config (rather than having the
    /// caller pass a `Config` struct) because ZeroClaw's `src/main.rs`
    /// includes `mod config;` as its own compilation unit, so the
    /// binary crate's `config::Config` and the library crate's
    /// `zeroclaw::Config` are nominally distinct types. Loading inside
    /// the lib sidesteps that entirely.
    pub expose_full_surface: bool,
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
