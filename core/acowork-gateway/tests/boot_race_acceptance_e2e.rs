//! End-to-end acceptance — the 2026-11-24 boot race, real processes.
//!
//! The unit / integration suites drive the state machines in-process.
//! This file spawns the ACTUAL Gateway binary (isolated temp home,
//! ephemeral ports, child killed on drop) and reproduces the incident
//! from the outside, exactly as an operator would see it:
//!
//!   1. The MQTT broker port is held by another process when the
//!      Gateway starts (the compiled-but-not-yet-listening broker of
//!      the incident). The Gateway must stay alive and BOOTING with
//!      visible progress — not discard its session and go silent.
//!   2. The port is released. WITHOUT restarting anything, the Gateway
//!      must reach READY on its own and seed the retained global
//!      resources that every Runtime depends on.
//!   3. `mqtt.enabled = false` must reach READY (a disabled feature is
//!      not a boot blocker).
//!
//! Nothing here touches the running production stack: every port is
//! ephemeral, every home is a fresh temp dir, and the child is killed
//! via its own process tree on drop.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BOOT_TIMEOUT: Duration = Duration::from_secs(30);
const READY_TIMEOUT: Duration = Duration::from_secs(60);

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

fn gateway_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_acowork-gateway"))
}

fn temp_home(tag: &str) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "acowork-boot-acceptance-{}-{}",
        tag,
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(home.join("config")).expect("create home");
    home
}

struct Gateway {
    child: std::process::Child,
    http_port: u16,
    stderr: Arc<Mutex<String>>,
}

