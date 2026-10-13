mod catalog;
mod higgsfield;
mod server;

use anyhow::Result;
use axum::{Router, routing::get};
use rmcp::{
    ServiceExt,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};

use crate::higgsfield::Credentials;
use crate::server::HiggsfieldMcp;

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Logs go to stderr so they never corrupt the stdio transport.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let stdio = args.iter().any(|a| a == "--stdio")
        || (!args.iter().any(|a| a == "--http") && env_opt("PORT").is_none());

    if stdio {
        // Local mode: the key comes from this machine's environment.
        let creds = match (env_opt("HF_API_KEY_ID"), env_opt("HF_API_KEY_SECRET")) {
            (Some(id), Some(secret)) => Some(Credentials::new(&id, &secret)?),
            _ => {
                tracing::warn!("HF_API_KEY_ID / HF_API_KEY_SECRET not set; paid tools will fail");
                None
            }
        };
        tracing::info!("serving MCP over stdio");
        let service = HiggsfieldMcp::new(creds).serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        return Ok(());
    }
    serve_http().await
}

/// Public multi-tenant mode: no server-side credentials; every caller sends their own key.
async fn serve_http() -> Result<()> {
    let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    if let Some(hosts) = env_opt("MCP_ALLOWED_HOSTS") {
        config = config.with_allowed_hosts(hosts.split(',').map(|h| h.trim().to_string()));
    }
    config.max_request_body_bytes = 150 * 1024 * 1024;
    let mcp = HiggsfieldMcp::new(None);
    let mcp_service: StreamableHttpService<HiggsfieldMcp, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(mcp.clone()), Default::default(), config);

    let app = Router::new()
        .route("/", get(|| async { "higgsfield-mcp: public MCP endpoint at /mcp. Send your own key as `Authorization: Key <KEY_ID>:<KEY_SECRET>`." }))
        .route("/health", get(|| async { "ok" }))
        .nest_service("/mcp", mcp_service);

    let port: u16 = env_opt("PORT").map(|p| p.parse()).transpose()?.unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("serving public MCP over streamable HTTP on 0.0.0.0:{port}/mcp");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
