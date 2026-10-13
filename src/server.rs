//! MCP tool surface for the Higgsfield API.
//!
//! The server holds no Higgsfield credentials of its own. Over HTTP every
//! request carries the caller's key; over stdio the key comes from the local
//! environment of the user's own machine.

use std::net::IpAddr;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Extensions, Implementation, ServerCapabilities, ServerConfig},
    schemars::{self, JsonSchema},
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::catalog::{Catalog, validate_endpoint};
use crate::higgsfield::{Credentials, HiggsfieldClient, is_terminal};

const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;

const INSTRUCTIONS: &str = "\
Higgsfield pay-as-you-go API: image and video generation billed to the caller's own Higgsfield API balance in real US dollars (not subscription credits).

Workflow:
1. list_models to find a model workflow and its `endpoint` and `doc_path`.
2. get_model_docs(doc_path) to read the exact input schema. Parameters differ per model; never guess them.
3. If the user supplies local or private media, upload_file first and use the returned public_url in params (image_url, video_url, ...).
4. estimate_cost(endpoint, params) returns the USD price without spending anything.
5. generate(endpoint, params) submits the job and is billed.
6. Generation is asynchronous. Use wait_for_request (or generate with wait_seconds) and return the output URLs. Outputs expire after ~7 days.
Failed, nsfw and canceled requests are refunded. Only queued requests can be canceled.";

const MISSING_CREDENTIALS: &str = "No Higgsfield API key was sent. Create a key at https://cloud.higgsfield.ai and configure your MCP client to send the header `Authorization: Key <KEY_ID>:<KEY_SECRET>` (see the project README).";

#[derive(Clone)]
pub struct HiggsfieldMcp {
    http: reqwest::Client,
    /// Only set in stdio mode, from the local environment.
    local_credentials: Option<Credentials>,
    catalog: Catalog,
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

impl HiggsfieldMcp {
    /// Builds a client for whoever is calling: their HTTP headers, or the local env in stdio mode.
    fn client_for(&self, ext: &Extensions) -> Result<HiggsfieldClient, String> {
        let creds = match ext.get::<http::request::Parts>() {
            Some(parts) => Credentials::from_headers(&parts.headers).map_err(err)?,
            None => self.local_credentials.clone(),
        };
        let creds = creds.ok_or_else(|| MISSING_CREDENTIALS.to_string())?;
        Ok(HiggsfieldClient::new(self.http.clone(), creds))
    }
}

#[tool_router]
impl HiggsfieldMcp {
    pub fn new(local_credentials: Option<Credentials>) -> Self {
        Self {
            http: crate::higgsfield::http_client(),
            local_credentials,
            catalog: Catalog::default(),
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
        description = "Estimate the USD and credit cost of a generation without running it. Free.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn estimate_cost(
        &self,
        ext: Extensions,
        Parameters(args): Parameters<EstimateArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&ext)?;
        let endpoint = validate_endpoint(&args.endpoint).map_err(err)?;
        let est = client
            .estimate(&endpoint, &Value::Object(args.params))
            .await
            .map_err(err)?;
        to_json(json!({ "endpoint": endpoint, "usd": est.usd, "credits": est.credits }))
    }

    #[tool(
        description = "Submit a generation to the Higgsfield API, billed to the caller's API balance. Returns request_id and, if wait_seconds > 0 and it finished, the output URLs.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn generate(
        &self,
        ext: Extensions,
        Parameters(args): Parameters<GenerateArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&ext)?;
        let endpoint = validate_endpoint(&args.endpoint).map_err(err)?;
        let params = Value::Object(args.params);
        let key = match args.idempotency_key {
            Some(k) => {
                let k = k.trim().to_string();
                if k.is_empty() || k.len() > 255 || !k.chars().all(|c| c.is_ascii_graphic()) {
                    return Err("idempotency_key must be 1-255 visible ASCII characters".into());
                }
                k
            }
            None => uuid::Uuid::new_v4().to_string(),
        };

        let accepted = client.submit(&endpoint, &params, &key).await.map_err(|e| {
            format!(
                "{e:#}\nIf this was a network error the request may still have been accepted; retry with idempotency_key {key:?} and identical params to avoid paying twice."
            )
        })?;
        let request_id = accepted
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("unexpected submit response: {accepted}"))?;

        let mut result = json!({
            "request_id": request_id,
            "idempotency_key": key,
            "status": accepted.get("status"),
        });
        if args.wait_seconds > 0 {
            let wait = Duration::from_secs(args.wait_seconds.min(300));
            match client.wait(&request_id, wait).await {
                Ok(status) => result["result"] = summarize(status),
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
    async fn get_request_status(
        &self,
        ext: Extensions,
        Parameters(args): Parameters<RequestIdArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&ext)?;
        let id = validate_request_id(&args.request_id).map_err(err)?;
        let status = client.status(id).await.map_err(err)?;
        to_json(summarize(status))
    }

    #[tool(
        description = "Wait for a Higgsfield request to finish (polling with backoff) and return its output URLs. Returns early with the latest status if the timeout passes; call again to keep waiting. Free.",
        annotations(read_only_hint = true, open_world_hint = true)
    )]
    async fn wait_for_request(
        &self,
        ext: Extensions,
        Parameters(args): Parameters<WaitArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&ext)?;
        let id = validate_request_id(&args.request_id).map_err(err)?;
        let timeout = Duration::from_secs(args.timeout_seconds.unwrap_or(120).clamp(1, 600));
        let status = client.wait(id, timeout).await.map_err(err)?;
        to_json(summarize(status))
    }

    #[tool(
        description = "Cancel a Higgsfield request that is still queued (refunded). Requests already in progress cannot be canceled.",
        annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = true)
    )]
    async fn cancel_request(
        &self,
        ext: Extensions,
        Parameters(args): Parameters<RequestIdArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&ext)?;
        let id = validate_request_id(&args.request_id).map_err(err)?;
        client.cancel(id).await.map_err(err)?;
        to_json(json!({ "request_id": id, "canceled": true }))
    }

    #[tool(
        description = "Upload input media (image, audio or video) to Higgsfield storage and get a public_url to pass in generation params such as image_url. Provide source_url (public https) or base64_data. Free.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn upload_file(
        &self,
        ext: Extensions,
        Parameters(args): Parameters<UploadArgs>,
    ) -> Result<String, String> {
        let client = self.client_for(&ext)?;
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
        let target = client.create_upload(&content_type).await.map_err(err)?;
        client.put_presigned(&target, bytes).await.map_err(err)?;
        to_json(json!({ "public_url": target.public_url, "content_type": content_type, "bytes": size }))
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
