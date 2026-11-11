//! `exec_session_route_confirm` — process-level E2E of the card-answer
//! re-delivery contract (ADR-106 D6 rev. 2026-11-12).
//!
//! The defect this covers was invisible to every in-process test: the parked
//! router turn registers its card under the `chat_row_id` of the delivery it
//! came in on, and the click's re-delivery must come back carrying **the same
//! coordinate** — derived by the Gateway, through the durable card row, across
//! two HTTP round-trips and one MQTT hop. The unit tests build both sides of
//! that envelope themselves, so a drift in the Gateway's derivation leaves
//! every one of them green while every real click silently dies on the ladder.
//!
//! This file boots the real Gateway binary (real embedded broker, real user
//! service) and walks the exact chain the Desktop walks:
//!
//! 1. a human message dispatches over MQTT as a `user_message` envelope whose
//!    `chat_row_id` is the durable row the Gateway just wrote;
//! 2. the Runtime's card write (the same `POST /api/intents/chat-append`
//!    payload the router's `emit_session_choice` produces) lands as an
//!    answerable `confirm_continue` notice threaded under that row;
//! 3. `GET …/answer` returns the original utterance verbatim, keyed to it;
//! 4. the click re-delivers under the SAME `message_id` with the chosen
//!    address (`target_session` / `force_new`), and the envelope that reaches
//!    the agent's control topic carries `chat_row_id` byte-equal to the one
//!    the first envelope carried — which is what the real
//!    `ChatSessionManager::claim_card_answer` matches the parked card
//!    against, so the click wakes the parked turn instead of opening a second
//!    router turn beside a live card.
//!
//! The agent's half of the wire is played by a fake node: the same retained
//! `InstalledAgentInfo` / `AgentStatus` publishes `acowork-node` makes, so the
//! Gateway's install table and liveness registry hold a routable instance
//! without spawning a Runtime binary (the provider turn itself is out of
//! scope here — the router's model-call behaviour is covered in-process).

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::sleep;
use std::time::{Duration, Instant};

use acowork_core::mqtt_proto::{
    self, data_envelope, AgentIdentity, AgentStatus, DataEnvelope, InstalledAgentInfo,
};
use acowork_runtime::agent::chat_router::{ChatSessionManager, RouteAnswer};
use acowork_runtime::agent::inbound::IntentEnvelope;
use prost::Message as _;
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
/// The execution session id the click chooses to continue. The Gateway never
/// resolves it (addressing is the Runtime's business); it must travel verbatim.
const EXEC: &str = "exec-session-1";

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
        "acowork-test-routecfm-{tag}-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(home.join("config")).expect("create home");
    home
}

/// Boot a multi_user gateway with a bootstrap admin (root / s3cret123) —
/// the same shape as `chat_sessions_e2e::boot_multi_user`.
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
    let (code, body, _) = http_get_auth(http, "/api/auth/me", &token);
    assert_eq!(code, 200, "me; body:\n{body}");
    let who = json_parse(&body)["user_id"].as_str().expect("user_id").to_string();
    (gw, token, who)
}

// ── raw HTTP (no client dependency), mirroring chat_sessions_e2e ─────────

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

/// The `user_agent` pair endpoint through the Gateway proxy.
fn default_pair(port: u16, token: &str, agent: &str) -> serde_json::Value {
    let (code, body, raw) = http_get_auth(port, &format!("/api/chat-sessions/default?pair={agent}"), token);
    assert_eq!(code, 200, "default pair; body:\n{body}");
    json_parse_bytes(&raw)
}

fn wait_for(what: &str, deadline: Duration, f: impl Fn() -> bool) {
    let end = Instant::now() + deadline;
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        sleep(Duration::from_millis(100));
    }
}

// ── MQTT: the fake node's retained publishes + the agent's capture ────────

/// A live MQTT connection with every publish collected off the wire by a
/// background poller thread. Owned through [`WIRE`] because the chain
/// interleaves synchronous HTTP steps with wire waits; one test per binary
/// run owns the slot (the boot is process-global anyway).
struct MqttCapture {
    client: AsyncClient,
    rx: std::sync::mpsc::Receiver<(String, Vec<u8>)>,
}

static WIRE: OnceLock<Mutex<Option<MqttCapture>>> = OnceLock::new();

fn wire_slot() -> &'static Mutex<Option<MqttCapture>> {
    WIRE.get_or_init(|| Mutex::new(None))
}

fn put_wire(w: MqttCapture) {
    *wire_slot().lock().unwrap() = Some(w);
}

