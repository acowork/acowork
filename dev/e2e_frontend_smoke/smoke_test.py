#!/usr/bin/env python3
"""ADR-033/055 Frontend Smoke Test — Desktop Simulator (full suite).

Covers every test case in docs/plan/zh/e2e-frontend-smoke-test.md (§5.1–5.13),
driving a real Gateway (embedded rumqttd) + Agent Runtime through their
public HTTP and MQTT surfaces, exactly like the Desktop App would.

Also covers the PM member feature end-to-end (cross-process): the Gateway
reverse-proxies `/api/pm/*` to a SEPARATE `acowork-pm` process that
persists to disk; REST paths run as human (X-Actor injected), the MCP path
as the assigned agent (X-MCP-Actor). Requires `target/debug/acowork-pm`
(CI builds it via `cargo build --workspace --bins`). See `run_pm_suite`.

Requires (installed once):
    pip install paho-mqtt httpx        # or: pip install -r requirements.txt

Usage:
    python3 smoke_test.py                                  # debug binaries, temp homes
    python3 smoke_test.py --gateway-bin ./target/release/acowork-gateway \
                          --node-bin ./target/release/acowork-node
    SMOKE_LLM=1 python3 smoke_test.py                      # also run LLM chat cases
    python3 smoke_test.py --no-start                       # reuse a running Gateway (main only)

Exit code 0 = all passed/skipped, 1 = any failure.
"""

import argparse
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

import httpx
import paho.mqtt.client as mqtt


# ── Config ──────────────────────────────────────────────────────────────

# Main instance ports. The defaults match the production gateway
# (:19876 HTTP / :19875 MQTT) for `--no-start` reuse; when a real
# gateway is already running on this machine, override via env so the
# spawned test instance does not collide:
#   SMOKE_HTTP_PORT=19832 SMOKE_MQTT_PORT=19831 python3 smoke_test.py
DEFAULT_HTTP_PORT = int(os.environ.get("SMOKE_HTTP_PORT", "19876"))
DEFAULT_MQTT_PORT = int(os.environ.get("SMOKE_MQTT_PORT", "19875"))
# Auth suite instance: HTTP port must stay <= GATEWAY_HTTP_PORT_MAX (19878)
# or the Gateway's find_available_port() range probe fails outright.
AUTH_HTTP_PORT = 19786
AUTH_MQTT_PORT = 19785
TIMEOUT = 30
AGENT_ID = "com.acowork.senior-engineer"
NODE_ID = "smoke-node"

# Builtin tool used by TC-SETUP-02/04 to exercise the enable/disable round-trip.
# Must be a tool the Runtime actually registers and that no other case depends on.
PROBE_TOOL = "doc_reader"

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
AGENT_PACKAGE = REPO_ROOT / "examples" / "agent-packages" / f"{AGENT_ID}.agent"

# LLM-dependent cases (chat streaming, tool calls, flow stop) only run when
# SMOKE_LLM=1 — a CI box without a configured provider skips them gracefully.
LLM_ENABLED = os.environ.get("SMOKE_LLM") == "1"

# Test result counters
passed = 0
failed = 0
skipped = 0


def log(level, msg):
    print(f"  [{level}] {msg}")


def ok(msg=""):
    global passed
    passed += 1
    print(f"✅ {msg}")


def fail(msg):
    global failed
    failed += 1
    print(f"❌ {msg}")


def skip(msg):
    global skipped
    skipped += 1
    print(f"⬜ SKIP: {msg}")


def assert_status(resp, expected, label=""):
    if resp.status_code == expected:
        ok(label or f"HTTP {resp.status_code}")
        return True
    fail(f"{label or 'HTTP'}: expected {expected}, got {resp.status_code}\n  body: {resp.text[:500]}")
    return False


def random_suffix():
    return uuid.uuid4().hex[:6]


# ── Minimal protobuf codec (proto3 wire format) ─────────────────────────
# Only the messages the smoke suite speaks. Field numbers mirror
# core/acowork-core/proto/mqtt_payload.proto; keep in sync on proto change.

def _varint(n):
    out = bytearray()
    while True:
        b = n & 0x7F
        n >>= 7
        if n:
            out.append(b | 0x80)
        else:
            out.append(b)
            return bytes(out)


def _read_varint(buf, pos):
    result = 0
    shift = 0
    while pos < len(buf):
        b = buf[pos]
        pos += 1
        result |= (b & 0x7F) << shift
        if not (b & 0x80):
            return result, pos
        shift += 7
    return result, pos


def _field(no, wt, payload):
    return _varint((no << 3) | wt) + payload


def _f_str(no, s):
    b = s.encode()
    return _field(no, 2, _varint(len(b)) + b)


def _f_bytes(no, b):
    return _field(no, 2, _varint(len(b)) + b)


def _f_uint(no, n):
    return _field(no, 0, _varint(n))


def _f_msg(no, inner):
    return _f_bytes(no, inner)


def _parse_fields(buf):
    fields = []  # (field_no, wire_type, value)
    pos = 0
    while pos < len(buf):
        tag, pos = _read_varint(buf, pos)
        no, wt = tag >> 3, tag & 7
        if wt == 0:
            v, pos = _read_varint(buf, pos)
            fields.append((no, wt, v))
        elif wt == 2:
            ln, pos = _read_varint(buf, pos)
            v = buf[pos:pos + ln]
            pos += ln
            fields.append((no, wt, v))
        else:  # groups never used by our protos
            break
    return fields


def _field_str(fields, no):
    for fno, wt, v in fields:
        if fno == no and wt == 2:
            return v.decode()
    return ""


def _field_uint(fields, no):
    for fno, wt, v in fields:
        if fno == no and wt == 0:
            return v
    return 0


def _field_msg(fields, no):
    for fno, wt, v in fields:
        if fno == no and wt == 2:
            return v
    return b""


# Control command builders: cmd name → (oneof field no, submessage builder).
_CONTROL_CMDS = {
    "create_session": (10, lambda **k: b""),
    "delete_session": (11, lambda **k: _f_str(2, k["session_id"])),
    "update_title": (20, lambda **k: _f_str(2, k["session_id"]) + _f_str(3, k["title"])),
    "message": (21, lambda **k: _f_str(2, k["session_id"]) + _f_str(3, k["message_id"]) + _f_str(4, k["content"])),
    "stop": (22, lambda **k: _f_str(2, k["session_id"])),
    "model_switch": (14, lambda **k: _f_str(2, k["session_id"]) + _f_str(3, k["model_id"])),
    "reasoning_effort": (15, lambda **k: _f_str(2, k["session_id"]) + _f_str(3, k["effort"])),
}


def encode_control(cmd_name, agent_id, **fields):
    """DataEnvelope{version=1, control_command=40} with the given command."""
    cmd_no, build = _CONTROL_CMDS[cmd_name]
    cmd = _f_str(1, agent_id) + _f_msg(cmd_no, build(**fields))
    return _f_uint(1, 1) + _f_msg(40, cmd)


def decode_envelope_payload(buf):
    """Return (oneof_field_no, message_bytes) of the DataEnvelope payload.

    Recognised oneofs: 30 session_created, 31 session_deleted, 34
    session_message, 85 node_enroll, 86 node_enroll_result.
    """
    for no, wt, v in _parse_fields(buf):
        if wt == 2 and no in (30, 31, 34, 85, 86):
            return no, v
    return None, b""


def decode_session_created(buf):
    f = _parse_fields(buf)
    return {"agent_id": _field_str(f, 1), "session_id": _field_str(f, 2),
            "title": _field_str(f, 3), "created_at": _field_str(f, 4)}


def decode_session_deleted(buf):
    f = _parse_fields(buf)
    return {"agent_id": _field_str(f, 1), "session_id": _field_str(f, 2)}


# SessionMessage event oneof: field no → event name
_EVENT_NAMES = {10: "chunk", 11: "tool_call", 12: "tool_result", 13: "done",
                14: "error", 15: "stopped", 16: "ask_question"}


def decode_session_message(buf):
    """Return {"event": str, "message_id": str, "delta": str, "error": str,
    "tool_name": str, "stopped": bool}."""
    f = _parse_fields(buf)
    ev_no = None
    ev_buf = b""
    for fno, wt, v in f:
        if wt == 2 and fno in _EVENT_NAMES:
            ev_no, ev_buf = fno, v
    out = {"event": _EVENT_NAMES.get(ev_no, "?"), "message_id": "", "delta": "",
           "error": "", "tool_name": "", "stopped": ev_no == 15}
    if ev_no is None:
        return out
    ef = _parse_fields(ev_buf)
    out["message_id"] = _field_str(ef, 1)
    out["delta"] = _field_str(ef, 2)
    out["error"] = _field_str(ef, 2) if ev_no == 14 else out["error"]
    if ev_no == 11:  # tool_call: message_id=1, tool_name=2
        out["tool_name"] = _field_str(ef, 2)
    return out


def decode_node_enroll(buf):
    f = _parse_fields(buf)
    return {"node_id": _field_str(f, 1),
            "enrollment_token": _field_str(f, 8)}


def decode_node_enroll_result(buf):
    f = _parse_fields(buf)
    return {"node_id": _field_str(f, 1),
            "node_token": _field_str(f, 3), "status": _field_str(f, 4),
            "message": _field_str(f, 5)}


def decode_available_providers_from_envelope(buf):
    """Decode DataEnvelope → AvailableProviders → [{id, base_url, api_key}].

    The api_key rides the MQTT payload DECRYPTED (mqtt.md §3.1.1); an empty
    value means the publisher had no credential at build time (e.g. vault
    still locked) — which is exactly the regression we guard against.
    """
    fields = _parse_fields(buf)
    version = _field_uint(fields, 1)
    for fno, wt, v in fields:
        if wt == 2 and fno == 10:  # available_providers
            ap = _parse_fields(v)
            providers = []
            for pno, pwt, pv in ap:
                if pno == 2 and pwt == 2:  # repeated ProviderRef
                    pf = _parse_fields(pv)
                    providers.append({
                        "id": _field_str(pf, 1),
                        "base_url": _field_str(pf, 2),
                        "api_key": _field_str(pf, 7),
                    })
            return version, providers
    return version, []


# ── Gateway Management ──────────────────────────────────────────────────

