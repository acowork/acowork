//! Intent/Chat Session unification — process-level E2E (plan Wave 5/6 acceptance).
//!
//! Mirrors `user_proxy_e2e.rs`'s shape: the real `acowork-gateway` binary
//! (which supervises the real `acowork-user` service and the embedded MQTT
//! broker) driven over dependency-light HTTP, plus one raw MQTT subscriber
//! for the doorbell. The in-process router tests cover handler logic;
//! nothing until this file exercises the *chains* the Desktop walks:
//!
//! * default-pair resolution + idempotency (§5.4.4)
//! * human message: persist-before-deliver + mailbox status (§5.4.4, plan 2a)
//! * agent write-back via the system surface → unread + chat/changed doorbell (§4.6, Wave 5.4a)
//! * delete guards: default pair / project group / hidden-404 non-member (plan 5.2)
//! * attachments: upload → reference → download → cross-session refusal (§4.2.1)
//! * participant sync (§8.3) and reinstalled-agent history (§10.19)
//! * card answer for a nonexistent card fails loud (§5.5.4)
//! * local mode (the desktop default form) refuses chat writes and serves
//!   no session surface — the multi_user-only gate stays (ADR-076 §决策 12)
//!
//! multi_user mode with the bootstrap admin, because the Gateway's chat
//! write entry requires a token-derived identity (local mode keeps the
//! legacy ChatPanel surface by design).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

fn gateway_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let dir = exe.parent().and_then(|p| p.parent()).expect("target/debug dir");
    let name = if cfg!(windows) { "acowork-gateway.exe" } else { "acowork-gateway" };
    let candidate = dir.join(name);
    if candidate.exists() {
        candidate
    } else {
        dir.parent().expect("target root").join(name)
    }
}

struct Gateway {
    child: Child,
    #[allow(dead_code)]
    home: PathBuf,
    http_port: u16,
    mqtt_port: u16,
    stderr: Arc<Mutex<String>>,
}

impl Gateway {
    fn stderr(&self) -> String {
        self.stderr.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn wait_until_serving(&mut self) {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!("gateway exited before serving ({status}); stderr:\n{}", self.stderr());
            }
            if std::net::TcpStream::connect(("127.0.0.1", self.http_port)).is_ok() {
                return;
            }
            sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        panic!("gateway did not serve within {BOOT_TIMEOUT:?}; stderr:\n{}", self.stderr());
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
            .output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn temp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!(
        "acowork-test-chat-e2e-{tag}-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(home.join("config")).expect("create home");
    home
}

/// Boot a multi_user gateway with a bootstrap admin (root / s3cret123).
fn boot_multi_user(tag: &str) -> (Gateway, String /*token*/, String /*root uid*/) {
    let home = temp_home(tag);
    let (http, mqtt) = (free_port(), free_port());
    let home_s = home.display().to_string().replace('\\', "/");
    let config_path = home.join("config").join("gateway.toml");
    fs::write(
        &config_path,
        format!(
            r#"
vault_dir = "{home}/data/vault"
packages_dir = "{home}/config/packages"
data_dir = "{home}/data"

auth_mode = "multi_user"

[user]
config = "{home}/acowork-user.toml"
"#,
            home = home_s
        ),
    )
    .expect("write gateway.toml");
    fs::write(
        home.join("acowork-user.toml"),
        r#"
[bootstrap_admin]
username = "root"
password = "s3cret123"
display_name = "Root"
"#,
    )
    .expect("write acowork-user.toml");

    let mut cmd = Command::new(gateway_binary());
    cmd.arg("--daemon")
        .arg("--home")
        .arg(&home)
        .arg("--addr")
        .arg(format!("127.0.0.1:{http}"))
        .arg("--mqtt-addr")
        .arg(format!("127.0.0.1:{mqtt}"))
        .arg("--no-spawn-local-node")
        .arg("--config-path")
        .arg(&config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn acowork-gateway");
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(pipe) = child.stderr.take() {
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                if let Ok(line) = line
                    && let Ok(mut buf) = sink.lock()
                {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
        });
    }
    let mut gw = Gateway {
        child,
        home,
        http_port: http,
        mqtt_port: mqtt,
        stderr,
    };
    gw.wait_until_serving();

    // Wait until the user service can verify tokens: the gate flips from
    // 503 ("authentication unavailable") to 401 once the verifier loads.
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        let (code, _, _) = http_request(http, "GET", "/api/users", None, "application/json", &[("Authorization", "Bearer probe")]);
        if code != 503 {
            break;
        }
        assert!(Instant::now() < deadline, "user service never became ready; stderr:\n{}", gw.stderr());
        sleep(Duration::from_millis(100));
    }

    let body = serde_json::to_vec(&serde_json::json!({"username": "root", "password": "s3cret123"})).unwrap();
    let (code, body, _) = http_post_json(http, "/api/auth/login", &body);
    assert_eq!(code, 200, "admin login; body:\n{body}");
    let v = json_parse(&body);
    let token = v["access_token"].as_str().expect("access_token").to_string();
    // TokenPair carries no identity — resolve it through /api/auth/me.
    let (code, body, _) = http_get_auth(http, "/api/auth/me", &token);
    assert_eq!(code, 200, "me; body:\n{body}");
    let who = json_parse(&body)["user_id"].as_str().expect("user_id").to_string();
    (gw, token, who)
}

/// The test gateway's own user-service port, read from the port file the
/// supervisor reports (`{data_dir}/user.port`). Scanning loopback instead
/// would find *some* acowork-user — on a dev machine the production
/// instance holds 18083 and the test's supervisor auto-incremented away
/// from it; the port file is the only unambiguous handle.
fn user_service_port(home: &Path) -> u16 {
    let file = home.join("data").join("user.port");
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        if let Ok(raw) = fs::read_to_string(&file)
            && let Ok(port) = raw.trim().parse::<u16>()
        {
            return port;
        }
        assert!(Instant::now() < deadline, "no user.port file at {}", file.display());
        sleep(Duration::from_millis(100));
    }
}

// ── raw HTTP (no client dependency), mirroring user_proxy_e2e ──────────

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&[u8]>,
    content_type: &str,
    extra_headers: &[(&str, &str)],
) -> (u16, String, Vec<u8>) {
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("set read timeout");
    let len = body.map(|b| b.len()).unwrap_or(0);
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if body.is_some() {
        req.push_str(&format!("Content-Type: {content_type}\r\nContent-Length: {len}\r\n"));
    }
    for (k, v) in extra_headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).expect("write head");
    if let Some(b) = body {
        stream.write_all(b).expect("write body");
    }
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n");
    let (head_b, body_b) = match split {
        Some(i) => (&raw[..i], raw[i + 4..].to_vec()),
        None => (&raw[..], Vec::new()),
    };
    let head = String::from_utf8_lossy(head_b).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    // The proxy re-emits decoded bodies with a Content-Length, so the raw
    // bytes after the header block *are* the body; no chunk framing here.
    (status, head, body_b)
}

