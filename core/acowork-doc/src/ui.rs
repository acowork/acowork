//! ADR-116: DOC serves its own app bundle through the Gateway app proxy.
//!
//! Mirrors `acowork-pm::ui`: `/ui/` is DOC-owned HTML reached only through
//! `/api/apps/doc/ui/`, and `/app-context` echoes the Gateway-injected
//! identity while proxying one narrow App → Platform read.

use std::time::Duration;

use axum::{Json, Router, http::HeaderMap, response::Html, routing::get};
use serde_json::{Value, json};

const APP_ID: &str = "doc";
const APP_NAME: &str = "文档库";
const SUMMARY_PATH: &str = "tree";
const TOKEN_ENV: &str = "ACOWORK_DOC_GATEWAY_TOKEN";

pub fn ui_router() -> Router {
    Router::new()
        .route("/ui/", get(ui_index))
        .route("/ui", get(ui_index))
        .route("/app-context", get(app_context))
}

async fn ui_index() -> Html<String> {
    Html(acowork_core::app::app_shell_html(
        APP_ID,
        APP_NAME,
        SUMMARY_PATH,
    ))
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
}

async fn app_context(headers: HeaderMap) -> Json<Value> {
    let platform = match platform_context(&headers).await {
        Ok(value) => value,
        Err(error) => json!({ "error": error }),
    };
    Json(json!({
        "app_id": header(&headers, "x-app-id"),
        "app_user": header(&headers, "x-app-user"),
        "actor": header(&headers, "x-actor"),
        "admin": header(&headers, "x-actor-admin"),
        "session_verified": header(&headers, "x-app-session-verified"),
        "platform_url": header(&headers, "x-app-platform-url"),
        "platform": platform,
    }))
}

async fn platform_context(headers: &HeaderMap) -> Result<Value, String> {
    let base = header(headers, "x-app-platform-url");
    if base.is_empty() {
        return Err("Gateway did not inject x-app-platform-url".to_string());
    }
    let app_id = header(headers, "x-app-id");
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .map_err(|e| e.to_string())?;
    let mut request = client
        .get(format!("{base}/api/platform/context"))
        .header("X-App-Id", app_id);
    if !token.is_empty() {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("platform returned {status}: {}", &text[..text.len().min(200)]));
    }
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "raw": text })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_is_bound_to_doc_channels() {
        let html = acowork_core::app::app_shell_html(APP_ID, APP_NAME, SUMMARY_PATH);
        assert!(html.contains("/api/apps/doc/api/"));
        assert!(html.contains("'tree'"));
    }
}
