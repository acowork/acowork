//! ADR-114 — account-system lifecycle acceptance, at the **process** level.
//!
//! The route surface is pinned by in-process router tests
//! (`http::auth_api::tests`, `http::restricted_mode::tests`), and the state
//! machine by `acowork_core::account::AccountSystemState` unit tests. What
//! none of those can reach is the wiring in between: an empty `--home` tree
//! → the supervised user service → `/api/status` → the restricted-mode gate
//! → `POST /api/auth/setup` → a usable session.
//!
//! There is deliberately no "mode" left to assert on. ADR-114 removed
//! `AUTH_MODE`: the account system is **always present**, and what varies is
//! a runtime fact about the store (`uninitialized` / `needs_setup` /
//! `ready`). The network bind answers only whether the listener is reachable
//! from outside; it never decides whether accounts exist, and it never
//! decides whether a client may bootstrap them (that is the peer address).
//!
//! Assertions stay dependency-free: exit status, stderr, a listening socket,
//! the `--home` tree, and hand-rolled raw-socket HTTP. Each test gets its own
//! temp home and ephemeral ports; the [`Gateway`] guard kills the child even
//! when an assertion panics, so a failing run cannot leave a stray daemon.

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// How long a *successful* boot may take before we call it a hang. Generous:
/// the point is to catch "never binds", not to benchmark startup.
const BOOT_TIMEOUT: Duration = Duration::from_secs(30);

/// Pick a free TCP port. The listener is dropped immediately, so the port is
/// only *probably* free — hence a fresh pair per test and a retry-free design
/// (a collision surfaces as a boot failure with stderr attached).
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

/// The Gateway binary for this test. Cargo builds it and hands us the
/// exact path — this package owns the target, so there is nothing to find.
fn gateway_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_acowork-gateway"))
}

/// A running (or finished) Gateway plus its temp home. Dropping the guard
/// kills the child, which is what keeps a panic from leaking a daemon.
struct Gateway {
    child: Child,
    http_port: u16,
    /// Drained by a reader thread, so the child can never block on a full
    /// pipe and a panic can still show what it said before dying.
    stderr: Arc<Mutex<String>>,
}

impl Gateway {
    fn stderr(&self) -> String {
        self.stderr.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Wait for the HTTP port to accept a connection — proof the binary
    /// booted far enough to serve.
    fn wait_until_serving(&mut self) {
        let deadline = Instant::now() + BOOT_TIMEOUT;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!(
                    "gateway exited before serving ({status}); stderr:\n{}",
                    self.stderr()
                );
            }
            if std::net::TcpStream::connect(("127.0.0.1", self.http_port)).is_ok() {
                // One more poll: a start refusal can race the bind.
                if let Some(status) = self.child.try_wait().expect("try_wait") {
                    panic!("gateway exited right after binding ({status})");
                }
                return;
            }
            sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        panic!(
            "gateway did not serve on port {} within {BOOT_TIMEOUT:?}; stderr:\n{}",
            self.http_port,
            self.stderr()
        );
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        kill_tree(&mut self.child);
    }
}

/// Kill the child **and every process it spawned**.
///
/// `Child::kill()` is TerminateProcess on Windows: it reaps only the direct
/// child, so the services that child hosted (embed / node / doc / pm / user)
/// survive until their ADR-018 watchdog notices the Gateway is gone (5 min),
/// holding their ports and — on Windows — a lock on `acowork-*.exe` that
/// fails the next `cargo build`. `taskkill /T` reaps the tree with it.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .output();
    let _ = child.kill();
    let _ = child.wait();
}

