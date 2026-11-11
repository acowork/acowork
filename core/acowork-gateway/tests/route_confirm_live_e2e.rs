//! Live-chain companion to `route_confirm_e2e.rs`.
//!
//! The fake-node test in that file proves the Gateway's derivation of the
//! card-answer coordinate over the real wire. It deliberately hand-builds the
//! card payload and never spawns a Runtime, so a drift in the router's OWN
//! card write (`emit_session_choice` / `session_choice_meta`), in the parked
//! turn's registry key, or in the wake-and-deliver path leaves it green.
//!
//! This file boots the real Runtime binary against a scripted
//! OpenAI-compatible provider and walks the whole defect as the Desktop sees
//! it:
//!
//! 1. message #1 → the scripted `exec_session_create` act opens an execution
//!    session (the candidate the card will later name — discovered from the
//!    router's own decision log, `{work_dir}/logs/chat-router-decisions.jsonl`);
//! 2. message #2 → the scripted `exec_session_route_confirm` act makes the
//!    REAL router write the `confirm_continue` card row and park the turn on
//!    the REAL registry;
//! 3. the click's two HTTP calls (answer lookup, then re-delivery with the
//!    chosen `target_session`) wake the parked turn, and the message lands in
//!    the execution session — proving the coordinate the router registered is
//!    byte-equal to the one the Gateway re-derives, through the real card row;
//! 4. the card projects as answered.
//!
//! Helpers mirror `route_confirm_e2e.rs` / `chat_sessions_e2e.rs`: each e2e
//! binary in this suite owns its boot code (the established convention).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
/// The fake node's identity + credential (see `boot_multi_user`).
const NODE_ID: &str = "e2e-node-live";
const NODE_TOKEN: &str = "e2e-node-token-0123456789";

const CHAT_TEXT_1: &str = "把登录页的按钮改成蓝色";
const CHAT_TEXT_2: &str = "继续那个任务，把边框也改了";

// ── process boot (mirrors route_confirm_e2e.rs) ───────────────────────────

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

fn binary(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("current exe");
    let dir = exe.parent().and_then(|p| p.parent()).expect("target/debug dir");
    let name = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let candidate = dir.join(&name);
    if candidate.exists() {
        candidate
    } else {
        dir.parent().expect("target root").join(&name)
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
        "acowork-test-live-{tag}-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(home.join("config")).expect("create home");
    home
}

/// Multi-user gateway with a bootstrap admin — same shape as the other
/// e2e boots.
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

    // The Gateway's HTTP channel authenticates agent calls with the
    // spawning node's token (`X-ACowork-Node-Token`, loaded from
    // `{data_dir}/node_tokens.json` at boot). There is no node binary
    // here to enroll, so the credential is seeded directly — the same
    // shape `NodeTokenStore::upsert` persists.
    fs::create_dir_all(home.join("data")).expect("create data dir");
    fs::write(
        home.join("data").join("node_tokens.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            NODE_ID: {
                "node_id": NODE_ID,
                "token": NODE_TOKEN,
                "created_at": "2026-01-01T00:00:00Z"
            }
        }))
        .unwrap(),
    )
    .expect("write node_tokens.json");

    let mut cmd = Command::new(binary("acowork-gateway"));
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
    let token = json_parse(&body)["access_token"].as_str().expect("access_token").to_string();
    let (code, body, _) = http_get_auth(http, "/api/auth/me", &token);
    assert_eq!(code, 200, "me; body:\n{body}");
    let who = json_parse(&body)["user_id"].as_str().expect("user_id").to_string();
    (gw, token, who)
}

// ── raw HTTP (no client dependency), mirroring route_confirm_e2e.rs ───────

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
    (status, head, body_b)
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