fn with_wire<R>(f: impl FnOnce(&mut MqttCapture) -> R) -> R {
    let mut guard = wire_slot().lock().unwrap();
    let w = guard.as_mut().expect("wire present");
    f(w)
}

impl MqttCapture {
    fn new(port: u16, id: &str, topics: &[&str]) -> Self {
        let mut opts = MqttOptions::new(id, "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(2));
        let (client, mut eventloop) = AsyncClient::new(opts, 20);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("capture poller runtime");
            loop {
                match rt.block_on(eventloop.poll()) {
                    Ok(Event::Incoming(Incoming::Publish(p))) => {
                        if tx.send((p.topic, p.payload.to_vec())).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => sleep(Duration::from_millis(100)),
                }
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for t in topics {
            rt.block_on(client.subscribe(*t, QoS::AtLeastOnce)).unwrap();
        }
        // Drain the SUBACKs so the broker's delivery of retained + fresh
        // publishes starts from a settled subscription.
        sleep(Duration::from_millis(200));
        Self { client, rx }
    }

    fn publish(&self, topic: &str, bytes: Vec<u8>, retain: bool) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(self.client.publish(topic, QoS::AtLeastOnce, retain, bytes))
            .unwrap();
    }

    /// The next publish on `topic` (waits up to `dur`).
    fn wait_publish(&mut self, topic: &str, dur: Duration) -> Vec<u8> {
        let deadline = Instant::now() + dur;
        loop {
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok((t, payload)) if t == topic => return payload,
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => panic!("capture poller died"),
            }
            assert!(Instant::now() < deadline, "no publish on {topic}");
        }
    }
}

/// The envelope the Gateway publishes for a delivered chat turn, projected
/// onto the Runtime's own inbound type — the exact struct the parked turn and
/// `claim_card_answer` read. The conversion mirrors
/// `control_handler::parse_control_payload` (proto field → envelope field).
fn capture_intent(topic: &str, what: &str) -> IntentEnvelope {
    let bytes = with_wire(|w| w.wait_publish(topic, Duration::from_secs(20)));
    let env = DataEnvelope::decode(bytes.as_slice())
        .unwrap_or_else(|e| panic!("DataEnvelope decode for {what}: {e}"));
    let Some(data_envelope::Payload::ControlCommand(cmd)) = env.payload else {
        panic!("control command expected for {what}");
    };
    let Some(mqtt_proto::control_command::Command::Intent(intent)) = cmd.command else {
        panic!("intent command expected for {what}");
    };
    IntentEnvelope {
        from: intent.from,
        action: intent.action,
        params: serde_json::from_str(&intent.params_json).unwrap_or(serde_json::json!({})),
        message_id: intent.message_id,
        conversation_id: intent.conversation_id,
        source_chat_session_id: intent.source_chat_session_id,
        repo: intent.repo,
        session_config_json: intent.session_config_json,
        kind: intent.kind,
        in_reply_to: intent.in_reply_to,
        status: intent.status,
        has_human: intent.has_human_participant,
        chat_attachment_ids: intent.chat_attachment_ids,
        chat_row_id: intent.chat_row_id,
        aware_users: intent.aware_users,
    }
}

// ════════════════════════════ the chain ════════════════════════════════════

/// The whole card-answer contract, process level: dispatch → card write →
/// answer lookup → click re-delivery → the real `claim_card_answer` wakes the
/// parked turn. One test because it is one chain; the two click arms
/// (continue / new task) share every step up to the re-delivery.
#[test]
fn card_answer_redispatch_carries_the_parked_turns_coordinate() {
    let (gw, token, root) = boot_multi_user("route-confirm");
    let http = gw.http_port;
    let instance = uuid::Uuid::new_v4().to_string();
    let package = "com.e2e.router";
    let node = "e2e-node-1";

    // ── 1. Fake node: the retained inventory + online status a real
    //    `acowork-node` publishes, so the Gateway holds a routable instance.
    let manifest_toml = format!(
        r#"
agent_id = "{package}"
version = "1.0.0"
name = "Router E2E"
description = "e2e"
author = "e2e"
runtime_version = "0.1.0"
"#
    );
    let installed_topic = format!("acowork/nodes/{node}/agents/{instance}/installed");
    let status_topic = format!("acowork/agents/{instance}/status");
    let control_topic = format!("acowork/agents/{instance}/sessions/control/intent");

    let wire = MqttCapture::new(
        gw.mqtt_port,
        &format!("e2e-wire-{}-{}", std::process::id(), instance),
        &[&control_topic, &status_topic],
    );
    let installed = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::InstalledAgentInfo(InstalledAgentInfo {
            agent_id: package.to_string(),
            instance_id: instance.clone(),
            version: "1.0.0".to_string(),
            name: "Router E2E".to_string(),
            install_path: String::new(),
            manifest_toml,
            identity: Some(AgentIdentity {
                name: "Router E2E".to_string(),
                avatar: String::new(),
                builtin_avatar: String::new(),
            }),
        })),
    };
    wire.publish(installed_topic.as_str(), installed.encode_to_vec(), true);
    let online = DataEnvelope {
        version: 1,
        payload: Some(data_envelope::Payload::AgentStatus(AgentStatus {
            agent_id: package.to_string(),
            instance_id: instance.clone(),
            node_id: node.to_string(),
            online: true,
            state: 0,
            detail: String::new(),
        })),
    };
    wire.publish(status_topic.as_str(), online.encode_to_vec(), true);
    put_wire(wire);

    wait_for(
        "the instance to aggregate into the install table",
        Duration::from_secs(20),
        || {
            let (code, _, raw) = http_get_auth(http, "/api/agents", &token);
            code == 200 && body_str(&raw).contains(&instance)
        },
    );

    // ── 2. The 1:1 chat pair (human + the agent instance).
    let pair = default_pair(http, &token, &instance);
    let sid = pair["session_id"].as_str().expect("session_id").to_string();

    // ── 3. The human's first message: dispatched over MQTT as a
    //    `user_message` envelope whose chat_row_id IS the durable row.
    let m1 = "m1";
    let (code, head, raw) = http_post_json_auth(
        http,
        "/api/intents/chat-message",
        &serde_json::to_vec(&serde_json::json!({
            "chat_session_id": sid, "from_user": root, "text": "继续那个任务", "message_id": m1,
        }))
        .unwrap(),
        &token,
    );
    assert_eq!(code, 200, "chat-message #1: {head}\n{}", body_str(&raw));
    let e1 = capture_intent(&control_topic, "the first delivery");
    assert_eq!(e1.action, "chat");
    assert_eq!(e1.kind, "user_message");
    assert_eq!(e1.from, root, "the envelope's `from` is the human — the card's `to`");
    assert_eq!(e1.source_chat_session_id, sid);
    assert_eq!(e1.chat_row_id, m1, "the declared coordinate is the row the Gateway wrote");

    // ── 4. The router's card write — the exact payload `emit_session_choice`
    //    produces (same meta shape as `session_choice_meta`).
    let card_id = format!("confirm_continue:{instance}:{m1}");
    let card = serde_json::json!({
        "from": instance,
        "chat_session_id": sid,
        "kind": "system_notice",
        "to": root,
        "body": "",
        "meta": {
            "message_id": card_id,
            "notice_type": "confirm_continue",
            "in_reply_to": m1,
            "options": [{
                "session_id": EXEC,
                "label": "S1",
                "status": "idle",
                "why": "the task the human just quoted",
            }],
            "allow_new_task": true,
        },
    });
    let (code, head, raw) = http_post_json_auth(
        http,
        "/api/intents/chat-append",
        &serde_json::to_vec(&card).unwrap(),
        &token,
    );
    assert_eq!(code, 200, "card write: {head}\n{}", body_str(&raw));
    let page = messages(http, &token, &sid);
    let card_row = page["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["meta"]["message_id"].as_str() == Some(card_id.as_str()))
        .expect("the card row is in the transcript");
    assert_eq!(card_row["kind"].as_str(), Some("system_notice"));
    assert_eq!(card_row["meta"]["notice_type"].as_str(), Some("confirm_continue"));

    // ── 5. The click's lookup: the service hands back the ORIGINAL words,
    //    keyed to the original row — the re-delivery's coordinates.
    let (code, head, raw) = http_get_auth(
        http,
        &format!("/api/chat-sessions/{sid}/messages/{card_id}/answer"),
        &token,
    );
    assert_eq!(code, 200, "answer lookup: {head}\n{}", body_str(&raw));
    let facts = json_parse_bytes(&raw);
    assert_eq!(facts["text"].as_str(), Some("继续那个任务"));
    assert_eq!(facts["message_id"].as_str(), Some(m1), "the re-delivery stays one row");
    assert_eq!(facts["agent_id"].as_str(), Some(instance.as_str()));

    // ── 6. The re-delivery, continue arm: same message_id + the chosen
    //    session. The envelope that arrives must carry chat_row_id == m1 —
    //    byte-equal to what the parked turn registered — or the click dies.
    let redeliver = |text: &str, mid: &str, target: Option<&str>, force_new: bool| {
        let (code, head, raw) = http_post_json_auth(
            http,
            "/api/intents/chat-message",
            &serde_json::to_vec(&serde_json::json!({
                "chat_session_id": sid,
                "from_user": root,
                "text": text,
                "message_id": mid,
                "target_session": target,
                "force_new": force_new,
            }))
            .unwrap(),
            &token,
        );
        assert_eq!(code, 200, "chat-message {mid}: {head}\n{}", body_str(&raw));
    };
    redeliver("继续那个任务", m1, Some(EXEC), false);
    let e2 = capture_intent(&control_topic, "the continue click's re-delivery");
    assert_eq!(
        e2.chat_row_id, e1.chat_row_id,
        "THE contract: the answer's re-delivery names the row the parked turn asked about"
    );
    assert_eq!(
        e2.params[acowork_core::protocol::TARGET_SESSION_PARAM].as_str(),
        Some(EXEC),
        "the chosen session travels verbatim"
    );
    assert!(
        !e2.params
            .get(acowork_core::protocol::FORCE_NEW_PARAM)
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        "a continue click carries no force_new"
    );

    // ── 7. The real claim: with a card parked on `m1` (exactly what the
    //    router's RouteConfirm arm registers), this envelope WAKES it.
    let manager = ChatSessionManager::new();
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    manager.register_card(&sid, &e1.chat_row_id, tx);
    let answer = manager
        .claim_card_answer(&e2)
        .expect("the continue click claims the parked card");
    assert_eq!(answer, RouteAnswer::Continue { session_id: EXEC.to_string() });
    assert_eq!(rx.try_recv().expect("the parked turn is woken"), answer);
    assert_eq!(manager.live_cards(), 0, "a claimed card is consumed exactly once");
    assert!(
        manager.claim_card_answer(&e2).is_none(),
        "a second claim of the same click falls to the ladder"
    );

    // ── 8. The new-task arm: force_new on the same coordinate resolves as
    //    `NewTask` (the parked turn feeds that back to the model).
    let m2 = "m2";
    redeliver("开个新任务", m2, None, false);
    let e_new = capture_intent(&control_topic, "the plain new message");
    assert_eq!(e_new.chat_row_id, m2);

    let card2_id = format!("confirm_continue:{instance}:{m2}");
    let card2 = serde_json::json!({
        "from": instance,
        "chat_session_id": sid,
        "kind": "system_notice",
        "to": root,
        "body": "",
        "meta": {
            "message_id": card2_id,
            "notice_type": "confirm_continue",
            "in_reply_to": m2,
            "options": [{ "session_id": EXEC, "label": "S1", "status": "idle", "why": "w" }],
            "allow_new_task": true,
        },
    });
    let (code, head, raw) = http_post_json_auth(
        http,
        "/api/intents/chat-append",
        &serde_json::to_vec(&card2).unwrap(),
        &token,
    );
    assert_eq!(code, 200, "card #2 write: {head}\n{}", body_str(&raw));
    let (code, head, raw) = http_get_auth(
        http,
        &format!("/api/chat-sessions/{sid}/messages/{card2_id}/answer"),
        &token,
    );
    assert_eq!(code, 200, "answer lookup #2: {head}\n{}", body_str(&raw));
    assert_eq!(json_parse_bytes(&raw)["message_id"].as_str(), Some(m2));

    redeliver("开个新任务", m2, None, true);
    let e3 = capture_intent(&control_topic, "the new-task click's re-delivery");
    assert_eq!(e3.chat_row_id, m2, "the new-task arm carries the same coordinate rule");
    assert!(e3
        .params
        .get(acowork_core::protocol::FORCE_NEW_PARAM)
        .and_then(|v| v.as_bool())
        .unwrap_or(false));
    let (tx, mut rx) = tokio::sync::oneshot::channel();
    manager.register_card(&sid, m2, tx);
    assert_eq!(manager.claim_card_answer(&e3), Some(RouteAnswer::NewTask));
    assert_eq!(rx.try_recv().expect("woken"), RouteAnswer::NewTask);

    // A card parked on a DIFFERENT row claims nothing — the click falls to the
    // ladder, which is the slow path a timed-out card lands on.
    let (tx, _rx) = tokio::sync::oneshot::channel();
    manager.register_card(&sid, "other-row", tx);
    assert!(manager.claim_card_answer(&e3).is_none());

    drop(gw);
}
