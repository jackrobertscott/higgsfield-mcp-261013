//! MCP tool surface for the Higgsfield API.
//!
//! Runs locally over stdio. The Higgsfield key comes from the environment of the
//! user's own machine and is only ever sent to api.higgsfield.ai.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

use crate::catalog::{Catalog, validate_endpoint};
use crate::higgsfield::{Credentials, HiggsfieldClient, is_terminal};

const MAX_UPLOAD_BYTES: usize = 100 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 2 * 1024 * 1024 * 1024;

const INSTRUCTIONS: &str = "\
Higgsfield pay-as-you-go API: image and video generation billed to the user's own Higgsfield API balance in real US dollars (not subscription credits).

Workflow:
1. list_models to find a model workflow and its `endpoint` and `doc_path`.
2. get_model_docs(doc_path) to read the exact input schema. Parameters differ per model; never guess them.
3. If the user supplies local or private media, upload_file first (file_path works for files on this computer) and use the returned public_url in params (image_url, video_url, ...).
4. estimate_cost(endpoint, params) returns the USD price without spending anything.
5. generate(endpoint, params) submits the job and is billed.
6. Generation is asynchronous. Use wait_for_request (or generate with wait_seconds) and return the output URLs. Outputs expire after ~7 days, so use download_output to save them to this computer.
Failed, nsfw and canceled requests are refunded. Only queued requests can be canceled.";

const MISSING_CREDENTIALS: &str = "No Higgsfield API key is configured. Create a key at https://cloud.higgsfield.ai, set HF_API_KEY_ID and HF_API_KEY_SECRET in the environment this MCP server is launched with, and restart it (see the project README).";

#[derive(Clone)]
pub struct HiggsfieldMcp {
    http: reqwest::Client,
    /// `None` when no key is configured; only the free catalog tools work then.
    client: Option<HiggsfieldClient>,
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
    /// Path to a file on this computer. `~` expands to the home directory.
    pub file_path: Option<String>,
    /// Public https URL to download and re-upload to Higgsfield storage.
    pub source_url: Option<String>,
    /// Base64-encoded file contents (alternative to source_url). A data: URI prefix is accepted.
    pub base64_data: Option<String>,
    /// MIME type: image/jpeg, image/png, image/webp, image/gif, audio/wav, audio/x-wav or video/mp4. Inferred when omitted.
    pub content_type: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DownloadArgs {
    /// Output URL from get_request_status or wait_for_request.
    pub url: String,
    /// Directory to save into. Defaults to HF_OUTPUT_DIR, or ~/Downloads/higgsfield.
    pub dir: Option<String>,
    /// File name to save as. Defaults to the last segment of the URL; never overwrites an existing file.
    pub filename: Option<String>,
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
    fn client(&self) -> Result<&HiggsfieldClient, String> {
        self.client.as_ref().ok_or_else(|| MISSING_CREDENTIALS.to_string())
    }
}

#[tool_router]
impl HiggsfieldMcp {
    pub fn new(credentials: Option<Credentials>) -> Self {
        let http = crate::higgsfield::http_client();
        Self {
            client: credentials.map(|c| HiggsfieldClient::new(http.clone(), c)),
            http,
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
        Parameters(args): Parameters<EstimateArgs>,
    ) -> Result<String, String> {
        let client = self.client()?;
        let endpoint = validate_endpoint(&args.endpoint).map_err(err)?;
        let est = client
            .estimate(&endpoint, &Value::Object(args.params))
            .await
            .map_err(err)?;
        to_json(json!({ "endpoint": endpoint, "usd": est.usd, "credits": est.credits }))
    }

    #[tool(
        description = "Submit a generation to the Higgsfield API, billed to the user's API balance. Returns request_id and, if wait_seconds > 0 and it finished, the output URLs.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn generate(
        &self,
        Parameters(args): Parameters<GenerateArgs>,
    ) -> Result<String, String> {
        let client = self.client()?;
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
        Parameters(args): Parameters<RequestIdArgs>,
    ) -> Result<String, String> {
        let client = self.client()?;
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
        Parameters(args): Parameters<WaitArgs>,
    ) -> Result<String, String> {
        let client = self.client()?;
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
        Parameters(args): Parameters<RequestIdArgs>,
    ) -> Result<String, String> {
        let client = self.client()?;
        let id = validate_request_id(&args.request_id).map_err(err)?;
        client.cancel(id).await.map_err(err)?;
        to_json(json!({ "request_id": id, "canceled": true }))
    }

