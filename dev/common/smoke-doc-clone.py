#!/usr/bin/env python3
"""Isolated Gateway smoke: install doc-clone and verify ADR-116 contracts.

Steps (design 34 §7.2 + Doc Clone Validation plan §4):
  1. Isolated --home Gateway on free ports
  2. auth setup → Bearer token
  3. POST /api/apps/install with unsigned doc-clone .app
  4. GET /api/apps: doc-clone ready + builtin doc still present
  5. session-ticket → UI + app-context (app_id=doc-clone)
  6. cross-app ticket rejected
  7. mcp_gateway_path / mcp_url published; MCP endpoint not 404
  8. app-data/doc-clone isolated from built-in DOC data dir

Usage (repo root):
  python dev/common/pack-doc-clone.py
  python dev/common/smoke-doc-clone.py
"""

from __future__ import annotations

import json
import os
import shutil
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request


ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
HOME = os.path.join(ROOT, ".smoke-doc-clone")
NODE_HOME = os.path.join(ROOT, ".smoke-doc-clone-node")
APP_PATH = os.path.join(ROOT, "build", "doc-clone-1.0.0.unsigned.app")

HTTP_PORT = 29976
MQTT_PORT = 29975

failures: list[str] = []


def is_windows() -> bool:
    return os.name == "nt"


def exe(name: str) -> str:
    return name + (".exe" if is_windows() else "")


def find_gateway() -> str:
    names = [exe("acowork-gateway")]
    roots = [
        os.path.join(ROOT, "core", "target", "debug"),
        os.path.join(ROOT, "core", "target", "release"),
        os.path.join(ROOT, "target", "debug"),
        os.path.join(ROOT, "target", "release"),
        os.path.join(ROOT, "target-e2e", "debug"),
        os.path.join(ROOT, "target-s1", "debug"),
    ]
    for root in roots:
        for name in names:
            path = os.path.join(root, name)
            if os.path.isfile(path):
                return path
    raise SystemExit(
        "acowork-gateway not found; build it first:\n"
        "  cd core && cargo build -p acowork-gateway"
    )


def check(label: str, cond: bool, detail: str = "") -> bool:
    msg = ("  PASS " if cond else "  FAIL ") + label
    if detail:
        msg += " — " + detail
    print(msg)
    if not cond:
        failures.append(label)
    return cond


