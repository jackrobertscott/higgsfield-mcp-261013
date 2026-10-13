//! Thin client for the pay-as-you-go Higgsfield API (https://api.higgsfield.ai).

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rand::Rng;
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use serde_json::Value;

pub const API_BASE: &str = "https://api.higgsfield.ai";

pub const TERMINAL_STATUSES: [&str; 4] = ["completed", "failed", "nsfw", "canceled"];

#[derive(Clone)]
pub struct HiggsfieldClient {
    http: reqwest::Client,
    auth_header: String,
}

#[derive(Debug, Deserialize)]
pub struct Estimate {
    pub credits: Option<Value>,
    pub usd: Value,
}

impl Estimate {
    pub fn usd_f64(&self) -> Result<f64> {
        match &self.usd {
            Value::String(s) => s.parse().with_context(|| format!("bad usd value {s:?}")),
            Value::Number(n) => n.as_f64().ok_or_else(|| anyhow!("bad usd value {n}")),
            other => bail!("unexpected usd value {other}"),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct UploadTarget {
    pub public_url: String,
    pub upload_url: String,
    #[serde(default)]
    pub upload_headers: serde_json::Map<String, Value>,
}

impl HiggsfieldClient {
    pub fn new(key_id: &str, key_secret: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("higgsfield-mcp/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            auth_header: format!("Key {key_id}:{key_secret}"),
        })
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        idempotency_key: Option<&str>,
    ) -> Result<(StatusCode, Value)> {
        let url = format!("{API_BASE}{path}");
        let mut req = self
            .http
            .request(method, &url)
            .header("Authorization", &self.auth_header);
        if let Some(body) = body {
            req = req.json(body);
        }
        if let Some(key) = idempotency_key {
            req = req.header("Idempotency-Key", key);
        }
        let resp = req.send().await.with_context(|| format!("request to {url} failed"))?;
        let status = resp.status();
        let correlation = resp
            .headers()
            .get("x-correlation-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let text = resp.text().await.unwrap_or_default();
        let json: Value = if text.trim().is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::String(text))
        };
        if !status.is_success() {
            let detail = json.get("detail").cloned().unwrap_or(json);
            bail!(
                "Higgsfield API returned {status} ({}) for {path}: {detail}{}",
                explain_status(status),
                correlation
                    .map(|c| format!(" [X-Correlation-ID: {c}]"))
                    .unwrap_or_default()
            );
        }
        Ok((status, json))
    }

    /// `endpoint` is a model endpoint path such as `/higgsfield-ai/soul/v2/standard`.
    pub async fn estimate(&self, endpoint: &str, params: &Value) -> Result<Estimate> {
        let (_, json) = self
            .send(Method::POST, &format!("/estimate{endpoint}"), Some(params), None)
            .await?;
        serde_json::from_value(json.clone())
            .with_context(|| format!("unexpected estimate response: {json}"))
    }

    pub async fn submit(&self, endpoint: &str, params: &Value, idempotency_key: &str) -> Result<Value> {
        // Submissions are idempotent with the same key, so retrying ambiguous failures is safe.
        let mut delay = Duration::from_millis(500);
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self
                .send(Method::POST, endpoint, Some(params), Some(idempotency_key))
                .await
            {
                Ok((_, json)) => return Ok(json),
                Err(e) if attempt < 3 && is_retryable(&e) => {
                    tracing::warn!("submit attempt {attempt} failed, retrying: {e:#}");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub async fn status(&self, request_id: &str) -> Result<Value> {
        let (_, json) = self
            .send(Method::GET, &format!("/requests/{request_id}/status"), None, None)
            .await?;
        Ok(json)
    }

    pub async fn cancel(&self, request_id: &str) -> Result<()> {
        self.send(Method::POST, &format!("/requests/{request_id}/cancel"), None, None)
            .await?;
        Ok(())
    }

    /// Polls with the backoff recommended by the docs (2s growing to 10s, with jitter).
    /// Returns the last status seen, which is non-terminal if `timeout` elapsed first.
    pub async fn wait(&self, request_id: &str, timeout: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut delay = 2.0_f64;
        loop {
            let status = match self.status(request_id).await {
                Ok(s) => s,
                Err(e) if is_retryable(&e) && tokio::time::Instant::now() < deadline => {
                    tracing::warn!("status poll failed, retrying: {e:#}");
                    Value::Null
                }
                Err(e) => return Err(e),
            };
            if is_terminal(&status) {
                return Ok(status);
            }
            let jitter = rand::rng().random_range(0.0..0.5);
            let sleep = Duration::from_secs_f64(delay + jitter);
            if tokio::time::Instant::now() + sleep >= deadline {
                return if status.is_null() { self.status(request_id).await } else { Ok(status) };
            }
            tokio::time::sleep(sleep).await;
            delay = (delay * 1.5).min(10.0);
        }
    }

    pub async fn create_upload(&self, content_type: &str) -> Result<UploadTarget> {
        let body = serde_json::json!({ "content_type": content_type });
        let (_, json) = self
            .send(Method::POST, "/files/generate-upload-url", Some(&body), None)
            .await?;
        serde_json::from_value(json.clone())
            .with_context(|| format!("unexpected upload-url response: {json}"))
    }

    /// PUTs bytes to a presigned storage URL. Never sends Higgsfield credentials.
    pub async fn put_presigned(&self, target: &UploadTarget, bytes: Vec<u8>) -> Result<()> {
        let url = url::Url::parse(&target.upload_url)?;
        if url.scheme() != "https" {
            bail!("refusing non-https upload URL");
        }
        let mut req = reqwest::Client::new().put(url).body(bytes);
        for (k, v) in &target.upload_headers {
            if let Some(v) = v.as_str() {
                req = req.header(k, v);
            }
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!("upload to storage failed with {status}: {text}");
        }
        Ok(())
    }
}

pub fn is_terminal(status: &Value) -> bool {
    status
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| TERMINAL_STATUSES.contains(&s))
}

fn is_retryable(e: &anyhow::Error) -> bool {
    if e.downcast_ref::<reqwest::Error>().is_some() {
        return true;
    }
    let msg = e.to_string();
    ["returned 500", "returned 502", "returned 504"]
        .iter()
        .any(|p| msg.contains(p))
}

fn explain_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        400 => "invalid parameters, rejected input, or concurrency limit reached",
        401 => "missing or invalid API credentials",
        403 => "insufficient API balance - top up at https://cloud.higgsfield.ai",
        404 => "request or model not found for this account",
        422 => "validation failed or idempotency key reused with different parameters",
        423 => "model temporarily blocked, try later",
        429 => "rate limited, try later",
        503 => "model disabled or not ready, try later",
        s if s >= 500 => "Higgsfield server error",
        _ => "error",
    }
}