impl Gateway {
    fn stderr(&self) -> String {
        self.stderr.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn assert_alive(&mut self, context: &str) {
        if let Some(status) = self.child.try_wait().expect("try_wait") {
            panic!(
                "gateway exited ({status}) {context}; stderr:\n{}",
                self.stderr()
            );
        }
    }

    fn wait_until_serving(&mut self) {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while Instant::now() < deadline {
            self.assert_alive("before serving");
            if std::net::TcpStream::connect(("127.0.0.1", self.http_port)).is_ok() {
                self.assert_alive("right after binding");
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!(
            "gateway did not serve on port {} within {BOOT_TIMEOUT:?}; stderr:\n{}",
            self.http_port,
            self.stderr()
        );
    }

    /// GET /api/bootstrap → the `phase` string (public pre-login route).
    fn bootstrap_phase(&self) -> Option<String> {
        let body = http_get(self.http_port, "/api/bootstrap")?;
        json_string_field(&body, "phase").map(str::to_owned)
    }

    fn wait_phase(&mut self, want: &str, budget: Duration, context: &str) {
        let deadline = Instant::now() + budget;
        let mut last = String::new();
        while Instant::now() < deadline {
            self.assert_alive(context);
            if let Some(phase) = self.bootstrap_phase() {
                if phase == want {
                    return;
                }
                last = phase;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!(
            "phase never reached {want} within {budget:?} (last: {last}) {context}; stderr:\n{}",
            self.stderr()
        );
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        #[cfg(windows)]
        let _ = Command::new("taskkill")
            .args(["/PID", &self.child.id().to_string(), "/T", "/F"])
            .output();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn spawn(home: &Path, http_port: u16, mqtt_port: u16) -> Gateway {
    let mut cmd = Command::new(gateway_binary());
    cmd.arg("--daemon")
        .arg("--home")
        .arg(home)
        .arg("--addr")
        .arg(format!("127.0.0.1:{http_port}"))
        .arg("--mqtt-addr")
        .arg(format!("127.0.0.1:{mqtt_port}"))
        .arg("--no-spawn-local-node")
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
    Gateway {
        child,
        http_port,
        stderr,
    }
}

/// Minimal isolated config: dev_mode auto-unlocks the vault; every
/// supervised side service is off so the acceptance Gateway owns only
/// its HTTP + MQTT ports (plus the optional embed attach, which is a
/// designed read-only reuse path).
fn write_config(home: &Path, mqtt_enabled: bool) {
    let home_str = home.to_string_lossy().replace('\\', "/");
    let body = format!(
        r#"dev_mode = true
vault_dir = "{home_str}/data/vault"
packages_dir = "{home_str}/config/packages"
data_dir = "{home_str}/data"

[mqtt]
enabled = {mqtt_enabled}

[user]
enabled = false

[pm]
enabled = false

[doc]
enabled = false

[memory]
enabled = false

[local_node]
enabled = false

[relay]
enabled = false
"#
    );
    std::fs::write(home.join("config").join("gateway.toml"), body).expect("write gateway.toml");
}

/// Minimal raw-socket HTTP/1.1 GET → body (None on connect/parse failure).
fn http_get(port: u16, path: &str) -> Option<String> {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .ok()?;
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw = String::new();
    stream.read_to_string(&mut raw).ok()?;
    let body = raw.split("\r\n\r\n").nth(1)?;
    Some(body.to_owned())
}

fn json_string_field<'a>(body: &'a str, field: &str) -> Option<&'a str> {
    let needle = format!("\"{field}\":");
    let start = body.find(&needle)? + needle.len();
    let rest = body[start..].trim_start();
    let s = rest.strip_prefix('"')?;
    let end = s.find('"')?;
    Some(&s[..end])
}

/// The incident itself: broker port held at boot, released later.
/// The Gateway must recover to READY with NO restart, and the retained
/// global resources must actually land on the broker.
#[test]
fn boot_race_broker_port_held_then_released_reaches_ready_without_restart() {
    let http_port = free_port();
    let mqtt_port = free_port();
    let home = temp_home("boot-race");
    write_config(&home, true);

    // Another process owns the broker port — silent holder, no accept.
    // rumqttd's bind fails; the supervisor must report Retrying and keep
    // trying, and the session must keep retrying behind it.
    let holder = std::net::TcpListener::bind(("127.0.0.1", mqtt_port))
        .expect("hold the broker port");

    let mut gw = spawn(&home, http_port, mqtt_port);
    gw.wait_until_serving();
    let pid = gw.pid();

    // While the port is held the phase must be BOOTING (mqtt is
    // Required and cannot connect), and the process must stay alive.
    gw.wait_phase("BOOTING", Duration::from_secs(15), "while broker port is held");
    let observe_until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < observe_until {
        gw.assert_alive("during the outage");
        assert_eq!(
            gw.bootstrap_phase().as_deref(),
            Some("BOOTING"),
            "phase must not fabricate READY while the broker port is held"
        );
        std::thread::sleep(Duration::from_millis(300));
    }

    // The port frees up. Nothing is restarted — the same process must
    // converge on its own (the old wiring dropped the session forever).
    drop(holder);
    gw.wait_phase("READY", READY_TIMEOUT, "after the broker port was released");
    assert_eq!(gw.pid(), pid, "the Gateway process must not have been replaced");
    gw.assert_alive("after reaching READY");

    // The downstream chain the incident killed: retained global
    // resources must be on the broker for every Runtime to consume.
    let retained = retained_global_payloads(mqtt_port, Duration::from_secs(15));
    assert!(
        !retained.is_empty(),
        "no retained acowork/global/* payload after READY — the publisher chain is dead"
    );
    assert!(
        retained.iter().any(|(topic, _)| topic == "acowork/global/bootstrap"),
        "retained bootstrap snapshot missing; got: {:?}",
        retained.iter().map(|(t, _)| t).collect::<Vec<_>>()
    );
}

/// Matrix #5, real wiring: a configuration-disabled MQTT subsystem must
/// not strand the Gateway in BOOTING.
#[test]
fn mqtt_disabled_reaches_ready() {
    let http_port = free_port();
    let mqtt_port = free_port();
    let home = temp_home("mqtt-disabled");
    write_config(&home, false);

    let mut gw = spawn(&home, http_port, mqtt_port);
    gw.wait_until_serving();
    gw.wait_phase("READY", Duration::from_secs(30), "with mqtt disabled");
}

/// Subscribe to `acowork/global/#` on a fresh broker and collect the
/// retained payloads that arrive right after SUBACK.
fn retained_global_payloads(port: u16, budget: Duration) -> Vec<(String, Vec<u8>)> {
    use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    rt.block_on(async move {
        let mut opts = MqttOptions::new("acceptance:retained-probe", "127.0.0.1", port);
        opts.set_keep_alive(Duration::from_secs(5));
        let (client, mut eventloop) = AsyncClient::new(opts, 32);
        client
            .subscribe("acowork/global/#", QoS::AtLeastOnce)
            .await
            .expect("subscribe");
        let mut out = Vec::new();
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, eventloop.poll()).await {
                Ok(Ok(Event::Incoming(Incoming::Publish(p)))) => {
                    out.push((p.topic.clone(), p.payload.to_vec()));
                }
                Ok(Ok(_)) => {}
                Ok(Err(e)) => panic!("retained probe connection failed: {e}"),
                Err(_) => break,
            }
        }
        out
    })
}
