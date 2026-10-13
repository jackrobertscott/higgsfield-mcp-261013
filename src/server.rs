//! MCP tool surface for the Higgsfield API.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerConfig},
    schemars::{self, JsonSchema},
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::Mutex;

use crate::catalog::{Catalog, validate_endpoint};
use crate::higgsfield::{HiggsfieldClient, is_terminal};

const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

const INSTRUCTIONS: &str = "\
Higgsfield pay-as-you-go API: image and video generation billed in real US dollars (not subscription credits).

Workflow:
1. list_models to find a model workflow and its `endpoint` and `doc_path`.
2. get_model_docs(doc_path) to read the exact input schema. Parameters differ per model; never guess them.
3. If the user supplies local or private media, upload_file first and use the returned public_url in params (image_url, video_url, ...).
4. estimate_cost(endpoint, params) and tell the user the USD price before spending. Get the user's go-ahead unless they already approved a budget.
5. generate(endpoint, params, max_usd) - max_usd must be >= the estimate. The server also enforces its own per-request and daily caps.
6. Generation is asynchronous. Use wait_for_request (or generate with wait_seconds) and return the output URLs. Outputs expire after ~7 days.
Failed, nsfw and canceled requests are refunded. Only queued requests can be canceled.";

#[derive(Clone)]
pub struct SpendGuard {
    pub max_usd_per_request: f64,
    pub max_usd_per_day: Option<f64>,
    spent: Arc<Mutex<(u64, f64)>>,
}

impl SpendGuard {
    pub fn new(max_usd_per_request: f64, max_usd_per_day: Option<f64>) -> Self {
        Self {
            max_usd_per_request,
            max_usd_per_day,
            spent: Arc::new(Mutex::new((today(), 0.0))),
        }
    }

    async fn spent_today(&self) -> f64 {
        let mut s = self.spent.lock().await;
        if s.0 != today() {
            *s = (today(), 0.0);
        }
        s.1
    }

    /// Checks the caps and reserves `usd` against today's budget.
    async fn reserve(&self, usd: f64, caller_max: f64) -> Result<()> {
        if usd > caller_max + 1e-9 {
            bail!("estimated cost ${usd:.3} exceeds max_usd ${caller_max:.3}; ask the user to approve a higher budget");
        }
        if usd > self.max_usd_per_request + 1e-9 {
            bail!(
                "estimated cost ${usd:.3} exceeds this server's per-request cap ${:.2} (HF_MAX_USD_PER_REQUEST)",
                self.max_usd_per_request
            );
        }
        let mut s = self.spent.lock().await;
        if s.0 != today() {
            *s = (today(), 0.0);
        }
        if let Some(day_cap) = self.max_usd_per_day {
            if s.1 + usd > day_cap + 1e-9 {
                bail!(
                    "this would bring today's spend to ${:.3}, over the daily cap ${day_cap:.2} (HF_MAX_USD_PER_DAY). Spent so far today (UTC, since last restart): ${:.3}",
                    s.1 + usd,
                    s.1
                );
            }
        }
        s.1 += usd;
        Ok(())
    }

    async fn release(&self, usd: f64) {
        let mut s = self.spent.lock().await;
        s.1 = (s.1 - usd).max(0.0);
    }
}

fn today() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 86_400
}