class Gateway:
    """Spawns an isolated acowork-gateway daemon under a fresh ACOWORK_HOME."""

    def __init__(self, bin_path, home, http_port=DEFAULT_HTTP_PORT,
                 mqtt_port=DEFAULT_MQTT_PORT, auth_enabled=False):
        self.bin = Path(bin_path)
        self.home = Path(home)
        self.http_port = http_port
        self.mqtt_port = mqtt_port
        self.auth_enabled = auth_enabled
        self.base = f"http://127.0.0.1:{http_port}"
        self.proc = None
        # Isolated node ports (see _write_config) — also used by stop() to
        # sweep THIS instance's lsp-relay without touching the real one.
        self.proxy_port = 19790
        self.lsp_relay_port = 19789
        self._write_config()

    def _write_config(self):
        (self.home / "config").mkdir(parents=True, exist_ok=True)
        toml = (
            f"vault_dir = '{self.home}/config/vault'\n"
            f"packages_dir = '{self.home}/config/packages'\n"
            f"data_dir = '{self.home}/data'\n"
            "log_level = \"info\"\n"
            "dev_mode = true\n"
            # ADR-055 D3: pin advertise_host to loopback — the HTTP API
            # binds 127.0.0.1, so a LAN-detected advertise address would
            # produce an unreachable package download URL for the node.
            "advertise_host = \"127.0.0.1\"\n"
            # Isolated node ports: the real install's local node owns
            # the defaults (:19900 proxy / :19878 LSP relay) on this
            # machine. Without overrides this instance's node would
            # fail to bind its proxy and its orphan scan could even
            # re-adopt the REAL install's Runtime (it talks to the
            # real broker, so it never comes online here).
            # NOTE: must stay in the root table — AFTER a [section]
            # header these keys would belong to that table and serde
            # would silently drop them.
            f"node_proxy_port = {self.proxy_port}\n"
            f"node_lsp_relay_port = {self.lsp_relay_port}\n"
            f"[http]\nport = {self.http_port}\n"
            f"[mqtt]\nenabled = true\nhost = \"127.0.0.1\"\nport = {self.mqtt_port}\n"
            f"auth_enabled = {str(self.auth_enabled).lower()}\n"
        )
        (self.home / "config" / "gateway.toml").write_text(toml)

    def start(self):
        log("INFO", f"Starting Gateway (http :{self.http_port}, mqtt :{self.mqtt_port}, "
                    f"auth={self.auth_enabled}): {self.bin}")
        # ACOWORK_NODE_HOME: the Gateway spawns its local node agent
        # (`--gateway-managed`, no explicit --name) WITHOUT --home, so the
        # node would otherwise
        # use the shared default ~/.acowork/acowork-node. A stale
        # identity.json there (left by an earlier auth-enabled run)
        # makes the node proxy demand X-ACowork-Node-Token even when
        # this instance runs with auth disabled — self-locking every
        # proxied request with 403. Isolate the node home per instance.
        node_home = self.home / "node"
        env = {**os.environ, "ACOWORK_HOME": str(self.home),
               "ACOWORK_NODE_HOME": str(node_home)}
        started = time.monotonic()
        # stderr=PIPE would deadlock the gateway: it emits ~MBs of logs in
        # the first seconds, and with no reader the pipe buffer (64 KB)
        # fills and blocks the logger, stalling the HTTP server and
        # blowing the desktop 10s probe window. Redirect to a file so the
        # gateway can start without backpressure (and we keep the log
        # for debugging).
        self.err_log = open(self.home / "gateway.err.log", "w", encoding="utf-8", errors="replace")
        self.proc = subprocess.Popen(
            [str(self.bin), "--daemon", "--log-level", "info", "--home", str(self.home)],
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=self.err_log,
        )
        # TC-BOOT-01 contract: the Desktop probes /health every 300 ms for
        # ~10 s (wait_for_gateway_ready, 34 × 300ms) and then gives up with
        # "Gateway did not become ready within 10 seconds". If the gateway
        # cannot serve /health inside that window the desktop boot FAILS,
        # so the smoke test enforces the same window — not a generous 30 s.
        with httpx.Client(timeout=3) as client:
            for _ in range(10):
                try:
                    r = client.get(f"{self.base}/health")
                    if r.status_code == 200:
                        elapsed = time.monotonic() - started
                        log("INFO", f"Gateway ready in {elapsed:.2f}s (desktop probe window is 10s)")
                        return True
                except Exception:
                    pass
                time.sleep(1)
        fail("Gateway did not become ready within 10s (desktop probe window)")
        return False

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.send_signal(signal.SIGTERM)
            try:
                self.proc.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.proc.kill()
            log("INFO", "Gateway stopped")
        self.proc = None
        if getattr(self, "err_log", None):
            self.err_log.close()
            self.err_log = None
        # Reap orphaned children the gateway spawned (local node agent /
        # embed model runner). Gateway SIGTERM does not kill them, and a
        # stale node keeps holding the 19900 proxy port — the next run's
        # node then logs "reverse proxy failed to bind" and degrades.
        # The user service (ADR-084) is reaped for the same reason: its
        # ADR-018 watchdog can lag up to ~300s while it keeps a lock on
        # target/debug/acowork-user.exe, which stalls concurrent rebuilds.
        home_str = str(self.home)
        for pat in ("acowork-node", "acowork-embed", "acowork-user"):
            try:
                out = subprocess.run(
                    ["pgrep", "-f", rf"{pat} .*{home_str}"],
                    capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5,
                ).stdout
            except Exception:
                continue
            for pid in out.split():
                try:
                    os.kill(int(pid), signal.SIGTERM)
                    log("INFO", f"reaped orphaned {pat} (pid {pid})")
                except ProcessLookupError:
                    pass
        # The node's LSP-relay sidecar has no home path on its command
        # line (it health-checks the node proxy), so it can't be matched
        # per-instance — sweep only the relay bound to THIS instance's
        # isolated port. The real install's relay runs on :19878 and must
        # never be touched by an isolated-instance teardown.
        try:
            out = subprocess.run(
                ["pgrep", "-f", rf"acowork-lsp-relay .*--port {self.lsp_relay_port}"],
                capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=5).stdout
        except Exception:
            out = ""
        for pid in out.split():
            try:
                os.kill(int(pid), signal.SIGTERM)
                log("INFO", f"reaped orphaned acowork-lsp-relay (pid {pid})")
            except ProcessLookupError:
                pass

    def cli(self, args, timeout=15):
        """Run a gateway CLI subcommand against this home (e.g. nodes token create)."""
        return subprocess.run(
            [str(self.bin), *args],
            env={**os.environ, "ACOWORK_HOME": str(self.home)},
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=timeout,
        )

    @property
    def http_token(self):
        """Bearer token for MQTT desktop-identity connections on an auth-enabled broker."""
        path = self.home / "data" / "http_token"
        return path.read_text().strip() if path.exists() else ""


# ── MQTT Client (Desktop Simulator) ─────────────────────────────────────

class MqttClient:
    def __init__(self, username=None, password=None, client_id=None):
        self.client_id = client_id or f"user:smoke-test:desktop:{os.getpid()}"
        self.client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id=self.client_id)
        if username is not None:
            self.client.username_pw_set(username, password)
        self.received = {}  # topic -> list of payloads
        self.connack_rc = None
        self.connected = False
        self.client.on_connect = self._on_connect
        self.client.on_message = self._on_message

    def _on_connect(self, client, userdata, flags, reason_code, properties):
        self.connack_rc = reason_code.value if hasattr(reason_code, "value") else reason_code
        self.connected = reason_code == 0 or (hasattr(reason_code, "is_failure") and not reason_code.is_failure)
        log("INFO", f"MQTT CONNACK {self.connack_rc} ({self.client_id})")

    def _on_message(self, client, userdata, msg):
        self.received.setdefault(msg.topic, []).append(msg.payload)
        log("MQTT", f"← {msg.topic}: {msg.payload[:80]}")

    def connect(self, host="127.0.0.1", port=DEFAULT_MQTT_PORT, timeout=8):
        self.client.connect(host, port, 60)
        self.client.loop_start()
        deadline = time.time() + timeout
        while time.time() < deadline and self.connack_rc is None:
            time.sleep(0.05)
        return self.connected

    def disconnect(self):
        try:
            self.client.loop_stop()
            self.client.disconnect()
        except Exception:
            pass

    def subscribe(self, topic, qos=1):
        self.client.subscribe(topic, qos)
        self.received.setdefault(topic, [])

    def publish(self, topic, payload, qos=1, retain=False):
        self.client.publish(topic, payload, qos, retain)

    def wait_for(self, topic, timeout=TIMEOUT, predicate=None):
        """Wait for a message on topic; returns payload bytes or None."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            msgs = self.received.get(topic, [])
            if predicate:
                for m in msgs:
                    if predicate(m):
                        return m
            elif msgs:
                return msgs[-1]
            time.sleep(0.1)
        return None

    def wait_any(self, prefixes, timeout=TIMEOUT, predicate=None):
        """Wait for the first message whose topic starts with any prefix."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            for topic, msgs in self.received.items():
                if any(topic.startswith(p) for p in prefixes):
                    for m in msgs:
                        if predicate is None or predicate(m):
                            return topic, m
            time.sleep(0.1)
        return None, None


# ── Helpers ─────────────────────────────────────────────────────────────

def wait_http_ok(base, timeout=TIMEOUT):
    with httpx.Client(timeout=3) as client:
        for _ in range(timeout):
            try:
                if client.get(f"{base}/health").status_code == 200:
                    return True
            except Exception:
                pass
            time.sleep(1)
    return False