fn body_str(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn http_get_plain(port: u16, path: &str) -> (u16, String, Vec<u8>) {
    let (s, _, b) = http_request(port, "GET", path, None, "", &[]);
    (s, body_str(&b), b)
}

fn http_get_auth(port: u16, path: &str, token: &str) -> (u16, String, Vec<u8>) {
    let (s, _, b) = http_request(port, "GET", path, None, "", &[("Authorization", &format!("Bearer {token}"))]);
    (s, body_str(&b), b)
}

fn http_post_json(port: u16, path: &str, body: &[u8]) -> (u16, String, Vec<u8>) {
    let (s, _, b) = http_request(port, "POST", path, Some(body), "application/json", &[]);
    (s, body_str(&b), b)
}

fn http_post_json_auth(port: u16, path: &str, body: &[u8], token: &str) -> (u16, String, Vec<u8>) {
    let (s, _, b) = http_request(
        port,
        "POST",
        path,
        Some(body),
        "application/json",
        &[("Authorization", &format!("Bearer {token}"))],
    );
    (s, body_str(&b), b)
}

fn http_delete_auth(port: u16, path: &str, token: &str) -> (u16, String, Vec<u8>) {
    let (s, _, b) = http_request(port, "DELETE", path, None, "", &[("Authorization", &format!("Bearer {token}"))]);
    (s, body_str(&b), b)
}

fn json_parse(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("invalid json {e}: {body}"))
}

/// The `user_agent` pair endpoint through the Gateway proxy.
fn default_pair(port: u16, token: &str, agent: &str) -> serde_json::Value {
    let (code, body, _) = http_get_auth(port, &format!("/api/chat-sessions/default?pair={agent}"), token);
    assert_eq!(code, 200, "default pair; body:\n{body}");
    json_parse(&body)
}

fn messages(port: u16, token: &str, sid: &str) -> serde_json::Value {
    let (code, body, _) = http_get_auth(port, &format!("/api/chat-sessions/{sid}/messages"), token);
    assert_eq!(code, 200, "messages; body:\n{body}");
    json_parse(&body)
}

// ════════════════════════════ tests ════════════════════════════════════

/// Each test boots its own Gateway, and the direct user-service probes
/// scan loopback ports — two live suites would see each other's services.
/// Serialize the whole file (a few minutes total; the process-level cost
/// is the point of the tier). Poison recovery keeps one panic from
/// cascading into "every later test deadlocks".
static CHAIN_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_chain() -> std::sync::MutexGuard<'static, ()> {
    CHAIN_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// §5.4.4 + plan 2a + §4.6 + Wave 5.4a doorbell: the whole 1:1 turn chain
/// as the Desktop walks it, against the real broker and the real store.
#[test]
fn default_pair_chat_message_persists_and_agent_writeback_doorbells() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("chain");
    let http = gw.http_port;

    // Idempotent default pair.
    let s1 = default_pair(http, &token, "agent-chain-1");
    let sid = s1["session_id"].as_str().unwrap_or_else(|| panic!("no session_id in {s1}")).to_string();
    let s2 = default_pair(http, &token, "agent-chain-1");
    assert_eq!(s2["session_id"].as_str(), Some(sid.as_str()), "default pair is idempotent");
    assert_eq!(s1["kind"].as_str(), Some("user_agent"));
    assert_eq!(s1["participants"].as_array().map(|p| p.len()), Some(2));

    // Human message: the row is durable *before* delivery is attempted,
    // so an offline agent (no node at all here) must not lose the turn.
    let msg = serde_json::json!({"chat_session_id": sid, "from_user": root, "text": "hello agent"});
    let (code, body, _) = http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&msg).unwrap(), &token);
    assert_eq!(code, 200, "chat-message accepted; body:\n{body}");
    let v = json_parse(&body);
    assert!(v["message_id"].as_str().map(|s| !s.is_empty()).unwrap_or(false), "message_id issued: {body}");
    // No node → delivery fails, but loudly, in the body — never a silent 2xx lie.
    assert!(
        v["status"] == serde_json::json!("failed") || v["status"] == serde_json::json!("delivered"),
        "explicit delivery status: {body}"
    );

    let page = messages(http, &token, &sid);
    let rows = page["messages"].as_array().cloned().unwrap_or_default();
    let mine = rows.iter().rev().find(|r| r["body"].as_str() == Some("hello agent"));
    assert!(mine.is_some(), "user_message row persisted before/independent of delivery: {body}");
    assert_eq!(mine.unwrap()["kind"].as_str(), Some("user_message"));
    assert_eq!(mine.unwrap()["from"].as_str(), Some(root.as_str()));

    // Design 27 §4.5: a chat message may carry a `session_config` suggestion
    // (model / workspace / reasoning chosen on the Chat surface before send).
    // A malformed shape is rejected *before* the durable row is written, so a
    // bad suggestion never costs a message an address it will never get.
    let bad_cfg = serde_json::json!({
        "chat_session_id": sid, "from_user": root, "text": "x",
        "session_config": {"model": ""},
    });
    let (code, _, _) = http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&bad_cfg).unwrap(), &token);
    assert_eq!(code, 422, "empty session_config value rejected at the gate");
    let page_after_bad = messages(http, &token, &sid);
    assert_eq!(
        page_after_bad["messages"].as_array().unwrap().iter()
            .filter(|r| r["body"].as_str() == Some("x")).count(),
        0,
        "rejected message must not persist a row",
    );

    // A valid session_config is accepted (the shape gate is key-agnostic, so
    // workspace / reasoning pass alongside model).
    let good_cfg = serde_json::json!({
        "chat_session_id": sid, "from_user": root, "text": "seeded turn",
        "session_config": {"workspace": "w-1", "reasoning_effort": "high"},
    });
    let (code, _, _) = http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&good_cfg).unwrap(), &token);
    assert_eq!(code, 200, "valid session_config accepted; body above");

    // Sender-forgets-identity: a chat-message claiming another user is refused.
    let forged = serde_json::json!({"chat_session_id": sid, "from_user": "someone-else", "text": "x"});
    let (code, _, _) = http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&forged).unwrap(), &token);
    assert_eq!(code, 401, "from_user must match the token");

    // Agent write-back through the system surface (what the Runtime's
    // chat-append relay lands on): unread bumps and the doorbell fires on
    // the real broker for the human participant.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async move {
        use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
        let mut opts = MqttOptions::new("chat-e2e-doorbell", "127.0.0.1", gw.mqtt_port);
        opts.set_keep_alive(Duration::from_secs(2));
        let (client, mut eventloop) = AsyncClient::new(opts, 10);
        client.subscribe("acowork/users/+/chat/changed", QoS::AtLeastOnce).await.unwrap();
        // Drain the SUBACK.
        for _ in 0..8 {
            let ev = tokio::time::timeout(Duration::from_secs(5), eventloop.poll()).await;
            if let Ok(Ok(Event::Incoming(Incoming::SubAck(_)))) = ev {
                break;
            }
        }

        let up = user_service_port(&gw.home);
        let append = serde_json::json!({
            "session_id": sid,
            "message": {"ts": 0, "from": "agent-chain-1", "kind": "agent_message", "body": "sure, on it", "meta": {"message_id": "e2e-agent-row-1"}}
        });
        let (code, body, _) = http_post_json(up, "/internal/chat-sessions/messages", &serde_json::to_vec(&append).unwrap());
        assert_eq!(code, 201, "system append; body:\n{body}");

        let topic = format!("acowork/users/{root}/chat/changed");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let ev = tokio::time::timeout(Duration::from_secs(5), eventloop.poll())
                .await
                .expect("mqtt eventloop tick")
                .expect("mqtt event");
            if let Event::Incoming(Incoming::Publish(p)) = ev
                && p.topic == topic
            {
                break; // doorbell observed on the wire
            }
            assert!(Instant::now() < deadline, "no chat/changed doorbell for {root}");
        }

        // §4.6: the agent row bumped the human's unread; opening the
        // session (the user-facing read) clears it.
        let list = {
            let (code, body, _) = http_get_auth(http, &format!("/api/chat-sessions/{root}"), &token);
            assert_eq!(code, 200, "list; body:\n{body}");
            json_parse(&body)
        };
        let sess = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["session_id"].as_str() == Some(sid.as_str()))
            .expect("session in list");
        assert_eq!(sess["unread"][&root].as_u64(), Some(1), "unread after agent write-back");
        let _ = messages(http, &token, &sid); // the read that clears
        let (code, body, _) = http_get_auth(http, &format!("/api/chat-sessions/{root}"), &token);
        assert_eq!(code, 200);
        let list = json_parse(&body);
        let sess = list.as_array().unwrap().iter().find(|s| s["session_id"].as_str() == Some(sid.as_str())).unwrap();
        assert!(
            sess["unread"].as_object().map(|m| m.get(&root).map(|v| v.as_u64() == Some(0)).unwrap_or(true)).unwrap_or(true),
            "unread cleared by the read: {body}"
        );
    });
}