fn body_str(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn json_parse(body: &str) -> serde_json::Value {
    serde_json::from_str(body).unwrap_or_else(|e| panic!("invalid json {e}: {body}"))
}

fn json_parse_bytes(body: &[u8]) -> serde_json::Value {
    json_parse(&body_str(body))
}

fn messages(port: u16, token: &str, sid: &str) -> serde_json::Value {
    let (code, body, raw) = http_get_auth(port, &format!("/api/chat-sessions/{sid}/messages"), token);
    assert_eq!(code, 200, "messages; body:\n{body}");
    json_parse_bytes(&raw)
}

fn default_pair(port: u16, token: &str, agent: &str) -> serde_json::Value {
    let (code, body, raw) = http_get_auth(port, &format!("/api/chat-sessions/default?pair={agent}"), token);
    assert_eq!(code, 200, "default pair; body:\n{body}");
    json_parse_bytes(&raw)
}

fn wait_for(what: &str, deadline: Duration, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + deadline;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(100));
    }
}

fn wait_for_value(what: &str, deadline: Duration, f: impl Fn() -> Option<String>) -> String {
    let end = Instant::now() + deadline;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(100));
    }
}

// ── the fake node's retained inventory ───────────────────────────────────

/// Publish the `InstalledAgentInfo` + online `AgentStatus` records a real
/// `acowork-node` retains for each agent, so the Gateway holds a routable
/// instance. The client is returned and must stay alive for the test.
fn publish_retained_inventory(
    mqtt_port: u16,
    instance: &str,
    install_path: &str,
) -> rumqttc::AsyncClient {
    let instance = instance.to_string();
    let install_path = install_path.to_string();
    let manifest_toml = r#"
agent_id = "com.e2e.live"
version = "1.0.0"
name = "Live Router"
description = "e2e"
author = "e2e"
runtime_version = "0.1.0"
"#;
    let mut opts = rumqttc::MqttOptions::new(
        format!("e2e-inventory-{}-{}", std::process::id(), instance),
        "127.0.0.1",
        mqtt_port,
    );
    opts.set_keep_alive(Duration::from_secs(2));
    let (client, eventloop) = rumqttc::AsyncClient::new(opts, 10);
    let publisher = client.clone();
    // The broker needs the connection established before publishes settle;
    // drive the eventloop on a dedicated thread, current-thread runtime.
    std::thread::spawn(move || {
        let client = publisher;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("inventory runtime");
        rt.block_on(async move {
            tokio::spawn(async move {
                let mut eventloop = eventloop;
                loop {
                    match eventloop.poll().await {
                        Ok(_) => {}
                        Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                    }
                }
            });
            // Let the CONNECT handshake settle before the publishes.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let installed = acowork_core::mqtt_proto::DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::InstalledAgentInfo(
                    acowork_core::mqtt_proto::InstalledAgentInfo {
                        agent_id: "com.e2e.live".to_string(),
                        instance_id: instance.to_string(),
                        version: "1.0.0".to_string(),
                        name: "Live Router".to_string(),
                        install_path: install_path.to_string(),
                        manifest_toml: manifest_toml.to_string(),
                        identity: Some(acowork_core::mqtt_proto::AgentIdentity {
                            name: "Live Router".to_string(),
                            avatar: String::new(),
                            builtin_avatar: String::new(),
                        }),
                    },
                )),
            };
            client
                .publish(
                    format!("acowork/nodes/{NODE_ID}/agents/{instance}/installed"),
                    rumqttc::QoS::AtLeastOnce,
                    true,
                    prost::Message::encode_to_vec(&installed),
                )
                .await
                .expect("publish installed");
            let online = acowork_core::mqtt_proto::DataEnvelope {
                version: 1,
                payload: Some(acowork_core::mqtt_proto::data_envelope::Payload::AgentStatus(
                    acowork_core::mqtt_proto::AgentStatus {
                        agent_id: "com.e2e.live".to_string(),
                        instance_id: instance.to_string(),
                        node_id: NODE_ID.to_string(),
                        online: true,
                        state: 0,
                        detail: String::new(),
                    },
                )),
            };
            client
                .publish(
                    format!("acowork/agents/{instance}/status"),
                    rumqttc::QoS::AtLeastOnce,
                    true,
                    prost::Message::encode_to_vec(&online),
                )
                .await
                .expect("publish status");
            // Keep this runtime alive for the retained publishes' delivery.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
    });
    client
}