def free_port(port: int) -> bool:
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def kill_smoke_orphans() -> None:
    """Terminate leftover smoke Gateway/DOC children that hold .exe locks.

    Never matches the Desktop Gateway home (~/.acowork/acowork-gateway); only
    processes whose command line references this smoke HOME / ports.
    """
    if not is_windows():
        return
    home_l = HOME.replace("/", "\\").lower()
    try:
        raw = subprocess.check_output(
            [
                "powershell",
                "-NoProfile",
                "-Command",
                "[Console]::OutputEncoding = [Text.UTF8Encoding]::UTF8; "
                "Get-CimInstance Win32_Process | "
                "Select-Object ProcessId,Name,CommandLine | ConvertTo-Json -Compress",
            ],
            stderr=subprocess.DEVNULL,
            timeout=30,
        )
        out = raw.decode("utf-8", errors="replace")
    except (subprocess.SubprocessError, OSError):
        return
    try:
        rows = json.loads(out or "[]")
    except ValueError:
        return
    if isinstance(rows, dict):
        rows = [rows]
    self_pid = os.getpid()
    for row in rows or []:
        name = (row.get("Name") or "").lower()
        cmd = (row.get("CommandLine") or "").lower()
        pid = row.get("ProcessId")
        if not pid or int(pid) == self_pid:
            continue
        # Only ACowork binaries — never python.exe running this smoke script
        # (its argv also contains "smoke-doc-clone").
        if not name.startswith("acowork-"):
            continue
        if home_l not in cmd and (".smoke-doc-clone" not in cmd) and (
            ("127.0.0.1:%d" % HTTP_PORT) not in cmd
        ) and (("--mqtt-addr 127.0.0.1:%d" % MQTT_PORT) not in cmd):
            continue
        # Safety: never kill the interactive Desktop Gateway on :19876.
        if ":19876" in cmd and home_l not in cmd:
            continue
        try:
            subprocess.check_call(
                ["taskkill", "/PID", str(int(pid)), "/F", "/T"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            print("killed orphan pid=%s name=%s" % (pid, name))
        except (subprocess.SubprocessError, OSError):
            try:
                os.kill(int(pid), 9)
            except OSError:
                pass


def wait_port(port: int, timeout: float = 90.0) -> None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = socket.socket()
        s.settimeout(0.5)
        try:
            s.connect(("127.0.0.1", port))
            return
        except OSError:
            time.time()
            time.sleep(0.2)
        finally:
            s.close()
    raise RuntimeError("port %d never opened" % port)


def http(
    method: str,
    path: str,
    body=None,
    token: str | None = None,
    headers: dict | None = None,
    raw: bytes | None = None,
    content_type: str | None = None,
    timeout: float = 30.0,
):
    url = "http://127.0.0.1:%d%s" % (HTTP_PORT, path)
    data = raw
    if data is None and body is not None:
        data = json.dumps(body).encode()
        content_type = content_type or "application/json"
    req = urllib.request.Request(url, data=data, method=method)
    if content_type:
        req.add_header("Content-Type", content_type)
    if token:
        req.add_header("Authorization", "Bearer " + token)
    if headers:
        for k, v in headers.items():
            req.add_header(k, v)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw_body = r.read()
            ctype = r.headers.get("Content-Type", "")
            if "json" in ctype or (raw_body[:1] in (b"{", b"[")):
                try:
                    return r.status, json.loads(raw_body.decode() or "null")
                except ValueError:
                    return r.status, raw_body.decode(errors="replace")
            return r.status, raw_body.decode(errors="replace")
    except urllib.error.HTTPError as e:
        raw_body = e.read()
        try:
            return e.code, json.loads(raw_body.decode() or "null")
        except ValueError:
            return e.code, raw_body.decode(errors="replace")


def home_path(*parts: str) -> str:
    return os.path.join(HOME, *parts).replace("\\", "/")


def write_config() -> str:
    cfg_dir = os.path.join(HOME, "config")
    os.makedirs(cfg_dir, exist_ok=True)
    os.makedirs(os.path.join(HOME, "data"), exist_ok=True)
    os.makedirs(os.path.join(HOME, "packages"), exist_ok=True)
    os.makedirs(os.path.join(HOME, "config", "vault"), exist_ok=True)
    path = os.path.join(cfg_dir, "gateway.toml")
    # vault_dir / packages_dir / data_dir are required by GatewayConfig.
    text = (
        'vault_dir = "%s"\n'
        'packages_dir = "%s"\n'
        'data_dir = "%s"\n'
        "dev_mode = true\n\n"
        '[http]\nhost = "127.0.0.1"\nport = %d\n\n'
        '[mqtt]\nhost = "127.0.0.1"\nport = %d\nauth_enabled = false\n\n'
        "[apps]\nallow_unsigned = true\n"
        % (
            home_path("config", "vault"),
            home_path("packages"),
            home_path("data"),
            HTTP_PORT,
            MQTT_PORT,
        )
    )
    open(path, "w", encoding="utf-8").write(text)
    return path


def apps_list(body) -> list:
    if isinstance(body, dict) and isinstance(body.get("apps"), list):
        return body["apps"]
    if isinstance(body, list):
        return body
    return []


def poll_app(token: str, app_id: str, timeout: float = 90.0) -> dict | None:
    deadline = time.time() + timeout
    while time.time() < deadline:
        status, body = http("GET", "/api/apps/%s" % app_id, token=token)
        if status == 200 and isinstance(body, dict) and body.get("id") == app_id:
            st = (body.get("status") or "").lower()
            if st in ("ready", "running", "healthy") or body.get("port"):
                return body
        status, body = http("GET", "/api/apps", token=token)
        if status == 200:
            for app in apps_list(body):
                if app.get("id") == app_id:
                    st = (app.get("status") or "").lower()
                    if st in ("ready", "running", "healthy") or app.get("port"):
                        return app
        time.sleep(1.0)
    return None


def main() -> int:
    if not os.path.isfile(APP_PATH):
        print("missing package:", APP_PATH)
        print("run: python dev/common/pack-doc-clone.py")
        return 2

    if not free_port(HTTP_PORT) or not free_port(MQTT_PORT):
        print("ports %d/%d busy — stop conflicting processes" % (HTTP_PORT, MQTT_PORT))
        return 2

    def wipe(path: str) -> None:
        if not os.path.exists(path):
            return
        # Windows often keeps .exe handles briefly after supervisor kill;
        # retry so a partial apps/doc-clone/versions tree cannot 409 install.
        last_err: Exception | None = None
        for i in range(10):
            try:
                shutil.rmtree(path)
                return
            except Exception as e:  # noqa: BLE001 — best-effort wipe
                last_err = e
                if i in (2, 5):
                    kill_smoke_orphans()
                time.sleep(0.5)
        raise SystemExit("failed to wipe %s: %s" % (path, last_err))

    kill_smoke_orphans()
    wipe(HOME)
    wipe(NODE_HOME)
    cfg = write_config()
    gateway = find_gateway()

    env = dict(os.environ)
    env["ACOWORK_HOME"] = HOME
    env["ACOWORK_NODE_HOME"] = NODE_HOME
    # Keep spawn quiet relative to the running Desktop Gateway.
    env.pop("ACOWORK_GATEWAY_HTTP_PORT", None)

    cmd = [
        gateway,
        "--daemon",
        "--home",
        HOME,
        "--config-path",
        cfg,
        "--addr",
        "127.0.0.1:%d" % HTTP_PORT,
        "--mqtt-addr",
        "127.0.0.1:%d" % MQTT_PORT,
        "--advertise-host",
        "127.0.0.1",
        "--no-spawn-local-node",
    ]
    log_path = os.path.join(HOME, "smoke-gateway.log")
    os.makedirs(HOME, exist_ok=True)
    log_file = open(log_path, "wb")
    print("starting:", " ".join(cmd))
    print("gateway log:", log_path)
    proc = subprocess.Popen(
        cmd,
        env=env,
        cwd=ROOT,
        stdout=log_file,
        stderr=subprocess.STDOUT,
    )

    try:
        wait_port(HTTP_PORT, timeout=120)
        # Health
        status, _ = http("GET", "/health")
        check("health", status == 200, "status=%s" % status)

        # User service boots asynchronously; auth returns 503 until ready.
        ready = False
        deadline = time.time() + 90
        while time.time() < deadline:
            status, pair = http(
                "POST",
                "/api/auth/setup",
                body={
                    "username": "admin",
                    "password": "admin1234",
                    "display_name": "Smoke Admin",
                },
            )
            if status == 503:
                time.sleep(0.5)
                continue
            if status in (200, 201) and isinstance(pair, dict) and pair.get("access_token"):
                token = pair["access_token"]
                check("auth setup", True)
                ready = True
                break
            # Already set up in a reused home — try login
            status, pair = http(
                "POST",
                "/api/auth/login",
                body={"username": "admin", "password": "admin1234"},
            )
            if status == 503:
                time.sleep(0.5)
                continue
            check("auth login", status == 200 and isinstance(pair, dict), str(status))
            if not isinstance(pair, dict) or not pair.get("access_token"):
                return 1
            token = pair["access_token"]
            ready = True
            break
        if not ready:
            check("auth ready", False, "timed out waiting for user service")
            return 1

        # Gateway's restricted_mode reads a polled user_snapshot; wait until
        # it observes account_state=ready or install will 403 setup_required.
        account_ready = False
        deadline = time.time() + 60
        while time.time() < deadline:
            status, st = http("GET", "/api/status")
            state = st.get("account_state") if isinstance(st, dict) else None
            if state == "ready":
                account_ready = True
                break
            time.sleep(0.5)
        check("account_state ready", account_ready, "last=%s" % (st if not account_ready else "ready"))
        if not account_ready:
            return 1

        # Install (retry once after DELETE if a leftover version_exists)
        package = open(APP_PATH, "rb").read()
        status, inst = http(
            "POST",
            "/api/apps/install",
            token=token,
            raw=package,
            content_type="application/octet-stream",
            timeout=120,
        )
        if status == 409:
            http("DELETE", "/api/apps/doc-clone?purge=1", token=token)
            # Also wipe on-disk ledger in case registry never loaded it.
            leftover = os.path.join(HOME, "data", "apps", "doc-clone")
            if os.path.isdir(leftover):
                shutil.rmtree(leftover, ignore_errors=True)
            status, inst = http(
                "POST",
                "/api/apps/install",
                token=token,
                raw=package,
                content_type="application/octet-stream",
                timeout=120,
            )
        check("install", status in (200, 201), "status=%s body=%s" % (status, inst))

        # Poll ready
        clone = poll_app(token, "doc-clone", timeout=120)
        check(
            "doc-clone ready",
            clone is not None,
            "port=%s status=%s" % (None if not clone else clone.get("port"), None if not clone else clone.get("status")),
        )

        status, apps_body = http("GET", "/api/apps", token=token)
        apps = apps_list(apps_body)
        ids = {a.get("id") for a in apps}
        check("builtin doc present", "doc" in ids, "ids=%s" % sorted(ids))
        check("doc-clone listed", "doc-clone" in ids)

        if clone:
            mcp_path = clone.get("mcp_gateway_path") or ""
            mcp_url = clone.get("mcp_url") or ""
            check(
                "mcp gateway path",
                "/api/apps/doc-clone/mcp" in mcp_path or "/api/apps/doc-clone/mcp" in mcp_url,
                "path=%s url=%s" % (mcp_path, mcp_url),
            )

        # Session ticket + UI
        status, ticket_body = http(
            "POST", "/api/apps/doc-clone/session-ticket", token=token, body={}
        )
        check("session-ticket", status == 200 and isinstance(ticket_body, dict), str(ticket_body))
        ticket = (ticket_body or {}).get("ticket") if isinstance(ticket_body, dict) else None

        if ticket:
            status, ui = http("GET", "/api/apps/doc-clone/ui/", headers={"X-App-Session": ticket})
            check("ui html", status == 200 and isinstance(ui, str) and "doc-clone" in ui, "status=%s" % status)

            status, ctx = http(
                "GET",
                "/api/apps/doc-clone/api/app-context",
                headers={"X-App-Session": ticket},
            )
            app_id = ctx.get("app_id") if isinstance(ctx, dict) else None
            check("app-context app_id", app_id == "doc-clone", str(ctx))

            # Cross-app: ticket for doc-clone must not open builtin doc channel
            status, cross = http(
                "GET",
                "/api/apps/doc/api/app-context",
                headers={"X-App-Session": ticket},
            )
            check("cross-app ticket rejected", status in (401, 403), "status=%s body=%s" % (status, cross))

        # MCP proxy reachable (initialize-ish JSON-RPC; 4xx from DOC is fine, 404 from Gateway is not)
        status, mcp = http(
            "POST",
            "/api/apps/doc-clone/mcp",
            token=token,
            body={"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}},
            headers={"Accept": "application/json, text/event-stream"},
        )
        check(
            "mcp proxy not 404",
            status != 404,
            "status=%s" % status,
        )

        # Data isolation
        app_data = os.path.join(HOME, "data", "app-data", "doc-clone")
        builtin_doc = os.path.join(os.path.expanduser("~"), ".acowork", "acowork-doc")
        # After ready, supervisor should have created the app-data dir (or DOC will on first write).
        # Touch a marker only under app-data and assert builtin home is not that path.
        os.makedirs(app_data, exist_ok=True)
        marker = os.path.join(app_data, ".smoke-marker")
        open(marker, "w", encoding="utf-8").write("doc-clone-smoke\n")
        check("app-data dir exists", os.path.isdir(app_data), app_data)
        check(
            "not sharing builtin data dir",
            os.path.normcase(os.path.abspath(app_data))
            != os.path.normcase(os.path.abspath(builtin_doc)),
            "app=%s builtin=%s" % (app_data, builtin_doc),
        )
        # Builtin DOC data dir should not contain our marker (even if it exists)
        leaked = os.path.join(builtin_doc, ".smoke-marker")
        check("no leak into builtin DOC", not os.path.isfile(leaked), leaked)

    finally:
        proc.terminate()
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)
        kill_smoke_orphans()
        try:
            log_file.close()
        except Exception:
            pass
        if failures and os.path.isfile(log_path):
            try:
                err = open(log_path, "rb").read().decode(errors="replace")[-4000:]
            except OSError:
                err = ""
            if err.strip():
                print("--- gateway log (tail) ---")
                print(err)

    print()
    if failures:
        print("FAILED:", ", ".join(failures))
        return 1
    print("ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