/// Plan 5.2 guards: the default pair and project-group sessions refuse
/// deletion, a transient session deletes, and a non-member gets the
/// hidden 404 on every surface.
#[test]
fn delete_guards_and_hidden_404_for_non_member() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("guards");
    let http = gw.http_port;

    // Default pair: undeletable, and the refusal says why.
    let sid = default_pair(http, &token, "agent-guard-1")["session_id"].as_str().unwrap().to_string();
    let (code, body, _) = http_delete_auth(http, &format!("/api/chat-sessions/{sid}"), &token);
    assert!((400..500).contains(&code), "default pair delete refused ({code}): {body}");
    let (c2, _, _) = http_get_auth(http, &format!("/api/chat-sessions/{sid}/messages"), &token);
    assert_eq!(c2, 200, "session still alive after refused delete");

    // Project group: undeletable (its lifecycle is the project's). The
    // public create surface refuses this kind by design — it is minted by
    // PM through the internal group entry.
    let up = user_service_port(&gw.home);
    let group = serde_json::json!({"project_id": "proj-guard-1", "participants": [root, "agent-guard-g"], "title": "G"});
    let (code, body, _) = http_post_json(up, "/internal/chat-sessions/group", &serde_json::to_vec(&group).unwrap());
    assert!((200..300).contains(&code), "group create (internal): {code} {body}");
    let gsid = json_parse(&body)["session_id"].as_str().unwrap().to_string();
    let (code, body, _) = http_delete_auth(http, &format!("/api/chat-sessions/{gsid}"), &token);
    assert!((400..500).contains(&code), "project group delete refused ({code}): {body}");

    // Transient user_agent session (not the default pair): deletable.
    let tmp = serde_json::json!({"kind": "user_agent", "participants": [root, "agent-guard-tmp"], "title": "tmp"});
    let (code, body, _) = http_post_json_auth(http, "/api/chat-sessions", &serde_json::to_vec(&tmp).unwrap(), &token);
    assert!((200..300).contains(&code), "transient create: {code} {body}");
    let tsid = json_parse(&body)["session_id"].as_str().unwrap().to_string();
    let (code, _, _) = http_delete_auth(http, &format!("/api/chat-sessions/{tsid}"), &token);
    assert!((200..300).contains(&code), "transient delete ok ({code})");
    let (code, _, _) = http_get_auth(http, &format!("/api/chat-sessions/{tsid}/messages"), &token);
    assert_eq!(code, 404, "deleted session reads 404");

    // Non-member: a session created on the system surface between two
    // ghosts is invisible to the admin's user through every public path —
    // hidden 404, never 403 (design 27 §5.3).
    let up = user_service_port(&gw.home);
    let pair = serde_json::json!({"a": "ghost-h", "b": "ghost-a", "kind": "agent_agent"});
    let (code, body, _) = http_post_json(up, "/internal/chat-sessions/pair", &serde_json::to_vec(&pair).unwrap());
    assert!((200..300).contains(&code), "ghost pair: {code} {body}");
    let gsid = json_parse(&body)["session_id"].as_str().unwrap().to_string();
    let (code, _, _) = http_get_auth(http, &format!("/api/chat-sessions/{gsid}/messages"), &token);
    assert_eq!(code, 404, "non-member read is a hidden 404");
    let (code, _, _) = http_delete_auth(http, &format!("/api/chat-sessions/{gsid}"), &token);
    assert_eq!(code, 404, "non-member delete is a hidden 404");
}

/// §4.2.1 (plan 3.5): upload → reference → download round-trip, and the
/// store refuses attachment ids from another session's `files/` dir.
#[test]
fn attachments_upload_reference_download_and_cross_session_refusal() {
    let _serial = lock_chain();
    let (gw, token, _root) = boot_multi_user("attach");
    let http = gw.http_port;
    let sid = default_pair(http, &token, "agent-attach-1")["session_id"].as_str().unwrap().to_string();
    let sid2 = default_pair(http, &token, "agent-attach-2")["session_id"].as_str().unwrap().to_string();

    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3, 4];
    let (aid, _) = upload_file(http, &token, &sid, "pic.png", "image/png", &png);

    // Reference it in a user-facing message (the human write entry).
    let m = serde_json::json!({"body": "look", "to": "", "attachment_ids": [aid]});
    let (code, body, _) = http_post_json_auth(http, &format!("/api/chat-sessions/{sid}/messages"), &serde_json::to_vec(&m).unwrap(), &token);
    assert!((200..300).contains(&code), "message with attachment: {code} {body}");
    let page = messages(http, &token, &sid);
    let row = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["body"].as_str() == Some("look"))
        .expect("row with attachment");
    assert_eq!(row["attachments"][0]["id"].as_str(), Some(aid.as_str()), "server-resolved metadata: {row}");
    assert_eq!(row["attachments"][0]["mime"].as_str(), Some("image/png"));

    // Download round-trip.
    let (code, _, bytes) = http_get_auth(http, &format!("/api/chat-sessions/{sid}/files/{aid}"), &token);
    assert_eq!(code, 200, "download");
    assert_eq!(bytes, png, "byte-exact blob round-trip");

    // Cross-session reference is refused (ids are scoped to the session).
    let (aid2, _) = upload_file(http, &token, &sid2, "other.png", "image/png", &png);
    let bad = serde_json::json!({"body": "steal", "to": "", "attachment_ids": [aid2]});
    let (code, body, _) = http_post_json_auth(http, &format!("/api/chat-sessions/{sid}/messages"), &serde_json::to_vec(&bad).unwrap(), &token);
    assert!((400..500).contains(&code), "foreign attachment id refused ({code}): {body}");
}