/// Kill the runtime process tree whatever way the test exits.
struct Proc(Child);
impl Drop for Proc {
    fn drop(&mut self) {
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
            .output();
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

// ── scripted OpenAI-compatible provider ────────────────────────────────────

/// One scripted assistant message, returned verbatim as the `message` of a
/// non-streaming `chat.completion`.
fn tool_call(name: &str, args: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": format!("call-{}", name),
            "type": "function",
            "function": { "name": name, "arguments": args.to_string() }
        }]
    })
}

/// Spawn a one-thread HTTP server that answers `*/chat/completions` from
/// `script` in order, and a plain-text stop once the script is drained (a
/// stray extra call then fails loudly in the assertions instead of hanging
/// the test). Returns the listener port and a shared hit counter.
fn fake_provider_server(
    script: Arc<Mutex<Vec<serde_json::Value>>>,
    hits: Arc<AtomicU64>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake provider");
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
            let mut buf = [0u8; 65536];
            let mut raw = Vec::new();
            loop {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        raw.extend_from_slice(&buf[..n]);
                        if raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let head = String::from_utf8_lossy(&raw).into_owned();
            let content_len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            let body_start = raw
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .map(|i| i + 4)
                .unwrap_or(0);
            while raw.len() < body_start + content_len {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => raw.extend_from_slice(&buf[..n]),
                }
            }
            hits.fetch_add(1, Ordering::SeqCst);
            let is_models = head.starts_with("GET ") && head.contains("/models");
            let resp_body = if is_models {
                serde_json::json!({ "data": [{ "id": "e2e-chat" }] }).to_string()
            } else {
                // The queue is drained back-to-front, so the script is
                // stored reversed.
                let next = script.lock().unwrap().pop();
                match next {
                    Some(message) => serde_json::json!({
                        "id": "e2e", "object": "chat.completion", "created": 0,
                        "model": "e2e-chat",
                        "choices": [{
                            "index": 0, "message": message,
                            "finish_reason": if message.get("tool_calls").is_some() { "tool_calls" } else { "stop" }
                        }],
                        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                    })
                    .to_string(),
                    None => serde_json::json!({
                        "id": "e2e", "object": "chat.completion", "created": 0,
                        "model": "e2e-chat",
                        "choices": [{
                            "index": 0,
                            "message": { "role": "assistant", "content": "script drained" },
                            "finish_reason": "stop"
                        }],
                        "usage": { "prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15 }
                    })
                    .to_string(),
                }
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            let _ = stream.write_all(resp.as_bytes());
            let _ = stream.flush();
        }
    });
    port
}

// ── the chain ──────────────────────────────────────────────────────────────