#[derive(Clone)]
pub struct HiggsfieldMcp {
    client: HiggsfieldClient,
    catalog: Catalog,
    guard: SpendGuard,
    allow_unestimated: bool,
    tool_router: ToolRouter<Self>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListModelsArgs {
    /// Filter by output type: "image" or "video".
    pub category: Option<String>,
    /// Case-insensitive text filter on model name, provider, description, workflow or endpoint.
    pub search: Option<String>,
    /// Bypass the one-hour catalog cache.
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ModelDocsArgs {
    /// Workflow doc path from list_models, e.g. "kling-3/turbo-image-to-video" or "soul-2/generate".
    pub doc_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct EstimateArgs {
    /// Model endpoint from list_models, e.g. "/higgsfield-ai/soul/v2/standard".
    pub endpoint: String,
    /// Model input parameters exactly as documented by get_model_docs.
    pub params: Map<String, Value>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GenerateArgs {
    /// Model endpoint from list_models, e.g. "/higgsfield-ai/soul/v2/standard".
    pub endpoint: String,
    /// Model input parameters exactly as documented by get_model_docs.
    pub params: Map<String, Value>,
    /// Maximum USD the user approved for this single generation. The request is refused if the estimate is higher.
    pub max_usd: f64,
    /// Optional idempotency key. Reuse the same key (with identical endpoint and params) only to retry a submission whose result was lost; a new key is generated otherwise.
    pub idempotency_key: Option<String>,
    /// Seconds to wait for the result before returning (0-300, default 0). Use wait_for_request for longer jobs.
    #[serde(default)]
    pub wait_seconds: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RequestIdArgs {
    /// request_id returned by generate.
    pub request_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitArgs {
    /// request_id returned by generate.
    pub request_id: String,
    /// Maximum seconds to wait (1-600, default 120). Call again if still running.
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UploadArgs {
    /// Public https URL the server downloads and re-uploads to Higgsfield storage.
    pub source_url: Option<String>,
    /// Base64-encoded file contents (alternative to source_url). A data: URI prefix is accepted.
    pub base64_data: Option<String>,
    /// MIME type: image/jpeg, image/png, image/webp, image/gif, audio/wav, audio/x-wav or video/mp4. Inferred when omitted.
    pub content_type: Option<String>,
}

const ALLOWED_UPLOAD_TYPES: [&str; 8] = [
    "image/jpeg", "image/jpg", "image/png", "image/webp", "image/gif", "audio/wav", "audio/x-wav", "video/mp4",
];

fn to_json(v: impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string_pretty(&v).map_err(|e| e.to_string())
}

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

fn validate_request_id(id: &str) -> Result<&str> {
    let id = id.trim();
    if id.is_empty() || id.len() > 100 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("invalid request_id {id:?}");
    }
    Ok(id)
}

/// Pulls output media URLs out of a status payload regardless of output type.
fn collect_outputs(status: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(obj) = status.as_object() {
        for (k, v) in obj {
            if matches!(k.as_str(), "status_url" | "cancel_url") {
                continue;
            }
            collect_urls(v, &mut urls);
        }
    }
    urls.dedup();
    urls
}

fn collect_urls(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(o) => {
            if let Some(Value::String(u)) = o.get("url") {
                out.push(u.clone());
            }
            for (k, v) in o {
                if k != "url" {
                    collect_urls(v, out);
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|v| collect_urls(v, out)),
        _ => {}
    }
}

fn summarize(status: Value) -> Value {
    let outputs = collect_outputs(&status);
    let terminal = is_terminal(&status);
    let mut v = json!({ "terminal": terminal, "output_urls": outputs, "raw": status });
    if !terminal {
        v["hint"] = json!("Still running. Call wait_for_request again with this request_id.");
    }
    v
}

#[tool_router]
impl HiggsfieldMcp {
    pub fn new(client: HiggsfieldClient, guard: SpendGuard, allow_unestimated: bool) -> Self {
        Self {
            client,
            catalog: Catalog::default(),
            guard,
            allow_unestimated,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List Higgsfield API models and their workflows (name, endpoint, doc_path), read live from the Higgsfield docs. Free.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn list_models(&self, Parameters(args): Parameters<ListModelsArgs>) -> Result<String, String> {
        let families = self.catalog.families(args.refresh).await.map_err(err)?;
        let category = args.category.map(|c| c.to_lowercase());
        let search = args.search.map(|s| s.to_lowercase());
        let filtered: Vec<_> = families
            .into_iter()
            .filter(|f| category.as_ref().is_none_or(|c| &f.category == c))
            .filter(|f| {
                search.as_ref().is_none_or(|s| {
                    let mut hay = format!("{} {} {} {}", f.name, f.provider, f.description, f.doc_path);
                    for w in &f.workflows {
                        hay.push_str(&format!(" {} {} {}", w.name, w.endpoint, w.doc_path));
                    }
                    hay.to_lowercase().contains(s.as_str())
                })
            })
            .collect();
        to_json(json!({ "count": filtered.len(), "models": filtered }))
    }

    #[tool(
        description = "Get the API reference for a model workflow: endpoint, usage notes, input parameters, full JSON schema and output shape. Call before estimate_cost/generate. Free.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn get_model_docs(&self, Parameters(args): Parameters<ModelDocsArgs>) -> Result<String, String> {
        self.catalog.model_docs(&args.doc_path).await.map_err(err)
    }

    #[tool(
        description = "Estimate the USD and credit cost of a generation without running it. Free. Always do this and tell the user the price before calling generate.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn estimate_cost(&self, Parameters(args): Parameters<EstimateArgs>) -> Result<String, String> {
        let endpoint = validate_endpoint(&args.endpoint).map_err(err)?;
        let est = self
            .client
            .estimate(&endpoint, &Value::Object(args.params))
            .await
            .map_err(err)?;
        to_json(json!({
            "endpoint": endpoint,
            "usd": est.usd,
            "credits": est.credits,
            "server_max_usd_per_request": self.guard.max_usd_per_request,
        }))
    }

    #[tool(
        description = "SPENDS REAL MONEY. Submit a generation to the Higgsfield API. Estimates the cost first and refuses if it exceeds max_usd or the server caps. Returns request_id and, if wait_seconds > 0 and it finished, the output URLs.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn generate(&self, Parameters(args): Parameters<GenerateArgs>) -> Result<String, String> {
        let endpoint = validate_endpoint(&args.endpoint).map_err(err)?;
        if !(args.max_usd.is_finite() && args.max_usd > 0.0) {
            return Err("max_usd must be a positive number".into());
        }
        let params = Value::Object(args.params);
        let estimate = match self.client.estimate(&endpoint, &params).await {
            Ok(e) => Some(e.usd_f64().map_err(err)?),
            Err(e) if self.allow_unestimated => {
                tracing::warn!("estimate failed, proceeding because HF_ALLOW_UNESTIMATED is set: {e:#}");
                None
            }
            Err(e) => {
                return Err(format!(
                    "could not estimate cost, so nothing was submitted: {e:#}. Fix the params (see get_model_docs) and retry."
                ));
            }
        };
        let reserved = estimate.unwrap_or(0.0);
        self.guard.reserve(reserved, args.max_usd).await.map_err(err)?;

        let key = match args.idempotency_key {
            Some(k) => {
                let k = k.trim().to_string();
                if k.is_empty() || k.len() > 255 || !k.chars().all(|c| c.is_ascii_graphic()) {
                    self.guard.release(reserved).await;
                    return Err("idempotency_key must be 1-255 visible ASCII characters".into());
                }
                k
            }
            None => uuid::Uuid::new_v4().to_string(),
        };

        let accepted = match self.client.submit(&endpoint, &params, &key).await {
            Ok(a) => a,
            Err(e) => {
                self.guard.release(reserved).await;
                return Err(format!(
                    "{e:#}\nIf this was a network error the request may still have been accepted; retry with idempotency_key {key:?} and identical params to avoid paying twice."
                ));
            }
        };
        let request_id = accepted
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("unexpected submit response: {accepted}"))?;

        let mut result = json!({
            "request_id": request_id,
            "idempotency_key": key,
            "estimated_usd": estimate,
            "status": accepted.get("status"),
        });
        if args.wait_seconds > 0 {
            let wait = Duration::from_secs(args.wait_seconds.min(300));
            match self.client.wait(&request_id, wait).await {
                Ok(status) => {
                    if let Some(s) = status.get("status").and_then(Value::as_str) {
                        if matches!(s, "failed" | "nsfw" | "canceled") {
                            self.guard.release(reserved).await;
                        }
                    }
                    result["result"] = summarize(status);
                }
                Err(e) => result["wait_error"] = json!(format!("{e:#}")),
            }
        } else {
            result["hint"] = json!("Call wait_for_request with this request_id to get the output.");
        }
        to_json(result)
    }

    #[tool(
        description = "Get the current status (and output URLs, if completed) of a Higgsfield request. Free.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn get_request_status(&self, Parameters(args): Parameters<RequestIdArgs>) -> Result<String, String> {
        let id = validate_request_id(&args.request_id).map_err(err)?;
        let status = self.client.status(id).await.map_err(err)?;
        to_json(summarize(status))
    }

    #[tool(
        description = "Wait for a Higgsfield request to finish (polling with backoff) and return its output URLs. Returns early with the latest status if the timeout passes; call again to keep waiting. Free.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn wait_for_request(&self, Parameters(args): Parameters<WaitArgs>) -> Result<String, String> {
        let id = validate_request_id(&args.request_id).map_err(err)?;
        let timeout = Duration::from_secs(args.timeout_seconds.unwrap_or(120).clamp(1, 600));
        let status = self.client.wait(id, timeout).await.map_err(err)?;
        to_json(summarize(status))
    }

    #[tool(
        description = "Cancel a Higgsfield request that is still queued (refunded). Requests already in progress cannot be canceled.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn cancel_request(&self, Parameters(args): Parameters<RequestIdArgs>) -> Result<String, String> {
        let id = validate_request_id(&args.request_id).map_err(err)?;
        self.client.cancel(id).await.map_err(err)?;
        to_json(json!({ "request_id": id, "canceled": true }))
    }

    #[tool(
        description = "Upload input media (image, audio or video) to Higgsfield storage and get a public_url to pass in generation params such as image_url. Provide source_url (public https) or base64_data. Free.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn upload_file(&self, Parameters(args): Parameters<UploadArgs>) -> Result<String, String> {
        let (bytes, detected) = match (args.source_url, args.base64_data) {
            (Some(url), None) => download_public(&url).await.map_err(err)?,
            (None, Some(data)) => decode_base64(&data).map_err(err)?,
            _ => return Err("provide exactly one of source_url or base64_data".into()),
        };
        let content_type = args
            .content_type
            .or(detected)
            .or_else(|| sniff_type(&bytes).map(str::to_owned))
            .map(|c| c.split(';').next().unwrap_or("").trim().to_lowercase())
            .ok_or("could not determine content_type; pass it explicitly")?;
        if !ALLOWED_UPLOAD_TYPES.contains(&content_type.as_str()) {
            return Err(format!(
                "unsupported content_type {content_type:?}; allowed: {}",
                ALLOWED_UPLOAD_TYPES.join(", ")
            ));
        }
        let size = bytes.len();
        let target = self.client.create_upload(&content_type).await.map_err(err)?;
        self.client.put_presigned(&target, bytes).await.map_err(err)?;
        to_json(json!({ "public_url": target.public_url, "content_type": content_type, "bytes": size }))
    }

    #[tool(
        description = "Show this server's spending safeguards and the estimated spend recorded today (UTC, since the server last restarted).",
        annotations(read_only_hint = true, open_world_hint = false)
    )]
    async fn get_spend_limits(&self) -> Result<String, String> {
        to_json(json!({
            "max_usd_per_request": self.guard.max_usd_per_request,
            "max_usd_per_day": self.guard.max_usd_per_day,
            "estimated_spend_today_usd": self.guard.spent_today().await,
            "allow_unestimated": self.allow_unestimated,
            "note": "Authoritative balance and usage: https://cloud.higgsfield.ai",
        }))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for HiggsfieldMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new("higgsfield-mcp", env!("CARGO_PKG_VERSION")).with_title("Higgsfield API"),
            )
            .with_instructions(INSTRUCTIONS)
    }
}