fn upload_file(port: u16, token: &str, sid: &str, name: &str, mime: &str, bytes: &[u8]) -> (String, serde_json::Value) {
    let boundary = "----acoworkE2EBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let ct = format!("multipart/form-data; boundary={boundary}");
    let (code, _, b) = http_request(
        port,
        "POST",
        &format!("/api/chat-sessions/{sid}/files"),
        Some(&body),
        &ct,
        &[("Authorization", &format!("Bearer {token}"))],
    );
    let text = body_str(&b);
    assert!((200..300).contains(&code), "upload {name} ({code}): {text}");
    let v = json_parse(&text);
    (v["id"].as_str().expect("attachment id").to_string(), v)
}

/// §8.3 + §10.19: the participant set converges through the sync entry,
/// removal revokes list visibility, and a reinstalled agent's history
/// stays readable while the live pair resolves to a fresh session.
#[test]
fn participant_sync_group_and_reinstall_history() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("sync");
    let http = gw.http_port;
    let up = user_service_port(&gw.home);

    // Reinstall: old pair session keeps its history and stays listed.
    let old_sid = default_pair(http, &token, "agent-inst-old")["session_id"].as_str().unwrap().to_string();
    let m = serde_json::json!({"chat_session_id": old_sid, "from_user": root, "text": "before reinstall"});
    http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&m).unwrap(), &token);
    let new_sid = default_pair(http, &token, "agent-inst-new")["session_id"].as_str().unwrap().to_string();
    assert_ne!(old_sid, new_sid, "a new instance id resolves a new pair");
    let (code, body, _) = http_get_auth(http, &format!("/api/chat-sessions/{root}"), &token);
    assert_eq!(code, 200);
    let list = json_parse(&body);
    let ids: Vec<&str> = list.as_array().unwrap().iter().filter_map(|s| s["session_id"].as_str()).collect();
    assert!(ids.contains(&old_sid.as_str()) && ids.contains(&new_sid.as_str()), "both pairs listed: {body}");
    let page = messages(http, &token, &old_sid);
    assert!(
        page["messages"].as_array().unwrap().iter().any(|r| r["body"].as_str() == Some("before reinstall")),
        "old history readable after reinstall"
    );

    // Group sync converges the participant set exactly (§8.3), and a
    // removed member's list loses the session.
    let sync = |participants: Vec<&str>| -> String {
        let b = serde_json::json!({"project_id": "proj-sync-1", "participants": participants, "title": "S"});
        let (code, body, _) = http_post_json(up, "/internal/chat-sessions/group/sync", &serde_json::to_vec(&b).unwrap());
        assert!((200..300).contains(&code), "group sync: {code} {body}");
        json_parse(&body)["session_id"].as_str().unwrap().to_string()
    };
    let gsid = sync(vec!["p-one", "p-two"]);
    assert_eq!(sync(vec!["p-one", "p-two"]), gsid, "sync is session-stable");
    assert_eq!(sync(vec!["p-one"]), gsid, "convergence reuses the same session");
    let listed = |pid: &str| -> bool {
        // The system list endpoint answers session ids, not full sessions.
        let (code, body, _) = http_get_plain(up, &format!("/internal/chat-sessions?participant={pid}"));
        assert_eq!(code, 200, "internal list: {code} {body}");
        json_parse(&body).as_array().unwrap().iter().any(|s| s.as_str() == Some(gsid.as_str()))
    };
    assert!(listed("p-one"), "kept member still lists the group");
    assert!(!listed("p-two"), "removed member no longer lists it (§8.3 revocation)");

    // Single add/remove through the mediated endpoints, idempotently.
    let b = serde_json::json!({"id": "p-three", "identity": "human"});
    let (code, _, _) = http_post_json(up, &format!("/internal/chat-sessions/{gsid}/participants"), &serde_json::to_vec(&b).unwrap());
    assert!((200..300).contains(&code), "add participant");
    let (code, _, _) = http_post_json(up, &format!("/internal/chat-sessions/{gsid}/participants"), &serde_json::to_vec(&b).unwrap());
    assert!((200..300).contains(&code), "add is idempotent");
    assert!(listed("p-three"));
    let (code, _, _) = http_request(up, "DELETE", &format!("/internal/chat-sessions/{gsid}/participants/p-three"), None, "", &[]);
    assert!((200..300).contains(&code), "remove participant");
    assert!(!listed("p-three"), "removed from the list too (index maintained)");
}

/// §5.5.4: an answer naming a card that does not exist is NOT dropped —
/// it falls through to a plain human turn (the row is durable, the
/// `in_reply_to` is recorded in its meta) and the delivery outcome is
/// reported explicitly. A silent swallow would be the real failure mode.
#[test]
fn card_answer_for_unknown_card_falls_through_as_plain_turn() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("card");
    let http = gw.http_port;
    let sid = default_pair(http, &token, "agent-card-1")["session_id"].as_str().unwrap().to_string();
    let before = messages(http, &token, &sid)["total"].as_u64().unwrap_or(0);

    let m = serde_json::json!({"chat_session_id": sid, "from_user": root, "text": "yes do it", "in_reply_to": "no-such-card-id"});
    let (code, body, _) = http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&m).unwrap(), &token);
    assert_eq!(code, 200, "fall-through turn is accepted: {code} {body}");
    let v = json_parse(&body);
    assert!(
        v["status"] == serde_json::json!("failed") || v["status"] == serde_json::json!("delivered"),
        "explicit delivery status: {body}"
    );
    let page = messages(http, &token, &sid);
    assert_eq!(page["total"].as_u64(), Some(before + 1), "the answer row is durable");
    let row = page["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(row["body"].as_str(), Some("yes do it"));
    assert_eq!(row["meta"]["in_reply_to"].as_str(), Some("no-such-card-id"), "the quote is recorded");
}