def wait_bootstrap_ready(http, base, timeout=TIMEOUT):
    """Readiness source of truth (ADR-059 §5.1): poll `GET /api/bootstrap`
    until phase == READY. Replaces the old node-online + fixed-sleep
    cold-start race (the install handler now answers 409
    `dependency_not_ready` until `node.local` announces readiness)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = http.get(f"{base}/api/bootstrap")
            if r.status_code == 200 and r.json().get("phase") == "READY":
                return True
        except Exception:
            pass
        time.sleep(0.3)
    return False


def ensure_agent_installed(http, base):
    """Install the smoke agent package if not already present (async install)."""
    # ADR-059: the install handler answers 409 `dependency_not_ready`
    # until node.local announces readiness — wait on /api/bootstrap (the
    # readiness source of truth) instead of the old node-online race.
    if not wait_bootstrap_ready(http, base):
        fail(f"bootstrap never reached READY — install would answer 409")
        return False
    r = http.get(f"{base}/api/agents")
    if r.status_code == 200 and any(a.get("agent_id") == AGENT_ID for a in r.json()):
        ok(f"agent already installed: {AGENT_ID}")
        return True
    if not AGENT_PACKAGE.exists():
        fail(f"agent package missing: {AGENT_PACKAGE}")
        return False
    with open(AGENT_PACKAGE, "rb") as f:
        r = http.post(f"{base}/api/agents/install",
                      files={"package": (AGENT_PACKAGE.name, f, "application/octet-stream")})
    if r.status_code == 409:
        # dependency_not_ready — a retry-aware client backs off and
        # retries until phase = READY (ADR-059 §6.3 retry_hint).
        deadline = time.time() + TIMEOUT
        while time.time() < deadline:
            time.sleep(0.5)
            with open(AGENT_PACKAGE, "rb") as f:
                r = http.post(f"{base}/api/agents/install",
                              files={"package": (AGENT_PACKAGE.name, f, "application/octet-stream")})
            if r.status_code != 409:
                break
        if r.status_code == 409:
            fail(f"agent install: dependency_not_ready persisted for {TIMEOUT}s")
            return False
    if r.status_code not in (200, 201, 202):
        fail(f"agent install: HTTP {r.status_code} {r.text[:300]}")
        return False
    # ADR-055 §3.2: async install — poll the aggregated inventory until
    # the local node reports the package installed (or timeout).
    deadline = time.time() + TIMEOUT
    while time.time() < deadline:
        r = http.get(f"{base}/api/agents")
        if r.status_code == 200 and any(a.get("agent_id") == AGENT_ID for a in r.json()):
            ok(f"agent installed: {AGENT_ID}")
            return True
        time.sleep(1)
    fail(f"agent install: not visible after {TIMEOUT}s (HTTP {r.status_code})")
    return False


def wait_identity_node_id(node_home, timeout=TIMEOUT):
    """Wait until identity.json carries a node_id (UUID)."""
    path = Path(node_home) / "identity.json"
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            try:
                data = json.loads(path.read_text())
                nid = data.get("node_id", "")
                if nid:
                    return nid
            except Exception:
                pass
        time.sleep(0.3)
    return None


def wait_identity_token(node_home, timeout=TIMEOUT):
    """Wait until identity.json carries a non-empty node_token."""
    path = Path(node_home) / "identity.json"
    deadline = time.time() + timeout
    while time.time() < deadline:
        if path.exists():
            try:
                data = json.loads(path.read_text())
                tok = data.get("node_token", "")
                if tok:
                    return tok
            except Exception:
                pass
        time.sleep(0.3)
    return None


# ── Test Cases — read-only (Gateway native, no Runtime needed) ──────────

def test_tc_chat_01_agent_list(http, base):
    """TC-CHAT-01: GET /api/agents — list + senior-engineer present."""
    print("\n── TC-CHAT-01: GET /api/agents ──")
    r = http.get(f"{base}/api/agents")
    if not assert_status(r, 200):
        return
    data = r.json()
    agents = {a.get("agent_id"): a for a in data}
    if AGENT_ID in agents:
        a = agents[AGENT_ID]
        missing = [f for f in ("agent_id", "name", "avatar") if f not in a]
        if missing:
            fail(f"missing fields: {missing}")
        else:
            ok(f"senior-engineer found ({len(data)} agents total)")
    else:
        fail(f"senior-engineer NOT in list: {list(agents.keys())}")


def test_tc_settings_01_status(http, base):
    """TC-SETTINGS-01: GET /api/status."""
    print("\n── TC-SETTINGS-01: GET /api/status ──")
    r = http.get(f"{base}/api/status")
    if not assert_status(r, 200):
        return
    data = r.json()
    missing = [f for f in ("version", "uptime_secs") if f not in data]
    ok(f"status: version={data.get('version')}" if not missing else f"missing: {missing}")


def test_tc_settings_02_config(http, base):
    """TC-SETTINGS-02: GET /api/config."""
    print("\n── TC-SETTINGS-02: GET /api/config ──")
    r = http.get(f"{base}/api/config")
    if not assert_status(r, 200):
        return
    data = r.json()
    missing = [f for f in ("log_level", "http") if f not in data]
    ok(f"config: log_level={data.get('log_level')}" if not missing else f"missing: {missing}")


def test_tc_setup_01_config(http, base):
    """TC-SETUP-01: GET /api/agents/{id}/config (must be 200 once runtime is ready)."""
    print(f"\n── TC-SETUP-01: GET /api/agents/{AGENT_ID}/config ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/config")
    if r.status_code != 200:
        fail(f"config: HTTP {r.status_code} {r.text[:200]}")
        return
    data = r.json()
    ok(f"config sections: {list(data.keys())[:8]}" if data else "empty config")


def test_tc_setup_05_mcp(http, base):
    """TC-SETUP-05: GET /api/agents/{id}/mcp-servers (must be 200 once runtime is ready)."""
    print(f"\n── TC-SETUP-05: GET /api/agents/{AGENT_ID}/mcp-servers ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/mcp-servers")
    assert_status(r, 200, "mcp-servers")


def test_tc_setup_06_search_providers(http, base):
    """TC-SETUP-06: GET /api/agents/{id}/search-providers (may be empty list)."""
    print(f"\n── TC-SETUP-06: GET /api/agents/{AGENT_ID}/search-providers ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/search-providers")
    if r.status_code == 200:
        data = r.json()
        provs = data.get("providers", []) if isinstance(data, dict) else data
        ok(f"search providers: {len(provs)}")
    else:
        fail(f"search-providers: HTTP {r.status_code} {r.text[:200]}")


def test_tc_setup_07_model(http, base):
    """TC-SETUP-07: GET /api/agents/{id}/model."""
    print(f"\n── TC-SETUP-07: GET /api/agents/{AGENT_ID}/model ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/model")
    if r.status_code == 200:
        ok(f"model: {r.json()}")
    else:
        fail(f"model: HTTP {r.status_code} {r.text[:200]}")


def test_tc_harness_01_providers(http, base):
    """TC-HARNESS-01: GET /api/providers (api_key masked)."""
    print("\n── TC-HARNESS-01: GET /api/providers ──")
    r = http.get(f"{base}/api/providers")
    if not assert_status(r, 200):
        return
    data = r.json()
    providers = data if isinstance(data, list) else data.get("providers", [])
    ok(f"{len(providers)} providers")
    for p in providers[:3]:
        key = str(p.get("api_key", ""))
        if key and key != "***":
            fail(f"api_key not masked: {key}")


def test_tc_harness_04_models(http, base):
    """TC-HARNESS-04: GET /api/models."""
    print("\n── TC-HARNESS-04: GET /api/models ──")
    r = http.get(f"{base}/api/models")
    if not assert_status(r, 200):
        return
    models = r.json().get("models", [])
    ok(f"{len(models)} models available")


def test_tc_harness_05_embedding(http, base):
    """TC-HARNESS-05: GET /api/embedding-models."""
    print("\n── TC-HARNESS-05: GET /api/embedding-models ──")
    r = http.get(f"{base}/api/embedding-models")
    if not assert_status(r, 200):
        return
    data = r.json()
    count = len(data) if isinstance(data, list) else len(data.get("models", []))
    ok(f"embedding models: {count}")


def test_tc_harness_06_mcp(http, base):
    """TC-HARNESS-06: GET /api/mcp-catalog (env masked)."""
    print("\n── TC-HARNESS-06: GET /api/mcp-catalog ──")
    r = http.get(f"{base}/api/mcp-catalog")
    if not assert_status(r, 200):
        return
    data = r.json()
    entries = data if isinstance(data, list) else data.get("entries", [])
    ok(f"{len(entries)} MCP entries")


def test_tc_harness_07_search_keys(http, base):
    """TC-HARNESS-07: GET /api/search/keys (masked)."""
    print("\n── TC-HARNESS-07: GET /api/search/keys ──")
    r = http.get(f"{base}/api/search/keys")
    assert_status(r, 200, "search keys")


def test_tc_skill_01_list(http, base):
    """TC-SKILL-01: GET /api/agents/{id}/skills (empty list ok)."""
    print(f"\n── TC-SKILL-01: GET /api/agents/{AGENT_ID}/skills ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/skills")
    if r.status_code == 200:
        skills = r.json().get("skills", [])
        ok(f"{len(skills)} skills")
    else:
        fail(f"skills: HTTP {r.status_code} {r.text[:200]}")


def test_tc_skill_02_detail(http, base, ctx):
    """TC-SKILL-02: GET /api/agents/{id}/skills/{name}."""
    print(f"\n── TC-SKILL-02: GET /api/agents/{AGENT_ID}/skills/{{name}} ──")
    name = ctx.get("skill_name")
    if not name:
        skip("no skill listed in TC-SKILL-01")
        return
    r = http.get(f"{base}/api/agents/{AGENT_ID}/skills/{name}")
    if r.status_code == 200:
        data = r.json()
        ok(f"skill detail: {list(data.keys())[:5]}")
    else:
        fail(f"skill detail: HTTP {r.status_code} {r.text[:200]}")


def test_tc_skill_03_history(http, base, ctx):
    """TC-SKILL-03: GET /api/agents/{id}/skills/{name}/history (empty ok)."""
    print(f"\n── TC-SKILL-03: history ──")
    name = ctx.get("skill_name")
    if not name:
        skip("no skill listed")
        return
    r = http.get(f"{base}/api/agents/{AGENT_ID}/skills/{name}/history")
    assert_status(r, 200, "skill history")


def test_tc_ws_01_list(http, base):
    """TC-WS-01: GET /api/agents/{id}/workspaces (read-only).

    A freshly installed agent has no workspace config, so an empty list
    is the normal state — the assertion is 200 + well-formed envelope.
    """
    print(f"\n── TC-WS-01: GET /api/agents/{AGENT_ID}/workspaces ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/workspaces")
    if r.status_code != 200:
        fail(f"workspaces: HTTP {r.status_code} {r.text[:200]}")
        return
    data = r.json()
    count = len(data) if isinstance(data, list) else len(data.get("workspaces", []))
    ok(f"workspaces endpoint live ({count} entries)")


def test_tc_mem_01_nodes(http, base):
    """TC-MEM-01: GET /api/agents/{id}/memory/nodes."""
    print(f"\n── TC-MEM-01: GET /api/agents/{AGENT_ID}/memory/nodes ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/memory/nodes", params={"page": 1, "size": 20})
    if r.status_code != 200:
        fail(f"memory nodes: HTTP {r.status_code} {r.text[:200]}")
        return
    data = r.json()
    nodes = data.get("nodes", []) if isinstance(data, dict) else data
    ok(f"memory nodes: total={data.get('total', len(nodes) if isinstance(nodes, list) else '?')}")


def test_tc_mem_02_stats(http, base):
    """TC-MEM-02: GET /api/agents/{id}/memory/stats."""
    print(f"\n── TC-MEM-02: GET /api/agents/{AGENT_ID}/memory/stats ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/memory/stats")
    if r.status_code == 200:
        ok(f"memory stats: {list(r.json().keys())}")
    else:
        fail(f"memory stats: HTTP {r.status_code} {r.text[:200]}")


def test_tc_lsp_01_endpoint(http, base):
    """TC-LSP-01: GET /api/agents/{id}/lsp-endpoint (Phase 4 contract)."""
    print(f"\n── TC-LSP-01: GET /api/agents/{AGENT_ID}/lsp-endpoint ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/lsp-endpoint")
    if r.status_code == 200:
        data = r.json()
        ok(f"lsp-endpoint: {data}")
    else:
        fail(f"lsp-endpoint: HTTP {r.status_code} {r.text[:200]}")


def test_tc_mqtt_broker(http, base):
    """MQTT broker reachable + pub/sub roundtrip."""
    print("\n── MQTT Broker Smoke ──")
    mq = MqttClient()
    try:
        if not mq.connect():
            fail(f"MQTT broker refused: CONNACK {mq.connack_rc}")
            return
        mq.subscribe("acowork/global/#", qos=1)
        time.sleep(0.3)
        mq.publish("acowork/global/_smoke_test", "ping", qos=1)
        if mq.wait_for("acowork/global/_smoke_test", timeout=2):
            ok("MQTT pub/sub roundtrip")
        else:
            fail("MQTT roundtrip: no echo received")
    finally:
        mq.disconnect()


def test_tc_harness_08_provider_key_mqtt_sync(http, base, mqtt_port):
    """TC-HARNESS-08: provider API key must ride the MQTT providers payload.

    Regression: the Gateway published its initial retained
    acowork/global/providers BEFORE the dev-mode vault auto-unlock
    finished, so every ProviderRef.api_key was empty and nothing
    triggered a republish afterwards — Runtimes got no credentials and
    chat failed with "invalid api key". The key must appear decrypted
    in the MQTT payload right after the provider is created.
    """
    print("\n── TC-HARNESS-08: provider api_key sync via MQTT ──")
    pid = f"smoke-key-sync-{random_suffix()}"
    key = f"sk-smoke-{random_suffix()}"
    r = http.post(f"{base}/api/providers",
                  json={"provider": pid, "key": key,
                        "base_url": "https://api.example.com/v1"})
    if r.status_code not in (200, 201):
        fail(f"provider create: HTTP {r.status_code} {r.text[:200]}")
        return
    ok(f"provider created: {pid}")
    mq = MqttClient()
    if not mq.connect(port=mqtt_port):
        fail("MQTT broker refused")
        http.delete(f"{base}/api/providers/{pid}")
        return
    mq.subscribe("acowork/global/providers", qos=1)
    deadline = time.time() + 10
    seen = None
    while time.time() < deadline:
        payload = mq.wait_for("acowork/global/providers", timeout=1)
        if payload:
            _, provs = decode_available_providers_from_envelope(payload)
            for p in provs:
                if p["id"] == pid:
                    seen = p
                    break
        if seen:
            break
    mq.disconnect()
    http.delete(f"{base}/api/providers/{pid}")
    if not seen:
        fail(f"provider {pid} missing from MQTT acowork/global/providers within 10s")
        return
    if seen["api_key"] == key:
        ok(f"api_key rides the MQTT payload decrypted ({len(key)} chars)")
    else:
        fail(f"api_key mismatch on MQTT: {seen['api_key'][:8]!r}… != {key[:8]!r}…"
             f" (vault unlock republish regression?)")


# ── Test Cases — session & chat (needs Runtime online) ──────────────────

def test_tc_chat_02_start_agent(http, base, mqtt_port):
    """TC-CHAT-02: POST start → MQTT status=online + meta."""
    print(f"\n── TC-CHAT-02: Start {AGENT_ID} via Gateway ──")
    mq = MqttClient()
    if not mq.connect(port=mqtt_port):
        fail("MQTT broker refused")
        return False
    mq.subscribe(f"acowork/agents/{AGENT_ID}/status", qos=1)
    mq.subscribe(f"acowork/agents/{AGENT_ID}/meta", qos=1)
    time.sleep(0.3)
    r = http.post(f"{base}/api/agents/{AGENT_ID}/start", json={})
    if r.status_code not in (200, 409):
        fail(f"Agent start: HTTP {r.status_code} {r.text[:200]}")
        mq.disconnect()
        return False
    ok(f"Agent start: HTTP {r.status_code}")
    payload = mq.wait_for(f"acowork/agents/{AGENT_ID}/status", timeout=40,
                          predicate=lambda p: b"online" in p)
    if payload:
        ok(f"Agent status: {payload.decode()}")
    else:
        fail("Agent status: no online message received (40s)")
        mq.disconnect()
        return False
    meta = mq.wait_for(f"acowork/agents/{AGENT_ID}/meta", timeout=10)
    if meta:
        ok(f"Agent meta received ({len(meta)} bytes)")
    mq.disconnect()
    return True


def wait_runtime_ready(http, base, timeout=30):
    """Wait until the agent's runtime-backed services are live.

    Phase A publishes MQTT status=online as soon as the HTTP server
    binds, but the workspace/memory/config usecase services are
    late-bound at the END of Phase B — a request in between answers
    503 "workspace service not ready". A real Desktop user never
    clicks inside that window; poll the first proxied endpoint until
    it serves.
    """
    print("\n── wait runtime services ready ──")
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        last = http.get(f"{base}/api/agents/{AGENT_ID}/workspaces")
        if last.status_code == 200:
            ok("runtime services ready")
            return True
        time.sleep(0.5)
    origin = last.headers.get("x-error-origin", "gateway")
    fail(f"runtime services not ready after {timeout}s (last HTTP {last.status_code}, "
         f"origin={origin}, body={last.text[:200]})")
    return False


def test_tc_chat_06_create_session(mq, base):
    """TC-CHAT-06: MQTT control/create_session → sessions/created (sid in payload)."""
    print("\n── TC-CHAT-06: Create session (MQTT control) ──")
    topic = f"acowork/agents/{AGENT_ID}/sessions/created"
    mq.subscribe(topic, qos=1)
    time.sleep(0.3)
    mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/create_session",
               encode_control("create_session", AGENT_ID), qos=1)
    payload = mq.wait_for(topic, timeout=10)
    if not payload:
        fail("no sessions/created event received")
        return None
    ev = decode_envelope_payload(payload)
    if ev[0] != 30:
        fail(f"unexpected payload oneof {ev[0]}")
        return None
    created = decode_session_created(ev[1])
    if not created["session_id"]:
        fail(f"created event missing sid: {created}")
        return None
    ok(f"session created: sid={created['session_id']} title={created['title']}")
    return created["session_id"]


def test_tc_chat_05_messages(http, base, sid):
    """TC-CHAT-05: GET /api/agents/{id}/sessions/{sid}/messages."""
    print(f"\n── TC-CHAT-05: GET messages ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/sessions/{sid}/messages")
    if not assert_status(r, 200):
        return
    data = r.json()
    msgs = data.get("messages", [])
    ok(f"messages: {len(msgs)} (metadata: {list(data.get('metadata', {}).keys())})")


def test_tc_chat_09_rename_session(http, base, mq, sid):
    """TC-CHAT-09: MQTT control/update_title → HTTP session shows new title."""
    print("\n── TC-CHAT-09: Rename session ──")
    title = f"smoke-renamed-{random_suffix()}"
    mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/update_title",
               encode_control("update_title", AGENT_ID, session_id=sid, title=title), qos=1)
    deadline = time.time() + 10
    while time.time() < deadline:
        # ADR-047: the title is a session config field, so get_session
        # no longer carries it — it lives in GET /sessions/{sid}/config.
        r = http.get(f"{base}/api/agents/{AGENT_ID}/sessions/{sid}/config")
        if r.status_code == 200 and r.json().get("title") == title:
            ok(f"session title updated: {title}")
            return
        time.sleep(0.3)
    fail(f"session title not updated to {title}")


def test_tc_chat_10_delete_session(mq, base, http, sid):
    """TC-CHAT-10: MQTT control/delete_session → sessions/deleted + list confirms."""
    print("\n── TC-CHAT-10: Delete session ──")
    topic = f"acowork/agents/{AGENT_ID}/sessions/deleted"
    mq.subscribe(topic, qos=1)
    time.sleep(0.3)
    mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/delete_session",
               encode_control("delete_session", AGENT_ID, session_id=sid), qos=1)
    payload = mq.wait_for(topic, timeout=10)
    if payload:
        ev = decode_envelope_payload(payload)
        deleted = decode_session_deleted(ev[1]) if ev[0] == 31 else {}
        ok(f"sessions/deleted: sid={deleted.get('session_id')}")
    else:
        fail("no sessions/deleted event")
        return
    r = http.get(f"{base}/api/agents/{AGENT_ID}/sessions")
    if r.status_code == 200:
        sids = [s.get("session_id") for s in r.json().get("sessions", [])]
        if sid not in sids:
            ok("session gone from list")
        else:
            fail("session still listed")


def test_tc_chat_11_stop_agent(http, base, mqtt_port):
    """TC-CHAT-11: POST stop → MQTT status=offline."""
    print(f"\n── TC-CHAT-11: Stop {AGENT_ID} ──")
    mq = MqttClient()
    mq.connect(port=mqtt_port)
    mq.subscribe(f"acowork/agents/{AGENT_ID}/status", qos=1)
    time.sleep(0.3)
    r = http.post(f"{base}/api/agents/{AGENT_ID}/stop", json={})
    assert_status(r, 200, "Agent stop")
    payload = mq.wait_for(f"acowork/agents/{AGENT_ID}/status", timeout=15,
                          predicate=lambda p: b"offline" in p)
    if payload:
        ok("Agent status: offline")
    else:
        fail("no offline status received")
    mq.disconnect()


# ── Test Cases — LLM-dependent (skipped unless SMOKE_LLM=1) ─────────────

def _collect_message_flow(mq, sid, timeout=60):
    """Send a chat message and collect messages/* until done/error/stopped.
    Returns (ok: bool, detail: str)."""
    prefix = f"acowork/agents/{AGENT_ID}/sessions/{sid}/messages/"
    mid = f"smoke-msg-{random_suffix()}"
    mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/message",
               encode_control("message", AGENT_ID, session_id=sid,
                              message_id=mid, content="Reply with just: OK"),
               qos=1)
    chunks = 0
    deadline = time.time() + timeout
    while time.time() < deadline:
        topic, payload = mq.wait_any([prefix], timeout=5)
        if not topic:
            continue
        ev = decode_envelope_payload(payload)
        if ev[0] != 34:
            continue
        msg = decode_session_message(ev[1])
        if msg["event"] == "chunk" and msg["delta"]:
            chunks += 1
        elif msg["event"] == "done":
            return True, f"done after {chunks} chunk(s)"
        elif msg["event"] == "error":
            return False, f"messages/error: {msg['error']}"
        elif msg["event"] == "stopped":
            return False, "stopped before done"
    return False, f"timeout ({timeout}s)"


def test_tc_chat_07_send_message(mq, sid):
    """TC-CHAT-07: send message → chunk(s) → done (needs LLM provider)."""
    print("\n── TC-CHAT-07: Send message, await chunk→done ──")
    if not LLM_ENABLED:
        skip("LLM case — set SMOKE_LLM=1 to enable")
        return
    mq.subscribe(f"acowork/agents/{AGENT_ID}/sessions/{sid}/messages/#", qos=0)
    ok_flow, detail = _collect_message_flow(mq, sid)
    if ok_flow:
        ok(f"message flow: {detail}")
    else:
        fail(f"message flow failed: {detail}")


def test_tc_chat_08_tool_call(mq, sid):
    """TC-CHAT-08: trigger a tool call (needs LLM + a reading tool enabled).

    The prompt asks for a file read; which tool answers it is the agent
    choice, so this case only asserts that a tool_call round-trips.
    """
    print("\n── TC-CHAT-08: tool call ──")
    if not LLM_ENABLED:
        skip("LLM case — set SMOKE_LLM=1 to enable")
        return
    prefix = f"acowork/agents/{AGENT_ID}/sessions/{sid}/messages/"
    mid = f"smoke-tool-{random_suffix()}"
    mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/message",
               encode_control("message", AGENT_ID, session_id=sid, message_id=mid,
                              content="Read the file /etc/hostname and tell me its content"),
               qos=1)
    deadline = time.time() + 90
    saw_tool = False
    while time.time() < deadline:
        topic, payload = mq.wait_any([prefix], timeout=5)
        if not topic:
            continue
        ev = decode_envelope_payload(payload)
        if ev[0] != 34:
            continue
        msg = decode_session_message(ev[1])
        if msg["event"] == "tool_call":
            saw_tool = True
            ok(f"tool_call: {msg['tool_name']}")
        elif msg["event"] == "done":
            ok("done after tool flow" if saw_tool else "done (no tool_call — check the agent tool config)")
            return
        elif msg["event"] == "error":
            fail(f"messages/error: {msg['error']}")
            return
    fail("no done within 90s")


def test_tc_model_01_switch(http, base, mq, sid):
    """TC-MODEL-01: control/model_switch → session state model field (no LLM needed)."""
    print("\n── TC-MODEL-01: Model switch ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/sessions/{sid}")
    if r.status_code != 200:
        skip(f"session detail HTTP {r.status_code}")
        return
    model = (r.json().get("model") or {}).get("id") if isinstance(r.json().get("model"), dict) else r.json().get("model")
    if not model:
        skip("session state carries no model field")
        return
    mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/model_switch",
               encode_control("model_switch", AGENT_ID, session_id=sid, model_id=model), qos=1)
    ok(f"model_switch published (idempotent to current model {model})")


def test_tc_model_02_reasoning_effort(http, base, mq, sid):
    """TC-MODEL-02: control/reasoning_effort → session state field."""
    print("\n── TC-MODEL-02: Reasoning effort ──")
    mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/reasoning_effort",
               encode_control("reasoning_effort", AGENT_ID, session_id=sid, effort="medium"), qos=1)
    ok("reasoning_effort published")


def test_tc_flow_01_stop(mq, sid):
    """TC-FLOW-01: chunk → control/stop → messages/stopped (needs LLM)."""
    print("\n── TC-FLOW-01: Interrupt generation ──")
    if not LLM_ENABLED:
        skip("LLM case — set SMOKE_LLM=1 to enable")
        return
    prefix = f"acowork/agents/{AGENT_ID}/sessions/{sid}/messages/"
    mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/message",
               encode_control("message", AGENT_ID, session_id=sid,
                              message_id=f"smoke-flow-{random_suffix()}",
                              content="Write a very long essay about the history of computing (keep going for a while)"),
               qos=1)
    deadline = time.time() + 60
    while time.time() < deadline:
        topic, payload = mq.wait_any([prefix], timeout=5)
        if not topic:
            continue
        ev = decode_envelope_payload(payload)
        if ev[0] != 34:
            continue
        msg = decode_session_message(ev[1])
        if msg["event"] == "chunk" and msg["delta"]:
            ok("first chunk received — sending stop")
            mq.publish(f"acowork/agents/{AGENT_ID}/sessions/control/stop",
                       encode_control("stop", AGENT_ID, session_id=sid), qos=1)
            stopped = mq.wait_any([prefix], timeout=10,
                                  predicate=lambda p: decode_envelope_payload(p)[0] == 34
                                  and decode_session_message(decode_envelope_payload(p)[1])["event"] == "stopped")
            if stopped[0]:
                ok("messages/stopped received")
            else:
                fail("no messages/stopped within 10s")
            return
        elif msg["event"] == "done":
            skip("flow finished before we could stop")
            return
    skip("no chunk within 60s (no LLM response?)")


# ── Test Cases — write operations (create → use → delete) ───────────────

def test_tc_settings_03_06_log_level(http, base):
    """TC-SETTINGS-03+06: modify log_level → restore original."""
    print("\n── TC-SETTINGS-03/06: log_level modify→restore ──")
    r = http.get(f"{base}/api/config")
    if not assert_status(r, 200):
        return
    original = r.json().get("log_level")
    if not original:
        skip("no log_level in config")
        return
    r = http.put(f"{base}/api/config", json={"log_level": "debug"})
    if not assert_status(r, 200, "PUT log_level=debug"):
        return
    r = http.get(f"{base}/api/config")
    if r.status_code == 200 and r.json().get("log_level") == "debug":
        ok("log_level is debug")
    else:
        fail("log_level not applied")
    r = http.put(f"{base}/api/config", json={"log_level": original})
    assert_status(r, 200, f"restore log_level={original}")


def test_tc_settings_04_users_list(http, base):
    """TC-SETTINGS-04: GET /api/users."""
    print("\n── TC-SETTINGS-04: GET /api/users ──")
    r = http.get(f"{base}/api/users")
    assert_status(r, 200, "users list")


def test_tc_settings_05_07_user(http, base):
    """TC-SETTINGS-05+07: create → update test user profile.

    NOTE: the product exposes PUT /api/users/{user_id} (update) and
    POST /api/users/{user_id}/activate but NO DELETE endpoint, so the
    write-loop verification is create → update instead of create → delete.

    Also asserts the UserResponse contract { user, version } on both POST
    and PUT. The Desktop client unwraps `user` and chains further PUTs off
    the returned user_id — a stale envelope (profile polluted with the
    wrapper) made the follow-up PUT hit /api/users/undefined → 404 and
    silently discarded the language/city changes.
    """
    print("\n── TC-SETTINGS-05/07: user create→update ──")
    name = f"smoke-{random_suffix()}"
    r = http.post(f"{base}/api/users", json={"display_name": name})
    if r.status_code not in (200, 201):
        fail(f"user create: HTTP {r.status_code} {r.text[:200]}")
        return
    data = r.json()
    # ADR-059 §6: POST /api/users returns an OperationAck
    # {operation_id, state, resource_version} — no user envelope. The
    # new user's id is resolved from the list endpoint instead.
    if not isinstance(data.get("resource_version"), int):
        fail(f"create response missing int resource_version: {data}")
        return
    r = http.get(f"{base}/api/users")
    if r.status_code != 200:
        fail(f"user list after create: HTTP {r.status_code} {r.text[:200]}")
        return
    uid = next(
        (u.get("user_id") for u in r.json().get("users", [])
         if u.get("display_name") == name),
        None,
    )
    if not uid:
        fail(f"created user {name!r} not found in list: {r.text[:300]}")
        return
    ok(f"user created: {uid} (resource_version={data['resource_version']})")

    # PUT must return the same UserResponse envelope with the echoed user.
    r = http.put(f"{base}/api/users/{uid}", json={"display_name": f"{name}-renamed"})
    if r.status_code != 200:
        fail(f"user update: HTTP {r.status_code} — FIXUP NEEDED for {uid}")
        return
    data = r.json()
    if data.get("user", {}).get("user_id") != uid:
        fail(f"update response user.user_id mismatch: {data}")
        return
    if not isinstance(data.get("version"), int):
        fail(f"update response missing int version: {data}")
        return
    ok("user updated via PUT (UserResponse envelope)")

    # Chained field updates — mirrors ProfileTab.saveField: each PUT uses the
    # user_id from the previous response (regression for the frontend bug
    # where the envelope leaked into the profile and the follow-up PUT hit
    # /api/users/undefined → 404 → "save failed").
    r = http.put(f"{base}/api/users/{uid}", json={"city": "上海"})
    if r.status_code != 200 or r.json().get("user", {}).get("city") != "上海":
        fail(f"chained PUT city failed: HTTP {r.status_code} {r.text[:200]}")
        return
    r = http.put(f"{base}/api/users/{uid}", json={"language": "zh-CN"})
    if r.status_code != 200 or r.json().get("user", {}).get("language") != "zh-CN":
        fail(f"chained PUT language failed: HTTP {r.status_code} {r.text[:200]}")
        return
    ok("chained field updates persisted (city + language)")

    # Verify persistence through the list endpoint — what the Desktop reads
    # when the Settings view re-mounts. Stale values here are what flipped
    # the UI language back to English after switching views.
    r = http.get(f"{base}/api/users")
    if r.status_code != 200:
        fail(f"users list after update: HTTP {r.status_code}")
        return
    saved = next((u for u in r.json().get("users", []) if u.get("user_id") == uid), None)
    if not saved or saved.get("city") != "上海" or saved.get("language") != "zh-CN":
        fail(f"updated fields not persisted: {saved}")
        return
    ok("updated fields visible via GET /api/users")


def test_tc_harness_02_03_provider(http, base):
    """TC-HARNESS-02+03: create → delete test provider."""
    print("\n── TC-HARNESS-02/03: provider create→delete ──")
    pid = f"smoke-{random_suffix()}"
    # AddProviderRequest: provider + key (+ optional base_url) — the key
    # is stored in the encrypted Vault, config in provider_list.json.
    r = http.post(f"{base}/api/providers", json={
        "provider": pid, "key": "sk-smoke-test",
        "base_url": "https://api.example.com/v1",
    })
    if r.status_code not in (200, 201):
        fail(f"provider create: HTTP {r.status_code} {r.text[:200]}")
        return
    ok(f"provider created: {pid}")
    r = http.delete(f"{base}/api/providers/{pid}")
    if r.status_code in (200, 204):
        ok("provider deleted")
    else:
        fail(f"provider delete: HTTP {r.status_code} — FIXUP NEEDED for {pid}")


def test_tc_setup_02_04_tool_toggle(http, base, ctx):
    """TC-SETUP-02+04: toggle one builtin tool on/off, restore after.

    Probes the ADR-040-follow-up `/builtin-tools` surface (the one the
    ToolsTab uses) with `doc_reader`: unconditionally registered, not a
    protected tool, and not needed by any other case here — so the flip
    is observable without perturbing the rest of the run. `file_read`
    used to be the probe tool and is now deprecated/unregistered; an
    unknown name is silently dropped per ADR-029 §7, which would have
    turned this case into a no-op that still reported green.
    """
    print("\n── TC-SETUP-02/04: builtin tool toggle ──")

    def enabled_set():
        r = http.get(f"{base}/api/agents/{AGENT_ID}/builtin-tools")
        if r.status_code != 200:
            raise AssertionError(f"builtin-tools HTTP {r.status_code} {r.text[:200]}")
        entries = r.json().get("tools", [])
        state = {e["name"]: bool(e.get("enabled")) for e in entries}
        return state, {n for n, on in state.items() if on}

    try:
        before, before_on = enabled_set()
    except AssertionError as e:
        fail(str(e))
        return
    if PROBE_TOOL not in before:
        fail(f"{PROBE_TOOL} is not a registered builtin tool: {sorted(before)}")
        return
    ctx["builtin_tools_before"] = before

    def put(names, label):
        r = http.put(f"{base}/api/agents/{AGENT_ID}/builtin-tools",
                     json={"builtin_tools": sorted(names)})
        if not assert_status(r, 200, label):
            return None
        return {e["name"]: bool(e.get("enabled")) for e in r.json().get("tools", [])}

    # 1) turn the probe on (it may already be on — the assert still pins
    #    that the write is echoed back, which is what the ToolsTab relies on)
    got = put(before_on | {PROBE_TOOL}, f"enable {PROBE_TOOL}")
    if got is None or got.get(PROBE_TOOL) is not True:
        fail(f"{PROBE_TOOL} did not become enabled: {got and got.get(PROBE_TOOL)}")
        return
    # 2) restore the persisted set verbatim
    got = put(before_on, f"restore {PROBE_TOOL}={before[PROBE_TOOL]}")
    if got is None or got.get(PROBE_TOOL) != before[PROBE_TOOL]:
        fail(f"{PROBE_TOOL} did not restore to {before[PROBE_TOOL]}: {got and got.get(PROBE_TOOL)}")
        return
    ctx.pop("builtin_tools_before", None)
    ok(f"{PROBE_TOOL} enable→restore round-trip")


def test_tc_ws_02_add_workspace(http, base, ctx):
    """TC-WS-02: create test workspace (path /tmp)."""
    print("\n── TC-WS-02: Add workspace ──")
    ws_path = f"/tmp/acowork-smoke-{random_suffix()}"
    Path(ws_path).mkdir(parents=True, exist_ok=True)
    # create_workspace validates `path` + `access` as required fields
    # (400 when missing) and echoes the full entry — including `id` —
    # as the response body (no top-level workspace_id).
    r = http.post(f"{base}/api/agents/{AGENT_ID}/workspaces",
                  json={"path": ws_path, "access": "read-write",
                        "alias": f"smoke-{random_suffix()}"})
    if r.status_code not in (200, 201):
        fail(f"workspace create: HTTP {r.status_code} {r.text[:200]}")
        shutil.rmtree(ws_path, ignore_errors=True)
        return
    data = r.json()
    ws_id = data.get("id") or data.get("workspace_id")
    if not ws_id:
        fail(f"no workspace id in response: {data}")
        shutil.rmtree(ws_path, ignore_errors=True)
        return
    ok(f"workspace created: {ws_id}")
    ctx["ws_id"] = ws_id
    ctx["ws_path"] = ws_path


def test_tc_ws_03_tree(http, base, ctx):
    """TC-WS-03: GET tree (test workspace only)."""
    print("\n── TC-WS-03: Workspace tree ──")
    ws_id = ctx.get("ws_id")
    if not ws_id:
        skip("no test workspace")
        return
    r = http.get(f"{base}/api/agents/{AGENT_ID}/workspaces/tree",
                 params={"workspace_id": ws_id})
    if not assert_status(r, 200, "tree"):
        return
    entries = r.json().get("entries", [])
    ok(f"tree entries: {len(entries)}")


def test_tc_ws_04_07_file_crud(http, base, ctx):
    """TC-WS-04..07: create → read → write → delete temp file."""
    print("\n── TC-WS-04..07: file create/read/write/delete ──")
    ws_id = ctx.get("ws_id")
    if not ws_id:
        skip("no test workspace")
        return
    # Workspace-relative path (no leading slash): `PathBuf::join("/…")`
    # replaces the root and trips the traversal guard.
    path = f"smoke-{random_suffix()}.txt"
    base_url = f"{base}/api/agents/{AGENT_ID}/workspaces/file"
    params = {"workspace_id": ws_id}
    r = http.post(base_url, params=params, json={"path": path, "content": "smoke test content"})
    if not assert_status(r, 200, "file create"):
        return
    r = http.get(base_url, params={**params, "path": path})
    if r.status_code == 200 and r.json().get("content") == "smoke test content":
        ok("file read: content matches")
    else:
        fail(f"file read: HTTP {r.status_code}")
    r = http.put(base_url, params={**params, "path": path},
                 json={"content": "updated content"})
    if not assert_status(r, 200, "file write"):
        return
    r = http.get(base_url, params={**params, "path": path})
    if r.status_code == 200 and r.json().get("content") == "updated content":
        ok("file re-read: content updated")
    else:
        fail("file content not updated")
    # DELETE requires a JSON body (axum Json extractor → 415 without
    # one) and carries `path` in the BODY — the handler only forwards
    # `workspace_id` from the querystring. httpx's Client.delete() has
    # no `json` kwarg, so use request() directly.
    r = http.request("DELETE", base_url, params=params, json={"path": path})
    if not assert_status(r, 200, "file delete"):
        return
    r = http.get(base_url, params={**params, "path": path})
    if r.status_code in (404, 200):
        ok(f"file gone after delete (GET {r.status_code})")
    else:
        fail(f"file still readable: HTTP {r.status_code}")


def test_tc_ws_08_find(http, base, ctx):
    """TC-WS-08: find by pattern (may be empty result set)."""
    print("\n── TC-WS-08: Workspace find ──")
    ws_id = ctx.get("ws_id")
    if not ws_id:
        skip("no test workspace")
        return
    r = http.get(f"{base}/api/agents/{AGENT_ID}/workspaces/find",
                 params={"workspace_id": ws_id, "q": "smoke"})
    assert_status(r, 200, "find")


def test_tc_ws_09_delete_workspace(http, base, ctx):
    """TC-WS-09: delete test workspace + physical dir."""
    print("\n── TC-WS-09: Delete workspace ──")
    ws_id = ctx.get("ws_id")
    if not ws_id:
        skip("no test workspace")
        return
    r = http.delete(f"{base}/api/agents/{AGENT_ID}/workspaces/{ws_id}")
    if not assert_status(r, 200, "workspace delete"):
        return
    shutil.rmtree(ctx.get("ws_path", "/tmp/acowork-smoke-"), ignore_errors=True)
    r = http.get(f"{base}/api/agents/{AGENT_ID}/workspaces")
    if r.status_code == 200:
        ws_list = r.json() if isinstance(r.json(), list) else r.json().get("workspaces", [])
        # List entries are the on-disk additional_dirs records — `id`,
        # not `workspace_id`.
        sids = [w.get("id") for w in ws_list]
        ok("workspace gone from list" if ws_id not in sids else "workspace still listed!")
    ctx.pop("ws_id", None)


def test_tc_fsb_01_default_hides_dotfiles(http, base, ctx):
    """TC-FSB-01: `GET /api/fs/browse` default hides `.`-prefixed entries.

    Reuses the empty `ws_path` directory created by TC-WS-02 and seeds
    one regular + one hidden fixture. The default filter (no
    `show_hidden` query) must surface `regular.txt` and silently drop
    `.dotfile` — matching the historical RemoteFolderPicker behaviour.
    """
    print("\n── TC-FSB-01: fs/browse default (hide hidden) ──")
    ws_path = ctx.get("ws_path")
    if not ws_path:
        skip("no test workspace")
        return
    # Idempotent seed — FSB-02 also reads these.
    Path(ws_path, "regular.txt").write_text("visible", encoding="utf-8")
    Path(ws_path, ".dotfile").write_text("hidden", encoding="utf-8")

    r = http.get(f"{base}/api/fs/browse", params={"path": ws_path})
    if not assert_status(r, 200, "browse default"):
        return
    names = {e["name"] for e in r.json().get("entries", [])}
    if "regular.txt" in names and ".dotfile" not in names:
        ok("default listing includes regular.txt, excludes .dotfile")
    else:
        fail(f"default listing wrong: {sorted(names)}")


def test_tc_fsb_02_show_hidden_includes_dotfiles(http, base, ctx):
    """TC-FSB-02: `GET /api/fs/browse?show_hidden=true` surfaces dotfiles.

    Same fixtures as TC-FSB-01; the opt-in flag is what the Desktop
    `RemoteFolderPicker` toggles when the user flips the new "show
    hidden files" switch. Both `.dotfile` and `regular.txt` must be
    in the response.
    """
    print("\n── TC-FSB-02: fs/browse?show_hidden=true ──")
    ws_path = ctx.get("ws_path")
    if not ws_path:
        skip("no test workspace")
        return
    r = http.get(f"{base}/api/fs/browse",
                 params={"path": ws_path, "show_hidden": "true"})
    if not assert_status(r, 200, "browse show_hidden"):
        return
    names = {e["name"] for e in r.json().get("entries", [])}
    if ".dotfile" in names and "regular.txt" in names:
        ok("show_hidden listing includes both .dotfile and regular.txt")
    else:
        fail(f"show_hidden listing wrong: {sorted(names)}")


def test_tc_mem_03_consolidate(http, base):
    """TC-MEM-03: trigger consolidate (downgraded to 200 check per doc)."""
    print("\n── TC-MEM-03: Memory consolidate (trigger only) ──")
    r = http.post(f"{base}/api/agents/{AGENT_ID}/memory/consolidate",
                  json={"force": True, "retention_days": 0})
    assert_status(r, 200, "consolidate")


def test_tc_mem_04_create_delete_node(http, base):
    """TC-MEM-04: create → use → delete memory node (HTTP API)."""
    print("\n── TC-MEM-04: memory node create→delete ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/memory/nodes", params={"page": 1, "size": 100})
    if r.status_code != 200:
        fail(f"memory list HTTP {r.status_code} {r.text[:200]}")
        return
    before = len(r.json().get("nodes", []))
    marker = f"smoke-mem-{random_suffix()}"
    # CreateMemoryNodeBody requires `label` (422 when missing); the
    # response returns the minted node_id directly — memory nodes are
    # addressed by numeric id, not by content search.
    r = http.post(f"{base}/api/agents/{AGENT_ID}/memory/nodes",
                  json={"label": marker})
    if r.status_code not in (200, 201):
        fail(f"memory node create: HTTP {r.status_code} {r.text[:200]}")
        return
    nid = r.json().get("node_id")
    if not nid:
        fail(f"no node_id in create response: {r.json()}")
        return
    ok(f"memory node created: {nid}")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/memory/nodes/{nid}")
    if r.status_code == 200 and r.json().get("node_id") == nid:
        ok(f"node detail readable: {nid}")
    else:
        fail(f"node detail: HTTP {r.status_code} — FIXUP NEEDED for {nid}")
    r = http.delete(f"{base}/api/agents/{AGENT_ID}/memory/nodes/{nid}")
    if r.status_code in (200, 204):
        ok("node deleted")
    else:
        fail(f"node delete: HTTP {r.status_code} — FIXUP NEEDED for {nid}")


def test_tc_doc_01_02_upload_read(http, base, sid):
    """TC-DOC-01+02: upload file to session, read blob back (no delete endpoint;
    attachments are cleaned up with the session — doc §5.10)."""
    print("\n── TC-DOC-01/02: attachment upload + read ──")
    if not sid:
        skip("no session")
        return
    content = f"smoke attachment {random_suffix()}".encode()
    r = http.post(f"{base}/api/agents/{AGENT_ID}/sessions/{sid}/files",
                  files={"file": ("smoke.txt", content, "text/plain")})
    if r.status_code not in (200, 201):
        fail(f"upload: HTTP {r.status_code} {r.text[:200]}")
        return
    doc_id = r.json().get("documentId")
    ok(f"uploaded: {doc_id} ({r.json().get('sizeBytes')} bytes)")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/files/{doc_id}")
    if r.status_code == 200 and r.content == content:
        ok("blob read: bytes match")
    else:
        fail(f"blob read: HTTP {r.status_code}")


def test_tc_doc_03_reference(mq, sid):
    """TC-DOC-03: message referencing attachment (needs LLM)."""
    print("\n── TC-DOC-03: message with attached file ──")
    if not LLM_ENABLED:
        skip("LLM case — set SMOKE_LLM=1 to enable")
        return
    if not sid:
        skip("no session")
        return
    mq.subscribe(f"acowork/agents/{AGENT_ID}/sessions/{sid}/messages/#", qos=0)
    ok_flow, detail = _collect_message_flow(mq, sid)
    ok(f"attached message flow: {detail}" if ok_flow else f"attached flow failed: {detail}")


def test_tc_chat_03_session_list(http, base):
    """TC-CHAT-03: GET /api/agents/{id}/sessions (must be 200 once runtime is ready)."""
    print(f"\n── TC-CHAT-03: GET /api/agents/{AGENT_ID}/sessions ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/sessions")
    if not assert_status(r, 200):
        return
    sessions = r.json().get("sessions", [])
    ok(f"sessions: {len(sessions)}")


def test_tc_chat_04_latest_session(http, base):
    """TC-CHAT-04: GET /api/agents/{id}/latest-session.

    This is the FIRST call the frontend makes when the user clicks an
    agent (the "loading session" spinner). A 403 here (e.g. node token
    mismatch) makes the UI poll forever — so any non-200 is a FAIL, not
    a skip. A fresh home legitimately has no session yet: an empty
    session_id with HTTP 200 is fine.
    """
    print(f"\n── TC-CHAT-04: latest-session ──")
    r = http.get(f"{base}/api/agents/{AGENT_ID}/latest-session")
    if r.status_code != 200:
        fail(f"latest-session: HTTP {r.status_code} {r.text[:200]}"
             f" (frontend would hang on 'loading session')")
        return
    data = r.json()
    sid = data.get("session_id")
    ok(f"latest session: {sid}" if sid else "no session_id yet (fresh home, 200 OK)")


# ── Test Cases — Phase 5a auth (isolated auth-enabled Gateway) ──────────

def test_tc_auth_01_create_token(gw_bin, auth_home):
    """TC-AUTH-01: `nodes token create` prints plaintext once (run before daemon)."""
    print("\n── TC-AUTH-01: nodes token create ──")
    r = subprocess.run(
        [str(gw_bin), "nodes", "token", "create", "--ttl", "10m"],
        env={**os.environ, "ACOWORK_HOME": str(auth_home)},
        capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=15,
    )
    if r.returncode != 0:
        fail(f"nodes token create failed: {r.stderr[:300]}")
        return None
    # The CLI prints an ASCII-art banner + ANSI color codes before the
    # token line — match the 64-hex plaintext instead of a fixed line.
    token = re.search(r"[0-9a-f]{64}", r.stdout)
    if not token:
        fail(f"no 64-hex token in output: {r.stdout[:300]}")
        return None
    ok(f"enrollment token issued: {token.group(0)[:8]}…")
    return token.group(0)


def test_tc_auth_03_reject_anonymous(port):
    """TC-AUTH-03: credential-less CONNECT must be rejected (CONNACK error)."""
    print("\n── TC-AUTH-03: anonymous CONNECT rejected ──")
    mq = MqttClient()  # desktop identity without password
    connected = mq.connect(port=port, timeout=8)
    rc = mq.connack_rc
    mq.disconnect()
    if not connected and rc != 0:
        ok(f"CONNECT rejected (CONNACK {rc})")
    else:
        fail(f"anonymous CONNECT accepted on auth broker (connack={rc})")


def test_tc_auth_02_enroll_reconnect(node_bin, auth_gw, mqtt_port, token, node_home, proxy_port):
    """TC-AUTH-02: full enroll loop against the REAL Gateway dispatch:
    node --token → enroll → enroll_result → identity.json node_token →
    kill & restart without token → reconnects with persisted credential."""
    print("\n── TC-AUTH-02: enroll → node_token → credential reconnect ──")
    # username must be non-None or paho skips username_pw_set entirely,
    # leaving the observer credential-less on an auth-enabled broker.
    observer = MqttClient(username="desktop", password=auth_gw.http_token)
    if not observer.connect(port=mqtt_port, timeout=8):
        fail(f"observer CONNECT failed (connack={observer.connack_rc}) — http_token from {auth_gw.home}/data")
        return None
    # node_id is a UUID generated at first start and persisted to
    # identity.json — the enroll/status topics use that UUID, not --name.
    # Subscribe to the wildcard first (so no message is missed while the
    # UUID is being discovered), then read the UUID from identity.json.
    for t in ("acowork/nodes/+/enroll", "acowork/nodes/+/enroll_result",
              "acowork/nodes/+/status"):
        observer.subscribe(t, qos=1)
    time.sleep(0.3)

    def spawn_node(extra):
        return subprocess.Popen(
            [str(node_bin), "start", "--gateway-host", "127.0.0.1", "--gateway-mqtt-port",
             str(mqtt_port), "--name", NODE_ID, "--proxy-port", str(proxy_port), *extra,
             "--home", str(node_home)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )

    child = spawn_node(["--token", token])
    try:
        node_id = wait_identity_node_id(node_home, timeout=20)
        if not node_id:
            fail("identity.json node_id (UUID) never appeared")
            return None
        ok(f"node identity: node_id={node_id[:8]}… node_name={NODE_ID}")
        enroll_topic = f"acowork/nodes/{node_id}/enroll"
        result_topic = f"acowork/nodes/{node_id}/enroll_result"
        status_topic = f"acowork/nodes/{node_id}/status"
        payload = observer.wait_for(enroll_topic, timeout=20)
        if not payload:
            fail("no enroll request received")
            return None
        enroll = decode_node_enroll(decode_envelope_payload(payload)[1])
        # The enrollment token is consumed at the MQTT CONNECT layer (the
        # broker's `node:{id}` rule) and is NOT echoed back inside the
        # enroll_request body — only node_id (UUID) travels there.
        if enroll.get("node_id") != node_id:
            fail(f"enroll payload node_id mismatch: {enroll.get('node_id', '')!r}")
            return None
        ok(f"enroll received: node_id={enroll['node_id'][:8]}…")

        payload = observer.wait_for(result_topic, timeout=20)
        if not payload:
            fail("no enroll_result from Gateway dispatch")
            return None
        result = decode_node_enroll_result(decode_envelope_payload(payload)[1])
        if not result.get("node_token"):
            fail(f"enroll_result without node_token: {result}")
            return None
        ok(f"enroll_result: status={result['status']} node_token={result['node_token'][:8]}…")

        persisted = wait_identity_token(node_home, timeout=15)
        if persisted != result["node_token"]:
            fail("identity.json node_token mismatch")
            return None
        ok("node_token persisted to identity.json")

        # Hard kill → restart WITHOUT the enrollment token: reconnect must
        # succeed via the persisted node_token (broker `node:{id}` rule).
        child.kill()
        child.wait()
        child2 = spawn_node([])
        status = observer.wait_for(status_topic, timeout=20,
                                   predicate=lambda p: b"online" in p)
        if status:
            ok("restart reconnected with node_token (status=online)")
        else:
            fail("restart did NOT reconnect (no online status)")
        child2.kill()
        child2.wait()
        return result["node_token"]
    finally:
        if child.poll() is None:
            child.kill()
            child.wait()
        observer.disconnect()


def test_tc_auth_04_download_auth(http, base, node_token):
    """TC-AUTH-04: package download requires X-ACowork-Node-Token on auth broker."""
    print("\n── TC-AUTH-04: package download auth ──")
    url = f"{base}/api/packages/{AGENT_ID}/download"
    r_anon = http.get(url)
    r_wrong = http.get(url, headers={"X-ACowork-Node-Token": "smoke-wrong-token"})
    r_right = http.get(url, headers={"X-ACowork-Node-Token": node_token})
    if r_anon.status_code in (401, 403):
        ok(f"no header → {r_anon.status_code}")
    else:
        fail(f"no header → {r_anon.status_code} (expected 401/403)")
    if r_wrong.status_code in (401, 403):
        ok(f"wrong token → {r_wrong.status_code}")
    else:
        fail(f"wrong token → {r_wrong.status_code} (expected 401/403)")
    if r_right.status_code == 200:
        ok(f"valid node_token → 200 ({len(r_right.content)} bytes)")
    else:
        fail(f"valid node_token → {r_right.status_code} (expected 200)")


def run_auth_suite(gw_bin, node_bin, http):
    """Phase 5a scenarios on an isolated auth-enabled Gateway instance."""
    print("\n" + "=" * 60)
    print("Phase 5a Auth Suite (isolated instance, auth_enabled=true)")
    print("=" * 60)
    auth_home = Path(tempfile.mkdtemp(prefix="acowork-smoke-auth-"))
    token = test_tc_auth_01_create_token(gw_bin, auth_home)
    if not token:
        return
    agw = Gateway(gw_bin, auth_home, AUTH_HTTP_PORT, AUTH_MQTT_PORT, auth_enabled=True)
    if not agw.start():
        agw.stop()
        return
    try:
        if not ensure_agent_installed(http, agw.base):
            fail("auth instance: agent install failed")
            return
        test_tc_auth_03_reject_anonymous(AUTH_MQTT_PORT)
        node_home = Path(tempfile.mkdtemp(prefix="acowork-smoke-node-"))
        node_token = test_tc_auth_02_enroll_reconnect(
            node_bin, agw, AUTH_MQTT_PORT, token, node_home, 19781)
        if node_token:
            test_tc_auth_04_download_auth(http, agw.base, node_token)
    finally:
        agw.stop()


def run_recovery_suite(gw_bin, node_bin, http):
    """Stale-token recovery (regression for the 'loading session' 403 bug).

    Scenario: the node holds an orphan node_token in its identity.json
    (left by an earlier auth-enabled run) while the Gateway's
    node_tokens store is empty and auth_enabled=false. The node must
    re-enroll with the persisted token, the Gateway must mint/store a
    fresh token, both sides must converge, and proxied requests must
    NOT answer 403 — otherwise the frontend polls latest-session forever.
    """
    print("\n" + "=" * 60)
    print("Recovery Suite (stale node token + empty gateway store)")
    print("=" * 60)
    home = Path(tempfile.mkdtemp(prefix="acowork-smoke-recover-"))
    gw = Gateway(gw_bin, home, AUTH_HTTP_PORT, AUTH_MQTT_PORT, auth_enabled=False)
    # Pre-seed the node home (the Gateway spawns its local node against
    # `ACOWORK_NODE_HOME = <home>/node`) with a stale enrolled identity.
    stale_token = "a" * 64
    stale_node_id = str(uuid.uuid4())
    node_dir = Path(gw.home) / "node"
    node_dir.mkdir(parents=True, exist_ok=True)
    (node_dir / "identity.json").write_text(json.dumps({
        "node_id": stale_node_id,
        "node_name": "smoke-node",
        "gateway_managed": True,
        "node_token": stale_token,
        "enrollment": "enrolled",
        "created_at": "2026-08-01T00:00:00Z",
    }))
    if not gw.start():
        gw.stop()
        return
    try:
        # 1. The gateway store must converge with the node identity.
        store_path = Path(gw.home) / "data" / "node_tokens.json"
        deadline = time.time() + TIMEOUT
        store_token = None
        while time.time() < deadline:
            if store_path.exists():
                try:
                    store_token = json.loads(store_path.read_text()).get(stale_node_id, {}).get("token")
                except Exception:
                    pass
                if store_token:
                    break
            time.sleep(0.5)
        if not store_token:
            fail("node never re-enrolled: gateway node_tokens.json empty")
            return
        ok(f"gateway store re-synced node token: {store_token[:8]}…")
        if store_token == stale_token:
            fail("gateway re-used the stale token — expected a fresh mint")
        else:
            ok("gateway minted a fresh token (stale one replaced)")
        # 2. The node identity must be updated to match (never-overwrite fix).
        identity_token = wait_identity_token(node_dir, timeout=15)
        if identity_token == store_token:
            ok("node identity.json converged to the gateway token")
        else:
            fail(f"identity.json token {identity_token and identity_token[:8]!r} != store {store_token[:8]!r}")
        # 3. An authenticated API call must answer 200 again (the frontend's
        #    post-reload path). `/api/agents` is the lightest one that needs
        #    no agent on this home — ADR-077 removed the auto-installed
        #    System Agent this probe used to rely on.
        base = f"http://127.0.0.1:{AUTH_HTTP_PORT}"
        deadline = time.time() + TIMEOUT
        last = None
        while time.time() < deadline:
            last = http.get(f"{base}/api/agents")
            if last.status_code == 200:
                break
            time.sleep(0.5)
        if last and last.status_code == 200:
            ok("agent list answers over the restored token (HTTP 200)")
        else:
            fail(f"agent list never 200 (last HTTP {last.status_code}) — stale token not recovered")
    finally:
        gw.stop()


# ── PM suite (cross-process: Gateway proxy → standalone acowork-pm) ─────
# Covers the PM member feature end-to-end: the Desktop talks to
# `GET/POST /api/pm/*`, the Gateway reverse-proxies to a SEPARATE
# `acowork-pm` process (ADR-064 Phase 3), which persists to its own
# projects dir on disk. REST paths get `X-Actor: human` injected by the
# proxy (human = project owner / reviewer); the MCP path (`/api/pm/mcp`)
# forwards a trusted `X-MCP-Actor` (agent instance_id, ADR-073) so the
# assigned agent can claim/submit its own tasks.

def pm_wait_ready(http, base, timeout=TIMEOUT):
    """Poll `GET /api/pm/projects` until the standalone PM process answers.

    The Gateway proxy answers 503 until pm_supervisor reports the spawned
    `acowork-pm` process ready (its port is discovered via --port-file).
    Returns the first non-503 status, or None on timeout.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = http.get(f"{base}/api/pm/projects")
            if r.status_code != 503:
                return r.status_code
        except Exception:
            pass
        time.sleep(0.5)
    return None


def pm_agent_instance_id(http, base):
    """Resolve the smoke agent's `instance_id` (ADR-073 identity key).

    The PM member store keys members by instance_id and the AgentDirectory
    validates against `GET /api/agents` — so a real installed instance id
    is required for a cross-process add_member to succeed.
    """
    r = http.get(f"{base}/api/agents")
    if r.status_code != 200:
        return None
    for a in r.json():
        if a.get("agent_id") == AGENT_ID:
            return a.get("instance_id")
    return None


def pm_error_code(body):
    """Extract the machine-readable code from a PM REST error body
    (`{"error": {"code": ..., "message": ...}}`)."""
    try:
        data = body.json()
    except Exception:
        return None
    err = data.get("error")
    if isinstance(err, dict) and isinstance(err.get("code"), str):
        return err["code"]
    return None


def pm_mcp_call(http, base, actor, tool, args, rpc_id=1):
    """Call a PM MCP tool through the Gateway proxy (`/api/pm/mcp`).

    `actor` = agent instance_id sent as `X-MCP-Actor` (the proxy forwards
    it only when the instance is installed, ADR-073). Returns
    (http_status, response_json_dict).
    """
    payload = {
        "jsonrpc": "2.0",
        "id": rpc_id,
        "method": "tools/call",
        "params": {"name": tool, "arguments": args},
    }
    headers = {"content-type": "application/json"}
    if actor:
        headers["x-mcp-actor"] = actor
    r = http.post(f"{base}/api/pm/mcp", json=payload, headers=headers)
    try:
        return r.status_code, r.json()
    except Exception:
        return r.status_code, {}


def pm_mcp_task(body):
    """Unpack a successful MCP tools/call result into the task dict.

    MCP envelopes tool output as `result.content[0].text` = JSON string
    (see acowork-pm mcp/mod.rs). Returns the parsed dict or None.
    """
    try:
        content = body.get("result", {}).get("content", [])
        return json.loads(content[0]["text"])
    except Exception:
        return None


def run_pm_suite(http, base):
    """PM member + linked-assignment e2e against the real Gateway proxy.

    Chain: human creates project → adds member → assigns task → agent
    claims/submits via MCP → human reviews → member removed (no open
    tasks) → state re-read to prove disk persistence.
    """
    print("\n" + "=" * 60)
    print("PM Suite (cross-process: Gateway proxy → standalone acowork-pm)")
    print("=" * 60)

    # ── TC-PM-01: service reachable ──
    print("\n── TC-PM-01: /api/pm/* reachable (standalone PM process) ──")
    status = pm_wait_ready(http, base)
    if status is None:
        fail("PM process never became ready via /api/pm/* (503 persisted)")
        return
    ok(f"PM REST ready (GET /api/pm/projects -> {status})")
    r = http.get(f"{base}/api/pm/projects")
    if r.status_code == 200 and isinstance(r.json(), list):
        ok("project list is a JSON array")
    else:
        fail(f"project list: HTTP {r.status_code} {r.text[:200]}")
        return

    # ── TC-PM-02: project create + member add ──
    print("\n── TC-PM-02: Project create + member add (REST, human) ──")
    member = pm_agent_instance_id(http, base)
    if not member:
        fail("cannot resolve smoke agent instance_id from /api/agents")
        return
    ok(f"smoke agent instance_id: {member[:8]}…")
    pid = None
    r = http.post(f"{base}/api/pm/projects",
                  json={"title": f"smoke-pm-{random_suffix()}",
                        "description": "e2e member flow"})
    if r.status_code == 200:
        pid = r.json().get("id")
        # ADR-076 §决策 11: the creator auto-joins members with kind=user
        # (REST proxy injects X-Actor: human in local mode), so a
        # human-created project starts with exactly that one member.
        members = r.json().get("members") or []
        if [m.get("instance_id") for m in members] == ["human"]:
            ok("human-created project has the creator as sole member")
        else:
            fail(f"members should be ['human'] for human create, got {members}")
    else:
        fail(f"project create: HTTP {r.status_code} {r.text[:200]}")
        return
    if not pid:
        fail(f"no project id in create response: {r.text[:200]}")
        return
    ok(f"project created: {pid}")

    r = http.post(f"{base}/api/pm/projects/{pid}/members",
                  json={"instance_id": member})
    if r.status_code == 200 and any(
        m.get("instance_id") == member for m in r.json().get("members", [])
    ):
        ok(f"member added: {member[:8]}…")
    else:
        fail(f"add member: HTTP {r.status_code} {r.text[:200]}")
        return

    r = http.post(f"{base}/api/pm/projects/{pid}/members",
                  json={"instance_id": member})
    if r.status_code == 409 and pm_error_code(r) == "member_already_exists":
        ok("duplicate member → 409 member_already_exists")
    else:
        fail(f"duplicate member: HTTP {r.status_code} code={pm_error_code(r)}")

    r = http.post(f"{base}/api/pm/projects/{pid}/members",
                  json={"instance_id": str(uuid.uuid4())})
    if r.status_code == 400 and pm_error_code(r) == "bad_request":
        ok("unknown agent instance → 400 bad_request (AgentDirectory)")
    else:
        fail(f"unknown member: HTTP {r.status_code} code={pm_error_code(r)}")

    # ── TC-PM-03: linked assignment ──
    print("\n── TC-PM-03: Linked assignment (assignee ∈ project.members) ──")
    r = http.post(f"{base}/api/pm/projects/{pid}/tasks",
                  json={"title": "assigned task", "assignee": member})
    tid = r.json().get("id") if r.status_code == 200 else None
    if r.status_code == 200 and tid and r.json().get("assignee") == member:
        ok(f"task assigned to member: {tid}")
    else:
        fail(f"assign task: HTTP {r.status_code} {r.text[:200]}")
        return

    r = http.post(f"{base}/api/pm/projects/{pid}/tasks",
                  json={"title": "outsider task",
                        "assignee": str(uuid.uuid4())})
    if r.status_code == 400 and pm_error_code(r) == "assignee_not_project_member":
        ok("non-member assignee → 400 assignee_not_project_member")
    else:
        fail(f"outsider assignee: HTTP {r.status_code} code={pm_error_code(r)}")

    r = http.delete(f"{base}/api/pm/projects/{pid}/members/{member}")
    if r.status_code == 409 and pm_error_code(r) == "member_has_open_tasks":
        ok("remove member with open task → 409 member_has_open_tasks")
    else:
        fail(f"remove busy member: HTTP {r.status_code} code={pm_error_code(r)}")

    # ── TC-PM-04: agent claim/submit via MCP path ──
    print("\n── TC-PM-04: Agent claim/submit (MCP path, X-MCP-Actor) ──")
    status, body = pm_mcp_call(http, base, member, "pm_claim_task",
                               {"task_id": tid})
    task = pm_mcp_task(body) if status == 200 else None
    if task and task.get("status") == "in_progress":
        ok(f"pm_claim_task → in_progress ({tid})")
    else:
        fail(f"pm_claim_task: HTTP {status} {body}")
        return

    status, body = pm_mcp_call(http, base, member, "pm_submit_task",
                               {"task_id": tid, "text": "done via smoke e2e"})
    task = pm_mcp_task(body) if status == 200 else None
    if task and task.get("status") == "submitted":
        ok("pm_submit_task → submitted")
    else:
        fail(f"pm_submit_task: HTTP {status} {body}")
        return

    # ── TC-PM-05: human review → member removal → persistence ──
    print("\n── TC-PM-05: Review + member removal + persistence ──")
    r = http.post(f"{base}/api/pm/tasks/{tid}/review",
                  json={"approved": True})
    if r.status_code == 200 and r.json().get("status") == "done":
        ok("human review → done")
    else:
        fail(f"review: HTTP {r.status_code} {r.text[:200]}")
        return

    r = http.delete(f"{base}/api/pm/projects/{pid}/members/{member}")
    if r.status_code == 200 and not any(
        m.get("instance_id") == member for m in r.json().get("members", [])
    ):
        ok("member removed after task done")
    else:
        fail(f"remove member: HTTP {r.status_code} {r.text[:200]}")
        return

    # 独立 PM 进程磁盘持久化：跨请求重新读，状态必须一致。
    # ADR-076 §决策 11: 只剩创建者（human）成员，agent 成员已移除。
    r = http.get(f"{base}/api/pm/projects/{pid}")
    if r.status_code == 200 and [
        m.get("instance_id") for m in (r.json().get("members") or [])
    ] == ["human"]:
        ok("project persisted with only the creator member (cross-request re-read)")
    else:
        fail(f"persisted project: HTTP {r.status_code} {r.text[:200]}")

    r = http.delete(f"{base}/api/pm/projects/{pid}/members/{member}")
    if r.status_code == 404 and pm_error_code(r) == "member_not_found":
        ok("remove absent member → 404 member_not_found")
    else:
        fail(f"remove absent member: HTTP {r.status_code} code={pm_error_code(r)}")


# ── Main ────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="ADR-033/055 Frontend Smoke Test (full suite)")
    parser.add_argument("--gateway-bin", default=None, help="path to acowork-gateway binary")
    parser.add_argument("--node-bin", default=None, help="path to acowork-node binary")
    parser.add_argument("--home", default=None, help="ACOWORK_HOME for the main instance (default: temp)")
    parser.add_argument("--no-start", action="store_true",
                        help="reuse a running Gateway on default ports (main suite only)")
    parser.add_argument("--skip-auth", action="store_true", help="skip the Phase 5a auth suite")
    parser.add_argument("--skip-pm", action="store_true",
                        help="skip the cross-process PM suite (needs target/debug/acowork-pm)")
    args = parser.parse_args()

    root = REPO_ROOT / "target" / "debug"
    gw_bin = Path(args.gateway_bin) if args.gateway_bin else root / "acowork-gateway"
    node_bin = Path(args.node_bin) if args.node_bin else root / "acowork-node"

    print("=" * 60)
    print("Frontend Smoke Test (full suite)")
    print(f"  Gateway: {gw_bin}")
    print(f"  Node:    {node_bin}")
    print(f"  Agent:   {AGENT_ID}")
    print(f"  SMOKE_LLM={os.environ.get('SMOKE_LLM', '') or '0'} (LLM cases {'ON' if LLM_ENABLED else 'skipped'})")
    print("=" * 60)

    home = Path(args.home) if args.home else Path(tempfile.mkdtemp(prefix="acowork-smoke-main-"))
    http = httpx.Client(timeout=15)
    ctx = {}
    base = f"http://127.0.0.1:{DEFAULT_HTTP_PORT}"

    gw = None
    if not args.no_start:
        gw = Gateway(gw_bin, home, DEFAULT_HTTP_PORT, DEFAULT_MQTT_PORT, auth_enabled=False)
        if not gw.start():
            # Don't leave the half-started gateway behind — it would hold
            # the test ports and break the next run.
            gw.stop()
            sys.exit(1)
    else:
        log("INFO", "Using running Gateway instance (default ports)")
        if not wait_http_ok(base):
            fail("no Gateway on default ports")
            sys.exit(1)

    try:
        # Health + install
        r = http.get(f"{base}/health")
        assert_status(r, 200, "Gateway /health")
        if not ensure_agent_installed(http, base):
            fail("aborting: agent not installed")
            sys.exit(1)

        # ── Read-only (Gateway native) ──
        test_tc_chat_01_agent_list(http, base)
        test_tc_settings_01_status(http, base)
        test_tc_settings_02_config(http, base)
        test_tc_harness_01_providers(http, base)
        test_tc_harness_04_models(http, base)
        test_tc_harness_05_embedding(http, base)
        test_tc_harness_06_mcp(http, base)
        test_tc_harness_07_search_keys(http, base)
        test_tc_mqtt_broker(http, base)
        test_tc_harness_08_provider_key_mqtt_sync(http, base, DEFAULT_MQTT_PORT)

        # ── TC-CHAT-02: start agent, wait for MQTT online ──
        started = test_tc_chat_02_start_agent(http, base, DEFAULT_MQTT_PORT)
        if started:
            # Phase B late-binds workspace/memory services; wait for the
            # runtime to serve before hammering the write cases.
            ready = wait_runtime_ready(http, base)
            # Runtime-backed read-only
            test_tc_setup_01_config(http, base)
            test_tc_setup_05_mcp(http, base)
            test_tc_setup_06_search_providers(http, base)
            test_tc_setup_07_model(http, base)
            test_tc_skill_01_list(http, base)
            r = http.get(f"{base}/api/agents/{AGENT_ID}/skills")
            if r.status_code == 200:
                skills = r.json().get("skills", [])
                if skills:
                    ctx["skill_name"] = skills[0].get("name")
            test_tc_skill_02_detail(http, base, ctx)
            test_tc_skill_03_history(http, base, ctx)
            test_tc_ws_01_list(http, base)
            test_tc_mem_01_nodes(http, base)
            test_tc_mem_02_stats(http, base)
            test_tc_chat_03_session_list(http, base)
            test_tc_chat_04_latest_session(http, base)
            test_tc_lsp_01_endpoint(http, base)

            # Write ops (create → use → delete)
            test_tc_settings_03_06_log_level(http, base)
            test_tc_settings_04_users_list(http, base)
            test_tc_settings_05_07_user(http, base)
            test_tc_harness_02_03_provider(http, base)
            test_tc_setup_02_04_tool_toggle(http, base, ctx)
            test_tc_ws_02_add_workspace(http, base, ctx)
            test_tc_ws_03_tree(http, base, ctx)
            test_tc_ws_04_07_file_crud(http, base, ctx)
            test_tc_ws_08_find(http, base, ctx)
            test_tc_fsb_01_default_hides_dotfiles(http, base, ctx)
            test_tc_fsb_02_show_hidden_includes_dotfiles(http, base, ctx)
            test_tc_ws_09_delete_workspace(http, base, ctx)
            test_tc_mem_03_consolidate(http, base)
            test_tc_mem_04_create_delete_node(http, base)

            # Session lifecycle + chat via MQTT
            mq = MqttClient()
            if mq.connect(port=DEFAULT_MQTT_PORT):
                sid = test_tc_chat_06_create_session(mq, base)
                if sid:
                    test_tc_chat_05_messages(http, base, sid)
                    test_tc_chat_07_send_message(mq, sid)
                    test_tc_chat_08_tool_call(mq, sid)
                    test_tc_model_01_switch(http, base, mq, sid)
                    test_tc_model_02_reasoning_effort(http, base, mq, sid)
                    test_tc_flow_01_stop(mq, sid)
                    test_tc_doc_01_02_upload_read(http, base, sid)
                    test_tc_doc_03_reference(mq, sid)
                    test_tc_chat_09_rename_session(http, base, mq, sid)
                    test_tc_chat_10_delete_session(mq, base, http, sid)
                mq.disconnect()
            else:
                fail("MQTT broker refused — session cases skipped")

            test_tc_chat_11_stop_agent(http, base, DEFAULT_MQTT_PORT)

        # ── PM suite: cross-process (Gateway proxy → standalone acowork-pm) ──
        if not args.skip_pm:
            run_pm_suite(http, base)

        # ── Phase 5a auth suite (isolated instance) ──
        if not args.skip_auth and not args.no_start:
            run_auth_suite(gw_bin, node_bin, http)

        # ── Stale-token recovery suite (isolated instance) ──
        if not args.no_start:
            run_recovery_suite(gw_bin, node_bin, http)
    finally:
        if gw:
            gw.stop()
        http.close()

    # ── Summary ──
    total = passed + failed + skipped
    print(f"\n{'=' * 60}")
    print(f"Results: {passed} passed, {failed} failed, {skipped} skipped ({total} total)")
    print(f"Main home kept at: {home} (inspect for debugging)")
    if failed > 0:
        print("SOME TESTS FAILED")
        sys.exit(1)
    print("ALL TESTS PASSED")
    sys.exit(0)


if __name__ == "__main__":
    main()