fn temp_home(tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let home = std::env::temp_dir().join(format!(
        "acowork-test-accountstate-{tag}-{}-{unique}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    fs::create_dir_all(home.join("config")).expect("create home");
    home
}

/// Spawn the binary with a private home and private ports.
///
/// `--no-spawn-local-node` keeps the test from depending on an
/// `acowork-node` binary being present.
fn spawn(home: &Path, http_port: u16, mqtt_port: u16, extra: &[&str]) -> Gateway {
    spawn_on(
        home,
        &format!("127.0.0.1:{http_port}"),
        http_port,
        mqtt_port,
        extra,
    )
}

/// `bind` is passed separately because `--addr` takes a single value: a
/// second occurrence is a clap error, so the public-bind case cannot simply
/// append another one.
fn spawn_on(
    home: &Path,
    bind: &str,
    http_port: u16,
    mqtt_port: u16,
    extra: &[&str],
) -> Gateway {
    let mut cmd = Command::new(gateway_binary());
    cmd.arg("--daemon")
        .arg("--home")
        .arg(home)
        .arg("--addr")
        .arg(bind)
        .arg("--mqtt-addr")
        .arg(format!("127.0.0.1:{mqtt_port}"))
        .arg("--no-spawn-local-node")
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("spawn acowork-gateway");
    let stderr = Arc::new(Mutex::new(String::new()));
    if let Some(pipe) = child.stderr.take() {
        let sink = Arc::clone(&stderr);
        std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines() {
                match line {
                    Ok(line) => {
                        if let Ok(mut buf) = sink.lock() {
                            buf.push_str(&line);
                            buf.push('\n');
                        }
                    }
                    Err(_) => break,
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

/// Write `gateway.toml` and return the path to pass as `--config-path`.
fn write_config(home: &Path, body: &str) -> PathBuf {
    let path = home.join("config").join("gateway.toml");
    fs::write(&path, body).expect("write gateway.toml");
    path
}

// ── Raw-socket HTTP/1.1 (dependency-free on purpose) ───────────────────

fn http_request(
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
) -> (u16, String) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set read timeout");
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n"
    );
    if let Some(b) = body {
        req.push_str("Content-Type: application/json\r\n");
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    if let Some(t) = token {
        req.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    req.push_str("\r\n");
    if let Some(b) = body {
        req.push_str(b);
    }
    stream.write_all(req.as_bytes()).expect("send request");
    let mut raw = String::new();
    let _ = stream.read_to_string(&mut raw); // the peer closes; the timeout is a backstop
    let code = raw
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let body = raw
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default();
    (code, body)
}

fn http_get(port: u16, path: &str) -> (u16, String) {
    http_request(port, "GET", path, None, None)
}

/// Pull a string field out of a compact JSON body without a JSON crate.
/// Chunked-transfer framing (if any) sits before the object, which is why
/// this searches rather than parsing from offset 0.
fn json_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = body.find(&needle)? + needle.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Poll `/api/status` until `pred` accepts the body, or panic.
///
/// Since ADR-084 the state this endpoint carries can *lag* the listen socket:
/// `account_state` / `registration_open` come from the snapshot the
/// supervisor reads off the supervised user service's `/health`, and that
/// service is spawned asynchronously after the Gateway binds. Waiting for the
/// socket is therefore no longer enough to observe a settled state.
fn wait_for_status(
    gw: &Gateway,
    port: u16,
    pred: impl Fn(&str) -> bool,
    what: &str,
) -> String {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut last = String::new();
    while Instant::now() < deadline {
        let (code, body) = http_get(port, "/api/status");
        if code == 200 && pred(&body) {
            return body;
        }
        last = body;
        sleep(Duration::from_millis(100));
    }
    panic!(
        "{what} was not observed within {BOOT_TIMEOUT:?}; last /api/status body:\n{last}\n--- gateway stderr ---\n{}",
        gw.stderr()
    );
}

/// `POST` until the Gateway stops answering 503 "user service not ready".
///
/// `/api/status` deliberately fail-opens to `ready` before the first health
/// snapshot arrives, so at the moment a test considers the Gateway booted the
/// proxy may still not have published `user_process`. The 503 that follows is
/// a correct and transient answer — it carries `Retry-After: 2`, the same
/// contract the Desktop's `with503Retry` absorbs. A test that asserts on the
/// first attempt is asserting on that race rather than on behaviour, and it
/// loses whenever the runner is busy.
fn post_until_ready(gw: &Gateway, port: u16, path: &str, json: &str) -> (u16, String) {
    request_until_ready(gw, port, "POST", path, Some(json), None)
}

/// The same window on the read side: the restricted-mode gate is a decision
/// the Gateway can make, but a 503 from the not-yet-published user service
/// arrives first and is not an answer about the gate.
fn request_until_ready(
    gw: &Gateway,
    port: u16,
    method: &str,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
) -> (u16, String) {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut last = (0u16, String::new());
    while Instant::now() < deadline {
        last = http_request(port, method, path, body, token);
        if last.0 != 503 {
            return last;
        }
        sleep(Duration::from_millis(100));
    }
    panic!(
        "{method} {path} kept answering {} within {:?}; last body:
{}
--- gateway stderr ---
{}",
        last.0,
        BOOT_TIMEOUT,
        last.1,
        gw.stderr()
    );
}

/// The snapshot may turn `ready` before the Gateway has published the user
/// service's public key to its verifier, so a freshly minted token can still
/// see a transient 503. Retry rather than asserting on a race.
fn wait_for_authenticated_api(port: u16, token: &str) -> (u16, String) {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    let mut last = (0, String::new());
    while Instant::now() < deadline {
        last = http_request(port, "GET", "/api/users", None, Some(token));
        if last.0 == 200 {
            return last;
        }
        sleep(Duration::from_millis(100));
    }
    panic!(
        "the authenticated API never opened after setup; last: {} {}\nbody:\n{}",
        last.0, "GET /api/users", last.1
    );
}

/// Wait for a file to appear (the user service writes its store at its own
/// boot, which the Gateway does not serialise against).
///
/// The Gateway handle is part of the signature on purpose: this file is written
/// by a *supervised child*, so when it never appears the reason is in the
/// Gateway's own log — a failed spawn, a health probe that never passed, a
/// refused bind. Without it the assertion says only "a file is missing" on the
/// one machine that cannot be reproduced locally, which is a failure nobody can
/// act on.
fn wait_for_file(gw: &Gateway, path: &Path, what: &str) {
    let deadline = Instant::now() + BOOT_TIMEOUT;
    while !path.exists() && Instant::now() < deadline {
        sleep(Duration::from_millis(100));
    }
    assert!(
        path.exists(),
        "{what} did not appear at {}
--- gateway stderr ---
{}",
        path.display(),
        gw.stderr()
    );
}

fn account_store(home: &Path) -> PathBuf {
    home.join("acowork-user").join("accounts.json")
}

fn is_first_boot_status(body: &str) -> bool {
    body.contains("\"account_state\":\"needs_setup\"")
        || body.contains("\"account_state\":\"uninitialized\"")
}

// ── First-boot lifecycle (ADR-114) ─────────────────────────────────────

/// A fresh install seeds a passwordless `admin`, boots into the restricted
/// gate, and — this is the ADR-114 change — can be **finished over HTTP from
/// the same machine**. No SSH, no TTY, no manual toml edit. A second setup
/// must be refused so the first administrator's password cannot be replaced
/// by whoever arrives next.
#[test]
fn a_fresh_install_is_restricted_until_setup_completes_over_http() {
    let home = temp_home("firstboot");
    let (http, mqtt) = (free_port(), free_port());
    let mut gw = spawn(&home, http, mqtt, &[]);
    gw.wait_until_serving();

    let status = wait_for_status(&gw, http, is_first_boot_status, "a first-boot account_state");
    assert!(
        !status.contains("\"account_state\":\"ready\""),
        "a fresh install must not advertise a usable admin"
    );

    let (code, body) = request_until_ready(&gw, http, "GET", "/api/users", None, None);
    assert_eq!(
        code, 403,
        "a token-less /api/users must be 403 setup_required, never 401; body:\n{body}"
    );
    assert!(
        body.contains("setup_required"),
        "the 403 must name the reason; body:\n{body}"
    );
    assert!(
        body.contains("account_state"),
        "the 403 must publish the state; body:\n{body}"
    );

    let accounts = account_store(&home);
    wait_for_file(&gw, &accounts, "the seeded account store");
    assert!(
        fs::read_to_string(&accounts)
            .expect("read accounts.json")
            .contains("\"admin\""),
        "the seed must be the built-in admin"
    );

    // The operator is still pointed at the CLI paths on stderr (they remain
    // valid), and the same text goes to the log file.
    assert!(
        gw.stderr().contains("admin-setup"),
        "stderr must point at the setup paths; got:\n{}",
        gw.stderr()
    );

    // ADR-114: the Desktop on the Gateway host drives the existing bootstrap
    // admin through `POST /api/auth/setup`, gets a token pair back, and the
    // Gateway lifts the gate on its next health poll.
    let (code, body) = post_until_ready(
        &gw,
        http,
        "/api/auth/setup",
        r#"{"username":"owner","password":"ownerpass1","display_name":"Owner"}"#,
    );
    assert_eq!(code, 200, "loopback setup must succeed; body:\n{body}");
    let access = json_string(&body, "access_token")
        .expect("the setup response must carry an access token");
    assert!(!access.is_empty());

    wait_for_status(
        &gw,
        http,
        |body| body.contains("\"account_state\":\"ready\""),
        "the ready state after setup",
    );

    let (code, body) = wait_for_authenticated_api(http, &access);
    assert_eq!(code, 200, "the normal API must open after setup; body:\n{body}");

    // One-shot: a second setup is 409 even while the Gateway's health
    // snapshot still says "needs setup" — the user service is the authority.
    let (code, body) = post_until_ready(
        &gw,
        http,
        "/api/auth/setup",
        r#"{"username":"attacker","password":"hacked123"}"#,
    );
    assert_eq!(code, 409, "a second setup must be refused; body:\n{body}");

    let raw = fs::read_to_string(&accounts).expect("read accounts.json");
    assert!(raw.contains("owner"), "the chosen username must be persisted");
    assert!(
        !raw.contains("attacker"),
        "the refused setup must not have written anything"
    );
}

// ── Config-driven bootstrap ────────────────────────────────────────────

/// An operator who pre-configures `bootstrap_admin` in the **user service**
/// TOML starts straight in `ready` — the HTTP setup step is then unnecessary
/// and must be refused.
#[test]
fn a_configured_bootstrap_admin_starts_ready() {
    let home = temp_home("bootstrap");
    let (http, mqtt) = (free_port(), free_port());
    // Forward slashes: `Path::display()` yields backslashes on Windows, and a
    // lone backslash in a TOML basic string is an escape introducer.
    let home_s = home.display().to_string().replace('\\', "/");
    let config = write_config(
        &home,
        &format!(
            r#"
vault_dir = "{home}/data/vault"
packages_dir = "{home}/config/packages"
data_dir = "{home}/data"

[user]
config = "{home}/acowork-user.toml"
"#,
            home = home_s
        ),
    );
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

    let mut gw = spawn(
        &home,
        http,
        mqtt,
        &["--config-path", config.to_str().unwrap()],
    );
    gw.wait_until_serving();

    let accounts = account_store(&home);
    wait_for_file(&gw, &accounts, "the bootstrapped account store");
    let raw = fs::read_to_string(&accounts).expect("read accounts.json");
    assert!(
        raw.contains("root"),
        "the bootstrap admin must have been written, got:\n{raw}"
    );
    assert!(
        !raw.contains("s3cret123"),
        "accounts.json must hold a password hash, not the password"
    );

    wait_for_status(
        &gw,
        http,
        |body| body.contains("\"account_state\":\"ready\""),
        "the ready state from a configured bootstrap admin",
    );
    // `/api/status` deliberately fail-opens to `ready` before the first
    // health snapshot arrives, so seeing `ready` alone does NOT prove the
    // proxy has published `user_process`. Retry the resulting 503 window —
    // the same `Retry-After: 2` contract the Desktop's `with503Retry` uses.
    // Once the proxy is live, the user service itself answers 409.
    let (code, body) = post_until_ready(
        &gw,
        http,
        "/api/auth/setup",
        r#"{"username":"intruder","password":"hacked123"}"#,
    );
    assert_eq!(code, 409, "setup must be refused once an admin can log in; body:\n{body}");
}

// ── Network topology is orthogonal to the account system ───────────────

/// A loopback-only Gateway is **not** "local mode". ADR-114: the account
/// system is always present, so even the most private bind creates the store
/// and the signing key.
#[test]
fn a_loopback_bind_still_has_the_full_account_system() {
    let home = temp_home("loopback");
    let (http, mqtt) = (free_port(), free_port());
    let mut gw = spawn(&home, http, mqtt, &[]);
    gw.wait_until_serving();

    wait_for_file(&gw, &account_store(&home), "the account store on a loopback bind");
    assert!(
        home.join("acowork-user").join("auth").exists(),
        "the signing-key directory must exist on every deployment"
    );
    wait_for_status(&gw, http, is_first_boot_status, "the first-boot state on a loopback bind");
}

/// The regression that motivated ADR-114: binding `0.0.0.0` used to flip the
/// account regime. It must now change **nothing** about accounts — the fresh
/// install is still in first-boot state, and a loopback peer can still finish
/// setup over HTTP (the peer, not the bind, is the trust boundary).
#[test]
fn a_public_bind_changes_nothing_about_the_account_state() {
    let home = temp_home("publicbind");
    let (http, mqtt) = (free_port(), free_port());
    let mut gw = spawn_on(
        &home,
        &format!("0.0.0.0:{http}"),
        http,
        mqtt,
        &[],
    );
    // `127.0.0.1:<port>` still connects when the listener is on `0.0.0.0`.
    gw.wait_until_serving();

    wait_for_status(&gw, http, is_first_boot_status, "the first-boot state on a public bind");
    wait_for_file(&gw, &account_store(&home), "the account store on a public bind");

    let (code, body) = post_until_ready(
        &gw,
        http,
        "/api/auth/setup",
        r#"{"username":"owner","password":"ownerpass1"}"#,
    );
    assert_eq!(
        code, 200,
        "a loopback peer must be able to bootstrap a publicly-bound Gateway; body:\n{body}"
    );
    wait_for_status(
        &gw,
        http,
        |body| body.contains("\"account_state\":\"ready\""),
        "the ready state on a public bind",
    );
}