/// Design 27 §5.3 form ③: a group turn — the human's row archives in the
/// group session, then the @-fan-out reports each target's outcome
/// explicitly (offline agent = loud `failed`, never a silent swallow).
#[test]
fn group_mention_fans_out_with_explicit_per_target_status() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("mention");
    let http = gw.http_port;
    let up = user_service_port(&gw.home);

    // The group session as PM's sync entry mints it (project lifecycle).
    let b = serde_json::json!({"project_id": "proj-mention-1", "participants": [root, "agent-mention-g"], "title": "M"});
    let (code, body, _) = http_post_json(up, "/internal/chat-sessions/group/sync", &serde_json::to_vec(&b).unwrap());
    assert!((200..300).contains(&code), "group sync: {code} {body}");
    let gsid = json_parse(&body)["session_id"].as_str().unwrap().to_string();

    // The row first (single write entry), then the fan-out.
    let m = serde_json::json!({"body": "@g take this", "to": ""});
    let (code, body, _) = http_post_json_auth(http, &format!("/api/chat-sessions/{gsid}/messages"), &serde_json::to_vec(&m).unwrap(), &token);
    assert!((200..300).contains(&code), "group user_message: {code} {body}");
    let page = messages(http, &token, &gsid);
    assert_eq!(
        page["messages"].as_array().unwrap().iter().filter(|r| r["kind"].as_str() == Some("user_message")).count(),
        1
    );

    let fan = serde_json::json!({"chat_session_id": gsid, "from_user": root, "targets": ["agent-mention-g"], "text": "@g take this"});
    let (code, body, _) = http_post_json_auth(http, "/api/intents/mentions", &serde_json::to_vec(&fan).unwrap(), &token);
    assert_eq!(code, 200, "mentions accepted: {code} {body}");
    let v = json_parse(&body);
    let results = v["results"].as_array().unwrap_or_else(|| panic!("per-target results: {v}"));
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["target"].as_str(), Some("agent-mention-g"));
    assert!(
        results[0]["status"] == serde_json::json!("failed") || results[0]["status"] == serde_json::json!("delivered"),
        "explicit outcome, not silence: {results:?}"
    );

    // A non-participant may not fan out into someone else's group.
    let other = serde_json::json!({"chat_session_id": gsid, "from_user": "not-a-member", "targets": ["agent-mention-g"], "text": "x"});
    let (code, _, _) = http_post_json_auth(http, "/api/intents/mentions", &serde_json::to_vec(&other).unwrap(), &token);
    assert!((400..500).contains(&code), "non-member fan-out refused ({code})");
}

/// The card-answer channel of `chat-message` also serves group sessions
/// (§5.5.4: the card names its own agent), but a group session has no
/// blanket delivery target — a plain message or an answer naming a
/// non-card row must be refused, not silently delivered.
#[test]
fn chat_message_in_group_answers_cards_only() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("groupmsg");
    let http = gw.http_port;
    let up = user_service_port(&gw.home);

    let b = serde_json::json!({"project_id": "proj-chatmsg-1", "participants": [root, "agent-groupmsg"], "title": "G"});
    let (code, body, _) = http_post_json(up, "/internal/chat-sessions/group/sync", &serde_json::to_vec(&b).unwrap());
    assert!((200..300).contains(&code), "group sync: {code} {body}");
    let gsid = json_parse(&body)["session_id"].as_str().unwrap().to_string();

    // A plain group message does not go through the chat-message channel.
    let m = serde_json::json!({"chat_session_id": gsid, "from_user": root, "text": "hello group"});
    let (code, body, _) = http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&m).unwrap(), &token);
    assert_eq!(code, 400, "plain group message refused on chat-message: {body}");

    // An answer naming a row that is not a card is refused too (there is
    // no derivable delivery target).
    let a = serde_json::json!({"chat_session_id": gsid, "from_user": root, "text": "answer", "in_reply_to": "no-such-message"});
    let (code, body, _) = http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&a).unwrap(), &token);
    assert_eq!(code, 400, "group answer must name an interaction_request card: {body}");
}

/// Design 27 §5.4.3 form ②: A–B intents share ONE agent-agent channel —
/// the pair entry is idempotent, both agents list it, and a row written
/// by A is in the session B reads. (The MQTT wire half of this form is
/// covered by `intent_delivery.rs` against a real broker; the process
/// tier cannot mint a node token from outside.)
#[test]
fn agent_agent_pair_is_one_shared_channel_for_both_sides() {
    let _serial = lock_chain();
    let (gw, _token, _root) = boot_multi_user("aapair");
    let up = user_service_port(&gw.home);

    let a = "11111111-1111-4111-8111-111111111111";
    let b = "22222222-2222-4222-8222-222222222222";
    let pair = |x: &str, y: &str| -> String {
        let body = serde_json::json!({"a": x, "b": y, "kind": "agent_agent"});
        let (code, resp, _) = http_post_json(up, "/internal/chat-sessions/pair", &serde_json::to_vec(&body).unwrap());
        assert!((200..300).contains(&code), "pair {x}×{y}: {code} {resp}");
        json_parse(&resp)["session_id"].as_str().unwrap().to_string()
    };
    let sid_ab = pair(a, b);
    let sid_ba = pair(b, a);
    assert_eq!(sid_ab, sid_ba, "A×B and B×A resolve to one shared session (§5.4.3)");

    let append = serde_json::json!({
        "session_id": sid_ab,
        "message": {"ts": 0, "from": a, "kind": "agent_message", "body": "from A", "meta": {"message_id": "aa-row-1"}}
    });
    let (code, body, _) = http_post_json(up, "/internal/chat-sessions/messages", &serde_json::to_vec(&append).unwrap());
    assert!((200..300).contains(&code), "A writes: {code} {body}");

    let listed = |pid: &str| -> Vec<String> {
        let (code, body, _) = http_get_plain(up, &format!("/internal/chat-sessions?participant={pid}"));
        assert_eq!(code, 200, "list {pid}: {code} {body}");
        json_parse(&body).as_array().unwrap().iter().filter_map(|s| s.as_str().map(str::to_string)).collect()
    };
    assert!(listed(a).contains(&sid_ab), "A lists the channel");
    assert!(listed(b).contains(&sid_ab), "B lists the same channel");

    let sess = {
        let (code, body, _) = http_get_plain(up, &format!("/internal/chat-sessions/{sid_ab}"));
        assert_eq!(code, 200);
        json_parse(&body)
    };
    assert_eq!(sess["kind"].as_str(), Some("agent_agent"));
    assert_eq!(sess["participants"].as_array().map(|p| p.len()), Some(2));

    // B cannot mint a row attributed to A (participant check on the write).
    let forged = serde_json::json!({
        "session_id": sid_ab,
        "message": {"ts": 0, "from": "99999999-9999-4999-8999-999999999999", "kind": "agent_message", "body": "x", "meta": {"message_id": "aa-row-2"}}
    });
    let (code, _, _) = http_post_json(up, "/internal/chat-sessions/messages", &serde_json::to_vec(&forged).unwrap());
    assert!((400..500).contains(&code), "non-participant write refused ({code})");
}

