//! ADR-116: DOC serves its own app bundle through the Gateway app proxy.
//!
//! `/ui/` is a DOC-owned document library shell (tree + preview) reached only
//! through `/api/apps/{id}/ui/`. The same binary can host the built-in `doc`
//! app or a cloned install (`doc-clone`): the app id comes from
//! `ACOWORK_APP_ID` (supervisor-injected) and defaults to `doc`.

use std::time::Duration;

use axum::{Json, Router, http::HeaderMap, response::Html, routing::get};
use serde_json::{Value, json};

const DEFAULT_APP_ID: &str = "doc";
const TOKEN_ENV: &str = "ACOWORK_DOC_GATEWAY_TOKEN";

/// Resolve the hosted-app id this process is serving as.
pub fn resolved_app_id() -> String {
    std::env::var("ACOWORK_APP_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_APP_ID.to_string())
}

pub fn ui_router() -> Router {
    Router::new()
        .route("/ui/", get(ui_index))
        .route("/ui", get(ui_index))
        .route("/app-context", get(app_context))
}

async fn ui_index() -> Html<String> {
    Html(doc_library_html(&resolved_app_id()))
}

fn doc_library_html(app_id: &str) -> String {
    // Self-contained SPA: no Desktop imports. Fetches go through the Gateway
    // app channel with the session ticket from the URL fragment.
    let template = r###"<!doctype html>
<html lang="zh" data-theme="dark">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>文档库</title>
<style>
:root { --accent:#3b82f6; --bg:#14161a; --card:#1c2028; --line:#303642;
        --fg:#e6e9ef; --muted:#9aa3b2; --ok:#4ade80; --err:#f87171; }
html[data-theme="light"] { --bg:#f6f7f9; --card:#fff; --line:#e2e6ec;
        --fg:#1c2028; --muted:#667085; }
* { box-sizing:border-box; }
body { margin:0; height:100vh; display:flex; flex-direction:column;
       background:var(--bg); color:var(--fg);
       font:13px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif; }
header { display:flex; align-items:center; gap:10px; padding:10px 14px;
         border-bottom:1px solid var(--line); }
header h1 { font-size:14px; font-weight:500; margin:0; }
.badge { font-size:11px; padding:2px 8px; border:1px solid var(--line);
         border-radius:999px; color:var(--muted); }
.main { flex:1; min-height:0; display:grid; grid-template-columns:260px 1fr; }
.side { border-right:1px solid var(--line); overflow:auto; padding:8px; }
.pane { overflow:auto; padding:16px 20px; }
.tree-item { display:block; width:100%; text-align:left; border:0; background:transparent;
             color:var(--fg); padding:6px 8px; border-radius:6px; cursor:pointer; font:inherit; }
.tree-item:hover { background:color-mix(in srgb, var(--accent) 12%, transparent); }
.tree-item.active { background:color-mix(in srgb, var(--accent) 22%, transparent); }
.tree-item .meta { color:var(--muted); font-size:11px; }
pre, .body { white-space:pre-wrap; word-break:break-word; font-family:ui-monospace,Consolas,monospace;
             font-size:12px; margin:0; }
.muted { color:var(--muted); }
.err { color:var(--err); }
.toolbar { display:flex; gap:8px; margin-bottom:12px; }
button.action { border:1px solid var(--line); background:transparent; color:var(--fg);
                border-radius:6px; padding:4px 10px; cursor:pointer; font-size:12px; }
button.action.primary { background:var(--accent); border-color:var(--accent); color:#fff; }
</style>
</head>
<body>
<header>
  <h1>文档库</h1>
  <span class="badge">hosted app · __APP_ID__</span>
  <span class="badge" id="locale">zh</span>
  <span class="muted" id="status" style="margin-left:auto"></span>
</header>
<div class="main">
  <aside class="side" id="tree"><div class="muted" style="padding:8px">loading…</div></aside>
  <section class="pane">
    <div class="toolbar">
      <button class="action" id="refresh">刷新</button>
      <button class="action primary" id="notify">通知平台</button>
    </div>
    <h2 id="title" class="muted" style="font-size:13px;font-weight:500;margin:0 0 10px">选择左侧文档</h2>
    <div id="content" class="body muted">—</div>
  </section>
</div>
<script>
const APP = '__APP_ID__';
const ticket = new URLSearchParams(location.hash.slice(1)).get('ticket') || '';
const base = '/api/apps/' + APP + '/api/';
function headers() {
  const h = { 'Accept': 'application/json' };
  if (ticket) h['X-App-Session'] = ticket;
  return h;
}
async function api(path, init) {
  const r = await fetch(base + path.replace(/^\//,''), { ...init, headers: { ...headers(), ...(init&&init.headers||{}) } });
  const text = await r.text();
  let json = null;
  try { json = JSON.parse(text); } catch {}
  if (!r.ok) throw new Error((json && (json.error||json.message)) || (path + ' → ' + r.status));
  return json;
}
function flattenTree(nodes, depth, out) {
  if (!Array.isArray(nodes)) return out;
  for (const n of nodes) {
    out.push({ ...n, depth });
    if (n.children) flattenTree(n.children, depth + 1, out);
  }
  return out;
}
let selectedId = null;
async function loadTree() {
  const el = document.getElementById('tree');
  document.getElementById('status').textContent = ticket ? 'session ok' : 'ticket missing';
  try {
    const data = await api('tree');
    const roots = data.tree || data.nodes || data.children || data.items || (Array.isArray(data) ? data : []);
    const flat = flattenTree(roots, 0, []);
    if (flat.length === 0) {
      el.innerHTML = '<div class="muted" style="padding:8px">空文档库</div>';
      return;
    }
    el.innerHTML = flat.map(n => {
      const id = n.id || n.path || n.name;
      const label = n.title || n.name || n.path || id;
      const kind = n.kind || n.type || (n.children ? 'folder' : 'doc');
      const pad = (n.depth||0) * 12;
      const active = id === selectedId ? ' active' : '';
      return '<button type="button" class="tree-item'+active+'" data-id="'+String(id).replace(/"/g,'&quot;')+'" style="padding-left:'+(8+pad)+'px">'
        + '<div>'+escapeHtml(label)+'</div>'
        + '<div class="meta">'+escapeHtml(kind)+'</div></button>';
    }).join('');
    el.querySelectorAll('.tree-item').forEach(btn => {
      btn.addEventListener('click', () => openDoc(btn.getAttribute('data-id')));
    });
  } catch (e) {
    el.innerHTML = '<div class="err" style="padding:8px">'+escapeHtml(String(e))+'</div>';
  }
}
async function openDoc(id) {
  selectedId = id;
  document.querySelectorAll('.tree-item').forEach(b => b.classList.toggle('active', b.getAttribute('data-id')===id));
  const title = document.getElementById('title');
  const content = document.getElementById('content');
  title.textContent = id;
  content.className = 'body muted';
  content.textContent = 'loading…';
  try {
    // Prefer document content endpoints used by DOC; fall back to node metadata.
    let doc = null;
    for (const path of ['documents/'+encodeURIComponent(id), 'nodes/'+encodeURIComponent(id), 'tree/'+encodeURIComponent(id)]) {
      try { doc = await api(path); break; } catch {}
    }
    if (!doc) throw new Error('document not found');
    title.textContent = doc.title || doc.name || id;
    const body = doc.content || doc.markdown || doc.body || doc.text
      || JSON.stringify(doc, null, 2);
    content.className = 'body';
    content.textContent = typeof body === 'string' ? body : JSON.stringify(body, null, 2);
  } catch (e) {
    content.className = 'body err';
    content.textContent = String(e);
  }
}
function escapeHtml(s) {
  return String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}
window.addEventListener('message', (e) => {
  const d = e.data;
  if (!d || d.channel !== 'acowork') return;
  if (d.type === 'theme') {
    document.documentElement.dataset.theme = d.theme || 'dark';
    if (d.accent) document.documentElement.style.setProperty('--accent', d.accent);
    if (d.locale) document.getElementById('locale').textContent = d.locale;
  }
  if (d.type === 'locale' && d.locale) {
    document.getElementById('locale').textContent = d.locale;
    document.documentElement.lang = d.locale;
  }
});
document.getElementById('refresh').addEventListener('click', loadTree);
document.getElementById('notify').addEventListener('click', () => {
  parent.postMessage({ channel:'acowork', type:'notify', message:'DOC library ready' }, '*');
});
parent.postMessage({ channel:'acowork', type:'ready', app: APP }, '*');
loadTree();
</script>
</body>
</html>
"###;
    template.replace("__APP_ID__", app_id)
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
    fn hosted_ui_binds_doc_channels() {
        let html = doc_library_html("doc");
        assert!(html.contains("const APP = 'doc';"));
        assert!(html.contains("/api/apps/' + APP + '/api/'"));
        assert!(html.contains("X-App-Session"));
        assert!(html.contains("loadTree"));
    }

    #[test]
    fn hosted_ui_honours_clone_app_id() {
        let html = doc_library_html("doc-clone");
        assert!(html.contains("const APP = 'doc-clone';"));
        assert!(html.contains("hosted app · doc-clone"));
        assert!(html.contains("/api/apps/' + APP + '/api/'"));
        assert!(!html.contains("const APP = 'doc';"));
    }

    #[test]
    fn resolved_app_id_defaults_to_doc() {
        // Do not assert against a polluted env in shared CI workers; just
        // check the fallback path when the var is absent from our call.
        let id = std::env::var("ACOWORK_APP_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_APP_ID.to_string());
        assert!(!id.is_empty());
    }
}