fn decode_base64(data: &str) -> Result<(Vec<u8>, Option<String>)> {
    let (mime, payload) = match data.strip_prefix("data:") {
        Some(rest) => {
            let (meta, payload) = rest.split_once(',').ok_or_else(|| anyhow!("malformed data URI"))?;
            (meta.split(';').next().map(str::to_owned).filter(|m| !m.is_empty()), payload)
        }
        None => (None, data),
    };
    let cleaned: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .context("base64_data is not valid base64")?;
    if bytes.len() > MAX_UPLOAD_BYTES {
        bail!("file exceeds {} MB", MAX_UPLOAD_BYTES / 1024 / 1024);
    }
    Ok((bytes, mime))
}

/// Downloads a public https URL, refusing private/internal addresses (SSRF guard).
async fn download_public(raw: &str) -> Result<(Vec<u8>, Option<String>)> {
    let url = url::Url::parse(raw.trim()).context("invalid source_url")?;
    if url.scheme() != "https" {
        bail!("source_url must be https");
    }
    let host = url.host_str().ok_or_else(|| anyhow!("source_url has no host"))?.to_string();
    let lower = host.to_lowercase();
    if lower == "localhost" || lower.ends_with(".internal") || lower.ends_with(".local") {
        bail!("source_url host is not public");
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs: Vec<_> = tokio::net::lookup_host((host.as_str(), port)).await?.collect();
    if addrs.is_empty() || addrs.iter().any(|a| !is_public_ip(a.ip())) {
        bail!("source_url resolves to a non-public address");
    }
    // Pin the vetted address so DNS can't change between check and fetch.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(&host, addrs[0])
        .timeout(Duration::from_secs(120))
        .build()?;
    let resp = client.get(url).send().await?;
    if resp.status().is_redirection() {
        bail!("source_url redirects; pass the final URL instead");
    }
    if !resp.status().is_success() {
        bail!("downloading source_url returned {}", resp.status());
    }
    if resp.content_length().is_some_and(|l| l as usize > MAX_UPLOAD_BYTES) {
        bail!("file exceeds {} MB", MAX_UPLOAD_BYTES / 1024 / 1024);
    }
    let ct = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .filter(|c| !c.starts_with("application/octet-stream"));
    let mut bytes = Vec::new();
    let mut stream = resp.bytes_stream();
    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        bytes.extend_from_slice(&chunk?);
        if bytes.len() > MAX_UPLOAD_BYTES {
            bail!("file exceeds {} MB", MAX_UPLOAD_BYTES / 1024 / 1024);
        }
    }
    Ok((bytes, ct))
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT
                || o[0] >= 224)
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public_ip(IpAddr::V4(v4));
            }
            let seg0 = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // unique local
                || (seg0 & 0xffc0) == 0xfe80) // link local
        }
    }
}