#[test]
fn the_real_router_turn_parks_on_its_own_card_and_the_click_wakes_it() {
    let (gw, token, root) = boot_multi_user("live");
    let http = gw.http_port;

    // ── 1. scripted provider: create, then route_confirm (stored reversed).
    let script = Arc::new(Mutex::new(vec![
        tool_call(
            "exec_session_route_confirm",
            serde_json::json!({ "target": "S1", "why": "same task, one more change" }),
        ),
        tool_call(
            "exec_session_create",
            serde_json::json!({ "background": "e2e brief: the login page task" }),
        ),
    ]));
    let hits = Arc::new(AtomicU64::new(0));
    let provider_port = fake_provider_server(Arc::clone(&script), Arc::clone(&hits));

    // ── 2. register it (custom = key-less, OpenAI-compatible) ─────────────
    // The vault starts locked; provider accounts are stored in it, and the
    // global-resource republish that the Runtime's provider pull waits on is
    // gated on vault readiness. Unlock through the real HTTP path first.
    let (code, body, _) = http_post_json_auth(
        http,
        "/api/vault/unlock",
        &serde_json::to_vec(&serde_json::json!({ "password": "e2e-vault" })).unwrap(),
        &token,
    );
    assert_eq!(code, 200, "vault unlock: {body}");

    let (code, body, _) = http_post_json_auth(
        http,
        "/api/providers",
        &serde_json::to_vec(&serde_json::json!({
            "provider": "e2e-fake",
            "custom": true,
            "base_url": format!("http://127.0.0.1:{provider_port}/v1"),
            "models": ["e2e-chat"],
        }))
        .unwrap(),
        &token,
    );
    assert_eq!(code, 201, "provider register: {body}");

    // ── 3. the agent package + the real Runtime binary ────────────────────
    let package_dir = temp_home("pkg");
    fs::write(
        package_dir.join("manifest.toml"),
        r#"
agent_id = "com.e2e.live"
version = "1.0.0"
name = "Live Router"
description = "e2e"
author = "e2e"
runtime_version = "0.1.0"
"#,
    )
    .expect("write manifest");
    let instance = uuid::Uuid::new_v4().to_string();
    // The Gateway's install table is fed by the NODE's retained publishes,
    // not by this test: same `InstalledAgentInfo` / `AgentStatus` shape
    // `acowork-node` makes, so the instance is routable without a node
    // binary. (The Runtime itself is spawned below with this instance id.)
    let _inventory = publish_retained_inventory(
        gw.mqtt_port,
        &instance,
        &package_dir.to_string_lossy(),
    );
    wait_for(
        "the instance to aggregate into the install table",
        Duration::from_secs(20),
        || {
            let (code, _, raw) = http_get_auth(http, "/api/agents", &token);
            code == 200 && body_str(&raw).contains(&instance)
        },
    );

    let work = temp_home("work");
    let runtime = Proc(Command::new(binary("acowork-runtime"))
        .args([
            "--agent-id", "com.e2e.live",
            "--agent-instance-id", &instance,
            "--package-path", &package_dir.to_string_lossy(),
            "--work-dir", &work.to_string_lossy(),
            "--gateway-host", "127.0.0.1",
            "--gateway-http-port", &http.to_string(),
            "--mqtt-port", &gw.mqtt_port.to_string(),
            "--node-id", NODE_ID,
            // The node token doubles as the broker CONNECT password and
            // the `X-ACowork-Node-Token` HTTP credential (ADR-055 Phase 5a).
            "--mqtt-password", NODE_TOKEN,
            "--log-level", "warn",
        ])
        // The port helper (`acowork_core::defaults::gateway_http_port`)
        // reads the environment, not `RuntimeConfig` — the CLI flag alone
        // would leave every call site on the default 19876.
        .env("ACOWORK_GATEWAY_HTTP_PORT", http.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn acowork-runtime"));

    let pair = default_pair(http, &token, &instance);
    let sid = pair["session_id"].as_str().expect("session_id").to_string();

    // ── 4. message #1: the scripted create act opens the exec session ─────
    let (code, body, _) = http_post_json_auth(
        http,
        "/api/intents/chat-message",
        &serde_json::to_vec(&serde_json::json!({
            "chat_session_id": sid,
            "from_user": root,
            "text": CHAT_TEXT_1,
            "message_id": "live-m1",
        }))
        .unwrap(),
        &token,
    );
    assert_eq!(code, 200, "dispatch #1: {body}");

    // The exec session id is minted inside the Runtime; the router's own
    // decision log is where a reviewer (and this test) learns the target.
    let decisions = work.join("logs").join("chat-router-decisions.jsonl");
    let exec = wait_for_value(
        "the create decision with a target session",
        Duration::from_secs(120),
        || {
            let text = fs::read_to_string(&decisions).unwrap_or_default();
            text.lines().find_map(|line| {
                let rec: serde_json::Value = serde_json::from_str(line).ok()?;
                (rec["decision"].as_str()? == "exec_session_create" && rec["outcome"].as_str()? == "ok")
                    .then(|| rec["target_session_id"].as_str().map(str::to_string))
                    .flatten()
            })
        },
    );

    // ── 5. message #2: the REAL router writes the card and parks ──────────
    let (code, body, _) = http_post_json_auth(
        http,
        "/api/intents/chat-message",
        &serde_json::to_vec(&serde_json::json!({
            "chat_session_id": sid,
            "from_user": root,
            "text": CHAT_TEXT_2,
            "message_id": "live-m2",
        }))
        .unwrap(),
        &token,
    );
    assert_eq!(code, 200, "dispatch #2: {body}");

    // The card row, written by the router's own `emit_session_choice`: it
    // must thread under m2 and name the session the create turn opened.
    let mut asked = String::new();
    let exec_for_assert = exec.clone();
    wait_for("the router's card row", Duration::from_secs(120), || {
        let page = messages(http, &token, &sid);
        let Some(rows) = page["messages"].as_array() else { return false };
        let Some(row) = rows.iter().find(|r| {
            r["meta"]["notice_type"].as_str() == Some("confirm_continue")
                && r["meta"]["in_reply_to"].as_str() == Some("live-m2")
        }) else {
            return false;
        };
        asked = row["meta"]["message_id"].as_str().unwrap_or_default().to_string();
        !asked.is_empty()
            && row["kind"].as_str() == Some("system_notice")
            && row["meta"]["options"]
                .as_array()
                .map(|o| {
                    o.iter()
                        .any(|x| x["session_id"].as_str() == Some(exec_for_assert.as_str()))
                })
                .unwrap_or(false)
    });
    assert!(
        hits.load(Ordering::SeqCst) >= 2,
        "the fake provider was called {} times — the router turns did not run through the provider transport",
        hits.load(Ordering::SeqCst)
    );

    // ── 6. the click: lookup, then re-delivery with the chosen session ────
    let (code, body, raw) = http_get_auth(
        http,
        &format!("/api/chat-sessions/{sid}/messages/{asked}/answer"),
        &token,
    );
    assert_eq!(code, 200, "answer lookup: {body}");
    let facts = json_parse_bytes(&raw);
    assert_eq!(facts["text"].as_str(), Some(CHAT_TEXT_2));
    assert_eq!(
        facts["message_id"].as_str(),
        Some("live-m2"),
        "the re-delivery stays one row"
    );

    let (code, body, _) = http_post_json_auth(
        http,
        "/api/intents/chat-message",
        &serde_json::to_vec(&serde_json::json!({
            "chat_session_id": sid,
            "from_user": root,
            "text": CHAT_TEXT_2,
            "message_id": "live-m2",
            "target_session": exec,
        }))
        .unwrap(),
        &token,
    );
    assert_eq!(code, 200, "click re-delivery: {body}");

    // ── 7. the parked turn woke and delivered into the exec session ───────
    // The assertion the fake-node test cannot make: only a coordinate that
    // matches the router's OWN registry key wakes the turn, and only the real
    // wake path opens a round against the human's row. The round reference
    // row (`in_reply_to = live-m2`, `agent_session_id` = the session the
    // create turn opened) is the durable fact the Desktop renders the
    // execution block from — a second message inside the exec session lives
    // in the Runtime's own store, not the user service's.
    let exec2 = exec.clone();
    wait_for(
        "the continue click's execution round under live-m2",
        Duration::from_secs(120),
        || {
            messages(http, &token, &sid)["messages"]
                .as_array()
                .map(|rows| {
                    rows.iter().any(|r| {
                        r["kind"].as_str() == Some("execution_round")
                            && r["meta"]["in_reply_to"].as_str() == Some("live-m2")
                            && r["meta"]["agent_session_id"].as_str() == Some(exec2.as_str())
                    })
                })
                .unwrap_or(false)
        },
    );

    // ── 8. the card projects as answered ──────────────────────────────────
    let asked2 = asked.clone();
    wait_for("the card to project as answered", Duration::from_secs(30), || {
        messages(http, &token, &sid)["messages"]
            .as_array()
            .map(|rows| {
                rows.iter().any(|r| {
                    r["meta"]["message_id"].as_str() == Some(asked2.as_str())
                        && r["delivery"]["question"]["answered"].as_bool() == Some(true)
                })
            })
            .unwrap_or(false)
    });

    drop(runtime);
    drop(gw);
}
