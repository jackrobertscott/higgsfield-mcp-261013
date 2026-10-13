//! Model catalog built from the live Higgsfield docs, so new models appear without a redeploy.

use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Serialize;
use tokio::sync::Mutex;

const DOCS_BASE: &str = "https://docs.higgsfield.ai";
const CATEGORY_PAGES: [(&str, &str); 2] = [
    ("image", "/docs/models/image-generation.md"),
    ("video", "/docs/models/video-generation.md"),
];
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Serialize)]
pub struct Workflow {
    pub name: String,
    /// Pass to `get_model_docs`.
    pub doc_path: String,
    /// Pass to `estimate_cost` / `generate`.
    pub endpoint: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelFamily {
    pub category: String,
    pub name: String,
    pub provider: String,
    pub description: String,
    pub doc_path: String,
    pub workflows: Vec<Workflow>,
}

#[derive(Clone, Default)]
pub struct Catalog {
    http: reqwest::Client,
    cache: Arc<Mutex<Option<(Instant, Vec<ModelFamily>)>>>,
}

static CARD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)featured-model-card"\s+href="(/docs/models/[^"]+)".*?featured-model-meta"><span>([^<]*)</span>.*?featured-model-title">([^<]*)</span>.*?featured-model-description">([^<]*)</span>"#,
    )
    .unwrap()
});
static WORKFLOW_ROW_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^\|\s*\[([^\]]+)\]\((/docs/models/[^)\s]+)\)\s*\|\s*`POST\s+(/[^`\s]+)`").unwrap()
});
static DOC_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9._-]*(/[a-z0-9][a-z0-9._-]*)*$").unwrap());

impl Catalog {
    async fn fetch(&self, path: &str) -> Result<String> {
        let url = format!("{DOCS_BASE}{path}");
        let resp = self.http.get(&url).timeout(Duration::from_secs(20)).send().await?;
        if !resp.status().is_success() {
            bail!("{url} returned {}", resp.status());
        }
        Ok(resp.text().await?)
    }

    pub async fn families(&self, refresh: bool) -> Result<Vec<ModelFamily>> {
        let mut cache = self.cache.lock().await;
        if !refresh {
            if let Some((at, families)) = cache.as_ref() {
                if at.elapsed() < CACHE_TTL {
                    return Ok(families.clone());
                }
            }
        }
        let families = self.load().await?;
        *cache = Some((Instant::now(), families.clone()));
        Ok(families)
    }

    async fn load(&self) -> Result<Vec<ModelFamily>> {
        let mut families = Vec::new();
        for (category, page) in CATEGORY_PAGES {
            let body = self.fetch(page).await.context("loading model catalog")?;
            for cap in CARD_RE.captures_iter(&body) {
                families.push(ModelFamily {
                    category: category.to_string(),
                    doc_path: cap[1].trim_start_matches("/docs/models/").to_string(),
                    provider: html_unescape(&cap[2]),
                    name: html_unescape(&cap[3]),
                    description: html_unescape(&cap[4]),
                    workflows: Vec::new(),
                });
            }
        }
        let pages = futures::future::join_all(
            families
                .iter()
                .map(|f| self.fetch_owned(format!("/docs/models/{}.md", f.doc_path))),
        )
        .await;
        for (family, page) in families.iter_mut().zip(pages) {
            match page {
                Ok(body) => {
                    family.workflows = WORKFLOW_ROW_RE
                        .captures_iter(&body)
                        .map(|c| Workflow {
                            name: c[1].to_string(),
                            doc_path: c[2].trim_start_matches("/docs/models/").to_string(),
                            endpoint: c[3].to_string(),
                        })
                        .collect();
                }
                Err(e) => tracing::warn!("failed to load {}: {e:#}", family.doc_path),
            }
        }
        if families.is_empty() {
            bail!("model catalog parsed as empty; the docs layout may have changed");
        }
        Ok(families)
    }

    async fn fetch_owned(&self, path: String) -> Result<String> {
        self.fetch(&path).await
    }

    /// Returns the condensed API reference for a workflow (or family) doc path.
    pub async fn model_docs(&self, doc_path: &str) -> Result<String> {
        let doc_path = doc_path
            .trim()
            .trim_start_matches(DOCS_BASE)
            .trim_start_matches('/')
            .trim_start_matches("docs/models/")
            .trim_end_matches(".md");
        if !DOC_PATH_RE.is_match(doc_path) || doc_path.contains("..") {
            bail!("invalid doc_path {doc_path:?}; use a doc_path from list_models, e.g. \"kling-3/turbo-image-to-video\"");
        }
        let body = self.fetch(&format!("/docs/models/{doc_path}.md")).await?;
        Ok(condense_doc(&body))
    }
}

/// Strips docs boilerplate and SDK code samples to save tokens while keeping
/// the endpoint, usage notes, parameter list, JSON schema and output shape.
fn condense_doc(body: &str) -> String {
    let mut out = Vec::new();
    let mut skip_fence = false;
    let mut in_fence = false;
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("> ") && (t.contains("Documentation Index") || t.contains("llms.txt") || t.contains("discover all available pages")) {
            continue;
        }
        if t.starts_with("## Related topics") || t.starts_with("This documentation is built") {
            break;
        }
        if t.starts_with("```") {
            if in_fence {
                in_fence = false;
                if skip_fence {
                    skip_fence = false;
                    continue;
                }
            } else {
                in_fence = true;
                let lang = t.trim_start_matches('`');
                skip_fence = lang.starts_with("python") || lang.starts_with("typescript") || lang.starts_with("bash");
                if skip_fence {
                    continue;
                }
            }
        }
        if skip_fence {
            continue;
        }
        if t.starts_with('<') && !t.starts_with("<ParamField") && !t.starts_with("</ParamField") {
            continue; // JSX layout wrappers
        }
        out.push(line.replace(r#" theme={"theme":{"light":"github-light","dark":"github-dark"}}"#, ""));
    }
    let text = out.join("\n");
    let collapsed = Regex::new(r"\n{3,}").unwrap().replace_all(&text, "\n\n");
    collapsed.trim().to_string()
}

fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .trim()
        .to_string()
}

pub fn validate_endpoint(endpoint: &str) -> Result<String> {
    let e = endpoint.trim();
    let e = e.strip_prefix(super::higgsfield::API_BASE).unwrap_or(e);
    let e = e.trim_start_matches("POST ").trim();
    let e = if e.starts_with('/') { e.to_string() } else { format!("/{e}") };
    let path = e.trim_start_matches('/');
    let first = path.split('/').next().unwrap_or("");
    if !DOC_PATH_RE.is_match(path)
        || path.contains("..")
        || path.split('/').count() < 2
        || ["requests", "files", "estimate"].contains(&first)
    {
        bail!("invalid endpoint {endpoint:?}; use an `endpoint` from list_models, e.g. \"/higgsfield-ai/soul/v2/standard\"");
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_validation() {
        assert_eq!(validate_endpoint("/higgsfield-ai/soul/v2/standard").unwrap(), "/higgsfield-ai/soul/v2/standard");
        assert_eq!(validate_endpoint("kling-video/v3.0-turbo/image-to-video").unwrap(), "/kling-video/v3.0-turbo/image-to-video");
        assert_eq!(
            validate_endpoint("https://api.higgsfield.ai/kling-video/v3.0-turbo/image-to-video").unwrap(),
            "/kling-video/v3.0-turbo/image-to-video"
        );
        assert!(validate_endpoint("/requests/abc/cancel").is_err());
        assert!(validate_endpoint("/a/../files/x").is_err());
        assert!(validate_endpoint("https://evil.com/x/y").is_err());
        assert!(validate_endpoint("/single").is_err());
    }

    #[test]
    fn parses_workflow_rows() {
        let body = "| Workflow | Endpoint |\n| - | - |\n| [Text to image](/docs/models/soul-2/generate) | `POST /higgsfield-ai/soul/v2/standard` |\n";
        let caps: Vec<_> = WORKFLOW_ROW_RE.captures_iter(body).collect();
        assert_eq!(caps.len(), 1);
        assert_eq!(&caps[0][2], "/docs/models/soul-2/generate");
        assert_eq!(&caps[0][3], "/higgsfield-ai/soul/v2/standard");
    }
}
