mod catalog;
mod higgsfield;
mod server;

use anyhow::Result;
use rmcp::ServiceExt;

use crate::higgsfield::Credentials;
use crate::server::HiggsfieldMcp;

const USAGE: &str = "\
higgsfield-mcp: local MCP server for the pay-as-you-go Higgsfield API.

Run it from an MCP client (Claude Code, Claude Desktop); it speaks MCP over stdio.

Environment:
  HF_API_KEY_ID, HF_API_KEY_SECRET  Higgsfield API key from https://cloud.higgsfield.ai
  HF_OUTPUT_DIR                     Where download_output saves files (default ~/Downloads/higgsfield)
  RUST_LOG                          Log filter, written to stderr (default info)";

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> Result<()> {
    // `--stdio` is accepted for older client configs; stdio is the only transport.
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--stdio" => {}
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            "-V" | "--version" => {
                println!("higgsfield-mcp {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    // Logs go to stderr so they never corrupt the stdio transport.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Start without a key so the free catalog tools still work; paid tools explain what's missing.
    let creds = match (env_opt("HF_API_KEY_ID"), env_opt("HF_API_KEY_SECRET")) {
        (Some(id), Some(secret)) => Some(Credentials::new(&id, &secret)?),
        _ => {
            tracing::warn!("HF_API_KEY_ID / HF_API_KEY_SECRET not set; only list_models and get_model_docs will work");
            None
        }
    };
    tracing::info!("serving MCP over stdio");
    let service = HiggsfieldMcp::new(creds).serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}