/// Boot a `local`-mode gateway (the desktop default form): no account
/// system, no token, no bootstrap admin — the auth middleware is a no-op
/// and the user service deliberately does not mount the chat-session
/// surface (ADR-076 §决策 12; the new Chat store is multi_user-only by
/// design, the legacy ChatPanel surface stays).
fn boot_local(tag: &str) -> Gateway {
    let home = temp_home(tag);
    let (http, mqtt) = (free_port(), free_port());
    let home_s = home.display().to_string().replace('\\', "/");
    let config_path = home.join("config").join("gateway.toml");
    fs::write(
        &config_path,
        format!(
            r#"
vault_dir = "{home}/data/vault"
packages_dir = "{home}/config/packages"
data_dir = "{home}/data"

[user]
config = "{home}/acowork-user.toml"
"#,
            home = home_s
        ),
    )
    .expect("write gateway.toml");
    fs::write(home.join("acowork-user.toml"), "").expect("write user config");

    let mut cmd = Command::new(gateway_binary());
    cmd.arg("--daemon")
        .arg("--home")
        .arg(&home)
        .arg("--addr")
        .arg(format!("127.0.0.1:{http}"))
        .arg("--mqtt-addr")
        .arg(format!("127.0.0.1:{mqtt}"))
        .arg("--no-spawn-local-node")
        .arg("--config-path")
        .arg(&config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn acowork-gateway");
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(pipe) = child.stderr.take() {
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                if let Ok(line) = line
                    && let Ok(mut buf) = sink.lock()
                {
                    buf.push_str(&line);
                    buf.push('\n');
                }
            }
        });
    }
    let mut gw = Gateway {
        child,
        home,
        http_port: http,
        mqtt_port: mqtt,
        stderr,
    };
    gw.wait_until_serving();
    gw
}

/// Wave 6 acceptance, third form: `local` mode keeps the legacy surface.
/// The new chat write entries must refuse cleanly (401 — identity is
/// token-derived and there is no token), and the proxied chat read must
/// fail soft (4xx, never a 5xx or a hang) because the user service does
/// not mount the route at all. A future "make local mode work like
/// multi_user" shortcut that removes the gate would flip these codes.
#[test]
fn local_mode_refuses_chat_writes_and_serves_no_session_surface() {
    let _serial = lock_chain();
    let gw = boot_local("local");
    let http = gw.http_port;

    // Wait for the user service to be up (proxy stops answering 503).
    let deadline = Instant::now() + BOOT_TIMEOUT;
    loop {
        let (code, _, _) = http_get_plain(http, "/api/chat-sessions/human");
        if code != 503 {
            break;
        }
        assert!(Instant::now() < deadline, "user service never came up; stderr:\n{}", gw.stderr());
        sleep(Duration::from_millis(100));
    }

    // Token-derived identity is required at every chat write entry; local
    // mode has no token → 401, before any body validation or user-service
    // call (so even a well-formed request is refused).
    let msg = serde_json::json!({"chat_session_id": "whatever", "from_user": "human", "text": "hi"});
    let (code, body, _) = http_post_json(http, "/api/intents/chat-message", &serde_json::to_vec(&msg).unwrap());
    assert_eq!(code, 401, "chat-message without identity in local mode; body:\n{body}");

    let mention = serde_json::json!({"chat_session_id": "whatever", "from_user": "human", "text": "hi", "targets": ["a"]});
    let (code, body, _) = http_post_json(http, "/api/intents/mentions", &serde_json::to_vec(&mention).unwrap());
    assert_eq!(code, 401, "mentions without identity in local mode; body:\n{body}");

    // The public session surface is not mounted in local mode: the proxy
    // forwards and the user service answers 404 — a clean miss, not a
    // 5xx, not a timeout.
    let (code, _, _) = http_get_plain(http, "/api/chat-sessions/human");
    assert!((400..500).contains(&code), "session list fails soft in local mode ({code})");
}

// ─────────────────────────────────────────────────────────────────────
// 6.2 — the legacy human↔human surface runs on the one chat store.
// Wire contract (paths, chat_id = sorted pair joined by "__", response
// shapes) is unchanged; the adapter in `chat_api.rs` now reads and
// writes `chat_sessions`. This test pins that the *product* behaviour
// callers depend on survives the heart transplant: send → list with
// peer label → read → unread badge → mark_read clears → hidden 404
// for a non-member of the pair.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn human_to_human_legacy_routes_run_on_the_new_store() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("chathh");
    let port = gw.http_port;

    // A second human: admin creates the account, then logs in for its own token.
    let body = serde_json::to_vec(&serde_json::json!({
        "username": "pal", "password": "palpass123", "display_name": "Pal"
    }))
    .unwrap();
    let (code, body, _) = http_post_json_auth(port, "/api/users", &body, &token);
    assert_eq!(code, 201, "create pal; body:\n{body}");
    let pal = json_parse(&body)["account"]["user_id"]
        .as_str()
        .expect("user_id")
        .to_string();

    let body = serde_json::to_vec(&serde_json::json!({"username": "pal", "password": "palpass123"}))
        .unwrap();
    let (code, body, _) = http_post_json(port, "/api/auth/login", &body);
    assert_eq!(code, 200, "pal login; body:\n{body}");
    let pal_token = json_parse(&body)["access_token"]
        .as_str()
        .expect("access_token")
        .to_string();

    // The wire chat_id is the frontend's local join: the two uids sorted,
    // "__"-separated. The adapter resolves it through pair_lookup — the
    // same key the startup migration writes — so the pair is found without
    // the client ever minting a session id.
    let (lo, hi) = if root < pal {
        (root.as_str(), pal.as_str())
    } else {
        (pal.as_str(), root.as_str())
    };
    let chat_id = format!("{lo}__{hi}");

    // root → pal: first send mints the UserUser session (201, seq starts at 1).
    let msg = serde_json::to_vec(&serde_json::json!({"body": "ping"})).unwrap();
    let (code, body, _) = http_post_json_auth(
        port,
        &format!("/api/users/{root}/chats/{chat_id}/messages"),
        &msg,
        &token,
    );
    assert_eq!(code, 201, "send ping; body:\n{body}");
    let sent = json_parse(&body);
    assert_eq!(sent["seq"].as_u64(), Some(1), "seq:\n{body}");
    assert_eq!(sent["from"].as_str(), Some(root.as_str()), "body:\n{body}");

    // pal's list: the chat is there, unread badge 1, peer label resolved
    // server-side (a non-admin cannot read /api/users to label it itself).
    let (code, body, _) = http_get_auth(port, &format!("/api/users/{pal}/chats"), &pal_token);
    assert_eq!(code, 200, "pal list; body:\n{body}");
    let chats = json_parse(&body);
    let chat = chats["chats"]
        .as_array()
        .expect("chats array")
        .iter()
        .find(|c| c["chat_id"].as_str() == Some(&chat_id))
        .unwrap_or_else(|| panic!("pair chat missing from pal list:\n{body}"));
    assert_eq!(chat["unread_count"].as_u64(), Some(1), "unread:\n{body}");
    assert_eq!(
        chat["peer_display_name"].as_str(),
        Some("Root"),
        "peer label:\n{body}"
    );

    // pal reads the thread, then replies — seq continues monotonically.
    let (code, body, _) = http_get_auth(
        port,
        &format!("/api/users/{pal}/chats/{chat_id}/messages"),
        &pal_token,
    );
    assert_eq!(code, 200, "pal messages; body:\n{body}");
    let page = json_parse(&body);
    assert_eq!(page["total"].as_u64(), Some(1), "page:\n{body}");
    assert_eq!(page["messages"][0]["body"].as_str(), Some("ping"));

    let msg = serde_json::to_vec(&serde_json::json!({"body": "pong"})).unwrap();
    let (code, body, _) = http_post_json_auth(
        port,
        &format!("/api/users/{pal}/chats/{chat_id}/messages"),
        &msg,
        &pal_token,
    );
    assert_eq!(code, 201, "send pong; body:\n{body}");
    assert_eq!(
        json_parse(&body)["seq"].as_u64(),
        Some(2),
        "seq continues:\n{body}"
    );

    // root's badge counts pal's reply; mark_read clears it (204, idempotent
    // silence on a second call is the store's contract, not asserted here).
    let (code, body, _) = http_get_auth(port, &format!("/api/users/{root}/chats"), &token);
    assert_eq!(code, 200, "root list; body:\n{body}");
    let listed = json_parse(&body);
    let root_chat = listed["chats"]
        .as_array()
        .expect("chats")
        .iter()
        .find(|c| c["chat_id"].as_str() == Some(&chat_id))
        .expect("root sees the pair");
    assert_eq!(root_chat["unread_count"].as_u64(), Some(1), "root:\n{body}");

    let (code, body, _) = http_post_json_auth(
        port,
        &format!("/api/users/{root}/chats/{chat_id}/read"),
        b"{}",
        &token,
    );
    assert_eq!(code, 204, "mark_read; body:\n{body}");
    let (code, body, _) = http_get_auth(port, &format!("/api/users/{root}/chats"), &token);
    assert_eq!(code, 200);
    let listed = json_parse(&body);
    let root_chat = listed["chats"]
        .as_array()
        .expect("chats")
        .iter()
        .find(|c| c["chat_id"].as_str() == Some(&chat_id))
        .expect("root sees the pair");
    assert_eq!(root_chat["unread_count"].as_u64(), Some(0), "cleared:\n{body}");

    // A third account is not in the pair: the same hidden 404 the new store
    // enforces everywhere — the old surface answered 403 here, and the
    // convergence to 404 is deliberate (design 27 §5.3).
    let body = serde_json::to_vec(&serde_json::json!({
        "username": "spy", "password": "spypass123", "display_name": "Spy"
    }))
    .unwrap();
    let (code, body, _) = http_post_json_auth(port, "/api/users", &body, &token);
    assert_eq!(code, 201, "create spy; body:\n{body}");
    let spy = json_parse(&body)["account"]["user_id"]
        .as_str()
        .expect("user_id")
        .to_string();
    let body = serde_json::to_vec(&serde_json::json!({"username": "spy", "password": "spypass123"}))
        .unwrap();
    let (code, body, _) = http_post_json(port, "/api/auth/login", &body);
    assert_eq!(code, 200, "spy login; body:\n{body}");
    let spy_token = json_parse(&body)["access_token"]
        .as_str()
        .expect("access_token")
        .to_string();

    let (code, _, _) = http_get_auth(
        port,
        &format!("/api/users/{spy}/chats/{chat_id}/messages"),
        &spy_token,
    );
    assert_eq!(code, 404, "non-member gets the hidden 404");
}