    #[tool(
        description = "Upload input media (image, audio or video) to Higgsfield storage and get a public_url to pass in generation params such as image_url. Provide file_path (a file on this computer), source_url (public https) or base64_data. Free.",
        annotations(read_only_hint = false, destructive_hint = false, open_world_hint = true)
    )]
    async fn upload_file(
        &self,
        Parameters(args): Parameters<UploadArgs>,
    ) -> Result<String, String> {
        let client = self.client()?;
        let (bytes, detected) = match (args.file_path, args.source_url, args.base64_data) {
            (Some(path), None, None) => read_local(&path).await.map_err(err)?,
            (None, Some(url), None) => download_public(&url).await.map_err(err)?,
            (None, None, Some(data)) => decode_base64(&data).map_err(err)?,
            _ => return Err("provide exactly one of file_path, source_url or base64_data".into()),
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

    #[tool(
        description = "Save a generated output (image, video or audio URL) to a file on this computer and return its local path. Higgsfield output URLs expire after ~7 days. Free.",
        annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn download_output(&self, Parameters(args): Parameters<DownloadArgs>) -> Result<String, String> {
        let dir = match args.dir {
            Some(d) => expand_home(&d),
            None => default_output_dir().map_err(err)?,
        };
        let (path, bytes) = download_to(&self.http, &args.url, &dir, args.filename.as_deref())
            .await
            .map_err(err)?;
        to_json(json!({ "path": path, "bytes": bytes }))
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

fn expand_home(path: &str) -> PathBuf {
    let path = path.trim();
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            PathBuf::from(home).join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

fn default_output_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var("HF_OUTPUT_DIR").ok().filter(|d| !d.trim().is_empty()) {
        return Ok(expand_home(&dir));
    }
    let home = std::env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set; pass dir explicitly"))?;
    Ok(PathBuf::from(home).join("Downloads").join("higgsfield"))
}

async fn read_local(path: &str) -> Result<(Vec<u8>, Option<String>)> {
    let path = expand_home(path);
    let meta = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    if !meta.is_file() {
        bail!("{} is not a file", path.display());
    }
    if meta.len() as usize > MAX_UPLOAD_BYTES {
        bail!("file exceeds {} MB", MAX_UPLOAD_BYTES / 1024 / 1024);
    }
    let bytes = tokio::fs::read(&path)
        .await
        .with_context(|| format!("cannot read {}", path.display()))?;
    Ok((bytes, None))
}

/// Keeps only a safe file name: no directories, no leading dots.
fn sanitize_filename(name: &str) -> Option<String> {
    let name = name.rsplit(['/', '\\']).next()?.trim();
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').to_string();
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Picks `name`, or `stem-1.ext`, `stem-2.ext`, ... so existing files are never overwritten.
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|i| dir.join(format!("{stem}-{i}{ext}")))
        .find(|p| !p.exists())
        .expect("unbounded search finds a free name")
}

async fn download_to(http: &reqwest::Client, raw: &str, dir: &Path, filename: Option<&str>) -> Result<(PathBuf, u64)> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let url = url::Url::parse(raw.trim()).context("invalid url")?;
    if url.scheme() != "https" {
        bail!("url must be https");
    }
    let name = filename
        .or_else(|| url.path_segments().and_then(|mut s| s.next_back()))
        .and_then(sanitize_filename)
        .unwrap_or_else(|| format!("higgsfield-{}", uuid::Uuid::new_v4()));

    // The shared client has a 60s total timeout, too short for large videos.
    let resp = http
        .get(url)
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .context("download failed")?;
    if !resp.status().is_success() {
        bail!("download returned {}", resp.status());
    }
    if resp.content_length().is_some_and(|l| l > MAX_DOWNLOAD_BYTES) {
        bail!("file exceeds {} GB", MAX_DOWNLOAD_BYTES / 1024 / 1024 / 1024);
    }

    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("cannot create {}", dir.display()))?;
    let path = unique_path(dir, &name);
    let tmp = path.with_file_name(format!(".{name}.{}.part", uuid::Uuid::new_v4()));
    let result = async {
        let mut file = tokio::fs::File::create_new(&tmp).await?;
        let mut written = 0u64;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            written += chunk.len() as u64;
            if written > MAX_DOWNLOAD_BYTES {
                bail!("file exceeds {} GB", MAX_DOWNLOAD_BYTES / 1024 / 1024 / 1024);
            }
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        Ok(written)
    }
    .await;
    match result {
        Ok(written) => {
            tokio::fs::rename(&tmp, &path).await?;
            Ok((path, written))
        }
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            Err(e)
        }
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
    fn sanitizes_filenames() {
        assert_eq!(sanitize_filename("clip.mp4").as_deref(), Some("clip.mp4"));
        assert_eq!(sanitize_filename("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(sanitize_filename("..").as_deref(), None);
        assert_eq!(sanitize_filename("a b?.png").as_deref(), Some("a_b_.png"));
    }

    #[test]
    fn never_overwrites() {
        let dir = std::env::temp_dir().join(format!("hf-mcp-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), b"x").unwrap();
        std::fs::write(dir.join("a-1.png"), b"x").unwrap();
        assert_eq!(unique_path(&dir, "a.png"), dir.join("a-2.png"));
        assert_eq!(unique_path(&dir, "b.png"), dir.join("b.png"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn decodes_data_uri() {
        let (b, m) = decode_base64("data:image/png;base64,iVBORw0KGgo=").unwrap();
        assert_eq!(m.as_deref(), Some("image/png"));
        assert_eq!(sniff_type(&b), Some("image/png"));
    }
}