fn sniff_type(b: &[u8]) -> Option<&'static str> {
    match b {
        [0xFF, 0xD8, 0xFF, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'A', b'V', b'E', ..] => Some("audio/wav"),
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => Some("video/mp4"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn spend_guard_enforces_caps() {
        let g = SpendGuard::new(2.0, Some(3.0));
        assert!(g.reserve(1.5, 1.0).await.is_err(), "over caller max");
        assert!(g.reserve(2.5, 10.0).await.is_err(), "over per-request cap");
        assert!(g.reserve(1.5, 2.0).await.is_ok());
        assert!(g.reserve(1.5, 2.0).await.is_ok());
        assert!(g.reserve(0.5, 2.0).await.is_err(), "over daily cap");
        g.release(1.5).await;
        assert!(g.reserve(0.5, 2.0).await.is_ok());
    }

    #[test]
    fn collects_output_urls() {
        let s = json!({
            "status": "completed",
            "status_url": "https://api.higgsfield.ai/requests/x/status",
            "images": [{"url": "https://cdn/a.jpg"}, {"url": "https://cdn/b.jpg"}],
            "video": {"url": "https://cdn/v.mp4"}
        });
        let mut urls = collect_outputs(&s);
        urls.sort();
        assert_eq!(urls, vec!["https://cdn/a.jpg", "https://cdn/b.jpg", "https://cdn/v.mp4"]);
    }

    #[test]
    fn blocks_private_ips() {
        for ip in ["127.0.0.1", "10.0.0.1", "192.168.1.1", "169.254.169.254", "100.64.0.1", "::1", "fd00::1", "::ffff:10.0.0.1"] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn decodes_data_uri() {
        let (b, m) = decode_base64("data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(m.as_deref(), Some("image/png"));
        assert_eq!(sniff_type(&b), Some("image/png"));
    }
}