/// ADR-100 D12: an execution view's composer sends through the Chat entry, so
/// the head of the turn is written under the caller's own identity and lands
/// durably before delivery is attempted. The row carries display names for its
/// attachments and nothing that the Chat side could lie about opening.
#[test]
fn a_pinned_send_lands_the_head_row_with_display_only_attachments() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("pinned");
    let http = gw.http_port;
    let sid = default_pair(http, &token, "agent-pin-1")["session_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The composer mints the row id itself: the bubble it already rendered,
    // this Chat row and the execution session's own line are one row.
    let m = serde_json::json!({
        "chat_session_id": sid,
        "from_user": root,
        "text": "keep going on the fix",
        "message_id": "msg-pin-1",
        "for_execution_session": "20261023_131156_a3bb7d",
        "command": "review",
        "attached_items": [
            { "type": "attached_file", "name": "src/main.rs", "absPath": "E:/work/demo/src/main.rs" },
            { "type": "file_upload", "documentId": "doc-notes", "filename": "notes.md", "format": "md", "sizeBytes": 12 },
        ],
    });
    let (code, body, _) =
        http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&m).unwrap(), &token);
    assert_eq!(code, 200, "pinned send accepted; body:\n{body}");

    let page = messages(http, &token, &sid);
    let rows = page["messages"].as_array().cloned().unwrap_or_default();
    let row = rows
        .iter()
        .rev()
        .find(|r| r["meta"]["message_id"].as_str() == Some("msg-pin-1"))
        .unwrap_or_else(|| panic!("the client row id must be honoured, not replaced: {body}"));
    assert_eq!(row["kind"].as_str(), Some("user_message"));
    assert_eq!(
        row["from"].as_str(),
        Some(root.as_str()),
        "the head row speaks as the human, never as the Runtime"
    );

    // Display text only. A node-local path means nothing to the Chat store
    // (ADR-009 §5) and an unopenable reference is worse than none.
    let labels = row["meta"]["exec_attachments"].as_array().cloned().unwrap_or_default();
    let names: Vec<&str> = labels.iter().filter_map(|v| v["filename"].as_str()).collect();
    assert!(names.contains(&"src/main.rs"), "workspace arm keeps its display name: {names:?}");
    assert!(names.contains(&"notes.md"), "upload arm keeps its filename: {names:?}");
    let raw = serde_json::to_string(&labels).unwrap();
    assert!(!raw.contains("E:/work/demo") && !raw.contains("absPath"), "no path rides along: {raw}");

    // Handing the task a file with no message is a legitimate send on the
    // execution surface — the pinned path must not reject what the
    // session-scoped write accepted.
    let only = serde_json::json!({
        "chat_session_id": sid,
        "from_user": root,
        "text": "",
        "message_id": "msg-pin-2",
        "for_execution_session": "20261023_131156_a3bb7d",
        "attached_items": [{ "type": "file_upload", "documentId": "doc-diff", "filename": "diff.patch", "format": "patch", "sizeBytes": 7 }],
    });
    let (code, body, _) =
        http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&only).unwrap(), &token);
    assert_eq!(code, 200, "an attachment-only pinned send is a turn: {code} {body}");

    // The ceilings bite only where the field would actually be delivered.
    let long = "x".repeat(200);
    let over = serde_json::json!({
        "chat_session_id": sid, "from_user": root, "text": "oversized skill name",
        "message_id": "msg-pin-3", "for_execution_session": "20261023_131156_a3bb7d",
        "command": long,
    });
    let (code, body, _) =
        http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&over).unwrap(), &token);
    assert_eq!(code, 400, "a pinned skill name is not silently truncated: {code} {body}");
    let after = messages(http, &token, &sid);
    assert_eq!(
        after["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["body"].as_str() == Some("oversized skill name"))
            .count(),
        0,
        "a rejected send writes no row",
    );

    // Without a pin the same field is dropped before delivery (a routed turn
    // is consumed as an Intent, which has no skill arm), so refusing the
    // message would punish the human for a value nobody reads.
    let unpin = serde_json::json!({
        "chat_session_id": sid, "from_user": root, "text": "no pin here", "command": long,
    });
    let (code, body, _) =
        http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&unpin).unwrap(), &token);
    assert_eq!(code, 200, "an unpinned overshoot is dropped, not rejected: {code} {body}");
}

