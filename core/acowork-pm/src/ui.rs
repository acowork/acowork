//! ADR-116: PM serves its own app bundle through the Gateway app proxy.
//!
//! `/ui/` is the app's front end (owned by PM, served by PM's HTTP service,
//! reached only through `/api/apps/pm/ui/`). `/app-context` echoes the
//! trusted identity headers the Gateway injected and proxies one narrow
//! App → Platform read, which is how the framework's identity and platform
//! API channels are verified end to end.

use std::time::Duration;

use axum::{
    Json, Router,
    http::HeaderMap,
    response::Html,
    routing::get,
};
use serde_json::{Value, json};

const APP_ID: &str = "pm";
const APP_NAME: &str = "项目管理";
const SUMMARY_PATH: &str = "projects";
const TOKEN_ENV: &str = "ACOWORK_PM_GATEWAY_TOKEN";

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

/// Read `/api/platform/context` with the internal service credential the PM
/// supervisor injected. The webview never sees this token.
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
    fn shell_is_self_contained_and_bound_to_this_app() {
        let html = acowork_core::app::app_shell_html(APP_ID, APP_NAME, SUMMARY_PATH);
        assert!(html.contains("/api/apps/pm/api/"));
        assert!(html.contains("X-App-Session"));
        assert!(html.contains("acowork"));
        assert!(!html.contains("<script src="), "no external sub-resources");
    }
}
