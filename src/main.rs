mod catalog;
mod higgsfield;
mod server;

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, Uri, header},
    middleware::Next,
    response::{IntoResponse, Response},
    routing::get,
};
use rmcp::{
    ServiceExt,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};
use subtle::ConstantTimeEq;
use tower::Layer;

use crate::higgsfield::HiggsfieldClient;
use crate::server::{HiggsfieldMcp, SpendGuard};

fn env_opt(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn env_f64(name: &str) -> Result<Option<f64>> {
    env_opt(name)
        .map(|v| v.parse::<f64>().with_context(|| format!("{name} must be a number")))
        .transpose()
}

fn env_bool(name: &str) -> bool {
    env_opt(name).is_some_and(|v| matches!(v.to_lowercase().as_str(), "1" | "true" | "yes"))
}

fn build_server() -> Result<HiggsfieldMcp> {
    let key_id = env_opt("HF_API_KEY_ID").context("HF_API_KEY_ID is not set (create one at https://cloud.higgsfield.ai)")?;
    let key_secret = env_opt("HF_API_KEY_SECRET").context("HF_API_KEY_SECRET is not set")?;
    let per_request = env_f64("HF_MAX_USD_PER_REQUEST")?.unwrap_or(5.0);
    let per_day = env_f64("HF_MAX_USD_PER_DAY")?;
    if per_request <= 0.0 || per_day.is_some_and(|d| d <= 0.0) {
        bail!("spend caps must be positive");
    }
    let client = HiggsfieldClient::new(&key_id, &key_secret)?;
    Ok(HiggsfieldMcp::new(
        client,
        SpendGuard::new(per_request, per_day),
        env_bool("HF_ALLOW_UNESTIMATED"),
    ))
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

    let mcp = build_server()?;
    if stdio {
        tracing::info!("serving MCP over stdio");
        let service = mcp.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        return Ok(());
    }
    serve_http(mcp).await
}

#[derive(Clone)]
struct AuthState {
    token: Option<Arc<String>>,
}

async fn serve_http(mcp: HiggsfieldMcp) -> Result<()> {
    let token = env_opt("MCP_AUTH_TOKEN");
    match &token {
        Some(t) if t.len() < 24 => bail!("MCP_AUTH_TOKEN must be at least 24 characters"),
        None if !env_bool("MCP_ALLOW_NO_AUTH") => bail!(
            "MCP_AUTH_TOKEN is required in HTTP mode because this server spends real money (set MCP_ALLOW_NO_AUTH=true to override)"
        ),
        None => tracing::warn!("running WITHOUT authentication"),
        _ => {}
    }

    let mut config = StreamableHttpServerConfig::default().disable_allowed_hosts();
    if let Some(hosts) = env_opt("MCP_ALLOWED_HOSTS") {
        config = config.with_allowed_hosts(hosts.split(',').map(|h| h.trim().to_string()));
    }
    config.max_request_body_bytes = 150 * 1024 * 1024;
    let mcp_service: StreamableHttpService<HiggsfieldMcp, LocalSessionManager> =
        StreamableHttpService::new(move || Ok(mcp.clone()), Default::default(), config);

    let router = Router::new()
        .route("/", get(|| async { "higgsfield-mcp: MCP endpoint is /mcp" }))
        .route("/health", get(|| async { "ok" }))
        .nest_service("/mcp", mcp_service);
    // Wrap the whole router so the path rewrite in `auth` happens before routing.
    let app = axum::middleware::from_fn_with_state(AuthState { token: token.map(Arc::new) }, auth)
        .layer(router);

    let port: u16 = env_opt("PORT").map(|p| p.parse()).transpose()?.unwrap_or(8080);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("serving MCP over streamable HTTP on 0.0.0.0:{port}/mcp");
    axum::serve(listener, axum::ServiceExt::<Request>::into_make_service(app))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// Accepts `Authorization: Bearer <token>` on /mcp, or the token as a path
/// segment (/mcp/<token>) for clients that cannot send custom headers.
async fn auth(State(state): State<AuthState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if !(path == "/mcp" || path.starts_with("/mcp/")) {
        return next.run(req).await;
    }
    let Some(expected) = state.token.as_deref() else {
        return next.run(req).await;
    };
    let eq = |given: &str| bool::from(given.as_bytes().ct_eq(expected.as_bytes()));

    if let Some(given) = path.strip_prefix("/mcp/") {
        if !eq(given.trim_end_matches('/')) {
            return unauthorized();
        }
        let pq = match req.uri().query() {
            Some(q) => format!("/mcp?{q}"),
            None => "/mcp".to_string(),
        };
        let mut parts = req.uri().clone().into_parts();
        parts.path_and_query = pq.parse().ok();
        if let Ok(uri) = Uri::from_parts(parts) {
            *req.uri_mut() = uri;
        }
        return next.run(req).await;
    }

    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(str::trim);
    match bearer {
        Some(t) if eq(t) => next.run(req).await,
        _ => unauthorized(),
    }
}

fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}