/// ADR-100 D12: a PM task's execution session is bound to the PROJECT GROUP it was
/// dispatched from (ADR-099), and a group has no single agent to deliver to — so
/// without a pin-aware branch the follow-up the whole feature exists for is
/// refused with a 400. The executor the client names is checked, not honoured:
/// its answer is written into this chat, so it has to belong to it.
#[test]
fn a_pinned_send_into_a_group_files_the_head_row_and_checks_its_target() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("pingroup");
    let http = gw.http_port;
    let up = user_service_port(&gw.home);

    let b = serde_json::json!({"project_id": "proj-pingroup-1", "participants": [root, "agent-pin-g"], "title": "G"});
    let (code, body, _) = http_post_json(up, "/internal/chat-sessions/group/sync", &serde_json::to_vec(&b).unwrap());
    assert!((200..300).contains(&code), "group sync: {code} {body}");
    let gsid = json_parse(&body)["session_id"].as_str().unwrap().to_string();

    let m = serde_json::json!({
        "chat_session_id": gsid,
        "from_user": root,
        "text": "continue the task",
        "message_id": "msg-gp-1",
        "for_execution_session": "20261024_090000_deadbeef",
        "pinned_agent": "agent-pin-g",
    });
    let (code, body, _) =
        http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&m).unwrap(), &token);
    assert_eq!(code, 200, "a pinned send into a group is a turn: {code} {body}");

    let page = messages(http, &token, &gsid);
    let row = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|r| r["meta"]["message_id"].as_str() == Some("msg-gp-1"))
        .unwrap_or_else(|| panic!("the head row must be filed in the group: {page}"));
    assert_eq!(row["kind"].as_str(), Some("user_message"));
    assert_eq!(
        row["from"].as_str(),
        Some(root.as_str()),
        "the human speaks for themselves in the group too"
    );
    assert_eq!(row["to"].as_str(), Some("agent-pin-g"), "the row names who answers it");

    // The claim is verified against this chat's participants.
    let stranger = serde_json::json!({
        "chat_session_id": gsid,
        "from_user": root,
        "text": "run this elsewhere",
        "message_id": "msg-gp-2",
        "for_execution_session": "20261024_090000_deadbeef",
        "pinned_agent": "agent-not-in-group",
    });
    let (code, body, _) =
        http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&stranger).unwrap(), &token);
    assert_eq!(code, 403, "a foreign instance is refused: {code} {body}");
    let after = messages(http, &token, &gsid);
    assert_eq!(
        after["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["body"].as_str() == Some("run this elsewhere"))
            .count(),
        0,
        "a refused send writes no row"
    );

    // A plain group message still has no derivable target: D12 opened the door
    // for the pinned turn only, never for the archive path (§5.3 form ③).
    let plain = serde_json::json!({"chat_session_id": gsid, "from_user": root, "text": "hello group"});
    let (code, body, _) =
        http_post_json_auth(http, "/api/intents/chat-message", &serde_json::to_vec(&plain).unwrap(), &token);
    assert_eq!(code, 400, "an unpinned group send still needs a card: {code} {body}");
}

/// ADR-100 D12: the content gate and the attachment check must judge the turn
/// this Gateway will ACTUALLY deliver, not the bytes that arrived. Two shapes
/// used to be accepted at the gate and then die — a pinned entry the Runtime
/// could not parse would have shown the chat a chip for a file the Agent never
/// received, and a label-less item with no text filed an empty row.
///
/// The attachment arm is NOT pin-only: a routed turn (the Chat surface's own
/// composer, which never pins) carries `attached_items` into the `Intent`
/// shape, so an attachment-only send is content on both paths. `command` stays
/// pin-only — the Intent shape has no skill arm.
#[test]
fn a_send_with_nothing_to_deliver_is_a_readable_400() {
    let _serial = lock_chain();
    let (gw, token, root) = boot_multi_user("capgate");
    let http = gw.http_port;
    let sid = default_pair(http, &token, "agent-cap-1")["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let pin = "20261024_090000_deadbeef";
    let post = |body: &serde_json::Value| {
        let (code, body, _) = http_post_json_auth(
            http,
            "/api/intents/chat-message",
            &serde_json::to_vec(body).unwrap(),
            &token,
        );
        (code, body)
    };

    // (a) No pin: the routed turn has an attachment arm too, so a file with no
    // words is a turn — the Chat surface's "Add to Chat" must not be judged
    // empty just because it never names a session.
    let unpin = serde_json::json!({
        "chat_session_id": sid, "from_user": root, "text": "",
        "message_id": "msg-cap-1",
        "attached_items": [{ "type": "attached_file", "name": "src/main.rs", "absPath": "E:/work/demo/src/main.rs" }],
    });
    let (code, body) = post(&unpin);
    assert_eq!(code, 200, "an unpinned attachment-only send is a turn: {code} {body}");
    let row = messages(http, &token, &sid)["messages"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .rev()
        .find(|r| r["meta"]["message_id"].as_str() == Some("msg-cap-1"))
        .expect("an unpinned attachment-only send files its head row")
        .clone();
    let raw = serde_json::to_string(&row["meta"]["exec_attachments"]).unwrap();
    assert!(raw.contains("src/main.rs"), "the routed row keeps the display name: {raw}");
    assert!(!raw.contains("E:/work/demo") && !raw.contains("absPath"), "no path rides along: {raw}");

    // (b) An entry the Runtime would discard: refused at the boundary on
    // either path instead of becoming a label for a file nobody got.
    let bogus = serde_json::json!({
        "chat_session_id": sid, "from_user": root, "text": "look at this",
        "message_id": "msg-cap-2",
        "attached_items": [{ "type": "upload", "filename": "notes.md" }],
    });
    let (code, body) = post(&bogus);
    assert_eq!(code, 400, "an unreadable attached_item is a 400 unpinned too: {code} {body}");
    let after = messages(http, &token, &sid);
    assert_eq!(
        after["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["body"].as_str() == Some("look at this"))
            .count(),
        0,
        "a rejected send writes no head row"
    );

    // (c) A valid item with no displayable name yields no label, so with no
    // text either there is nothing left to file.
    let nameless = serde_json::json!({
        "chat_session_id": sid, "from_user": root, "text": "",
        "message_id": "msg-cap-3", "for_execution_session": pin,
        "attached_items": [{ "type": "file_upload", "documentId": "d", "filename": "", "format": "md", "sizeBytes": 1 }],
    });
    let (code, body) = post(&nameless);
    assert_eq!(code, 400, "a label-less, text-less send is not content: {code} {body}");
}
