#!/usr/bin/env python3
"""Onboarding Flow E2E — ADR-059 bootstrap handshake regression test.

Reproduces the exact first-run sequence the Tauri Desktop fires during
onboarding, but drives it against the ADR-059 readiness contract:

  1. Gateway boots; `GET /api/bootstrap` is the readiness source of
     truth (phase BOOTING → READY, ADR-059 §5.1). `/health` is
     liveness-only and is NOT a readiness claim.
  2. The retained MQTT topic `acowork/global/bootstrap` carries the
     same aggregated BootstrapState (DataEnvelope field 16) — HTTP and
     MQTT consumers always agree on instance_id / version / phase.
  3. `POST /api/agents/install` answers 409 `dependency_not_ready`
     while `node.local` has not announced readiness, and 202 +
     OperationAck{operation_id, ...} once it has. The install's
     completion is correlated via the node's NodeEvent reply
     (request_id == operation_id, status "ok") on
     `acowork/nodes/local/agents/{agent_id}/events` (ADR-059 §6.2).
  4. instance_id is minted fresh on every Gateway process start (never
     persisted, ADR-059 §5.1) — a warm restart must rotate it, and the
     client (DesktopView) must drop its previous baseline + any
     in-flight operation when it detects the new instance (§8.3).
  5. A reconnect with a fresh MQTT client must re-deliver the retained
     snapshot with the same instance_id and a non-regressing version.

Scenarios (--scenario, default all):
  * cold      — clean HOME: 409 probe → READY → HTTP/MQTT consistency
                → §8.3 subscriber rules → operation_id install round
                trip for every package → full inventory
  * warm      — same HOME restart: install one package, stop, restart,
                assert instance_id rotated, stale snapshots rejected,
                inventory recovered
  * reconnect — same instance, fresh MQTT client: retained re-delivery
                keeps instance_id and does not regress version

Requires (installed once):
    pip install paho-mqtt httpx

Usage:
    python3 onboarding_installs_all_agents.py
    python3 onboarding_installs_all_agents.py --scenario cold
    python3 onboarding_installs_all_agents.py --gateway-bin ./target/release/acowork-gateway
    python3 onboarding_installs_all_agents.py --keep-home   # do not wipe $ACOWORK_HOME

Exit code 0 = all passed, 1 = any failure.
"""

import argparse
import codecs
import json
import os
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import httpx
import paho.mqtt.client as mqtt


# ── Config ──────────────────────────────────────────────────────────────

# Reserve 19820-19829 for this script — sits below the smoke_test.py
# DEFAULT_HTTP_PORT (19876) and well below the auth suite (19786) so
# simultaneous runs don't collide.
DEFAULT_HTTP_PORT = 19822
DEFAULT_MQTT_PORT = 19821

TIMEOUT = 30          # per-condition wait budget

REPO_ROOT = Path(__file__).resolve().parent.parent.parent
DEFAULT_GATEWAY_BIN = REPO_ROOT / "target" / "debug" / "acowork-gateway.exe"

# Default 3 user-chosen packages — same set the README's onboarding
# screenshot picks. ADR-077 removed the auto-installed System Agent, so the
# inventory is exactly what the user picked.
DEFAULT_PACKAGES = [
    "com.acowork.senior-engineer",
    "com.acowork.product-manager",
    "com.acowork.software-architect",
]

# ── Wire constants (mqtt_payload.proto) ────────────────────────────────

# DataEnvelope {version=1, payload=2}; the payload is a oneof carrying
# the variant message under these field numbers.
BS_FIELD = 16            # BootstrapState
NE_FIELD = 83            # NodeEvent
INSTALLED_FIELD = 84     # InstalledAgentInfo
NODE_READY_FIELD = 87    # NodeReady

# BootstrapState inner fields.
BS_FIELDS = {
    1: ("varint", "protocol_version"),
    2: ("str", "instance_id"),
    3: ("varint", "version"),
    4: ("varint", "phase"),
    5: ("str", "phase_detail"),
    6: ("varint", "issued_at_ms"),
}

# NodeEvent inner fields.
NE_FIELDS = {
    1: ("str", "node_id"),
    2: ("str", "request_id"),
    3: ("str", "status"),
    4: ("str", "message"),
    5: ("str", "result_json"),
}

# BootstrapPhase proto enum values.
PHASE_UNSPECIFIED = 0
PHASE_BOOTING = 1
PHASE_READY = 2
PHASE_DEGRADED = 3
PHASE_FAILED = 4
PHASE_SHUTTING_DOWN = 5

# HTTP /api/bootstrap serialises phase as the SCREAMING_SNAKE_CASE proto
# name; MQTT carries the raw enum int. Map one to the other.
PHASE_BY_NAME = {
    "UNSPECIFIED": PHASE_UNSPECIFIED,
    "BOOTING": PHASE_BOOTING,
    "READY": PHASE_READY,
    "DEGRADED": PHASE_DEGRADED,
    "FAILED": PHASE_FAILED,
    "SHUTTING_DOWN": PHASE_SHUTTING_DOWN,
}

BOOTSTRAP_TOPIC = "acowork/global/bootstrap"
NODE_EVENTS_FILTER = "acowork/nodes/local/agents/+/events"

# ── Logging ────────────────────────────────────────────────────────────

passed = 0
failed = 0
skipped = 0


def log(level, msg):
    print(f"  [{level}] {msg}")


def ok(msg=""):
    global passed
    passed += 1
    # ASCII markers — Windows console default encoding (cp936/gbk)
    # can't render the original emoji under `python` (vs `py -3`).
    print(f"[ OK ] {msg}")


def fail(msg):
    global failed
    failed += 1
    print(f"[FAIL] {msg}")


def skip(msg):
    global skipped
    skipped += 1
    print(f"[SKIP] {msg}")


# ── Minimal protobuf wire decoder ──────────────────────────────────────
#
# The MQTT payloads are DataEnvelope protobuf messages. Decoding them
# with a full protobuf runtime would add a dependency; the wire format
# used here is tiny (varints + length-delimited strings), so a hand
# rolled reader is enough — and it keeps the E2E self-contained.


def _read_varint(data, pos):
    result = 0
    shift = 0
    while True:
        if pos >= len(data):
            raise ValueError("truncated varint")
        b = data[pos]
        pos += 1
        result |= (b & 0x7F) << shift
        if not (b & 0x80):
            return result, pos
        shift += 7
        if shift > 63:
            raise ValueError("varint overflow")


def _decode_message(data, fields):
    """Decode a protobuf message. `fields` maps field numbers to
    (wire_kind, key) with wire_kind 'varint', 'str' (length-delimited
    UTF-8) or 'bytes' (raw length-delimited, for nested messages).
    Unknown fields are skipped."""
    out = {}
    pos = 0
    while pos < len(data):
        tag, pos = _read_varint(data, pos)
        field_no = tag >> 3
        wire = tag & 0x07
        spec = fields.get(field_no)
        if spec is None:
            if wire == 0:
                _, pos = _read_varint(data, pos)
            elif wire == 2:
                ln, pos = _read_varint(data, pos)
                pos += ln
            else:
                raise ValueError(f"unsupported wire type {wire} for field {field_no}")
            continue
        kind, key = spec
        if kind == "varint":
            out[key], pos = _read_varint(data, pos)
        else:
            ln, pos = _read_varint(data, pos)
            raw = data[pos:pos + ln]
            pos += ln
            out[key] = raw if kind == "bytes" else raw.decode("utf-8", errors="replace")
    return out


def _extract_oneof(payload_bytes, target_field):
    """Pull the `target_field` message out of a DataEnvelope.

    The oneof variants (BootstrapState=16, NodeEvent=83,
    InstalledAgentInfo=84, NodeReady=87, per mqtt_payload.proto) are
    INLINED at the DataEnvelope top level — there is no nested field-2
    "payload" wrapper. Returns the inner message bytes, or None when
    the envelope carries a different oneof variant (or is undecodable).
    """
    try:
        envelope = _decode_message(
            payload_bytes, {1: ("varint", "version"), target_field: ("bytes", "inner")}
        )
    except ValueError:
        return None
    return envelope.get("inner")


def decode_bootstrap(payload):
    """Decode a BootstrapState from a DataEnvelope payload; None if the
    payload is not a BootstrapState message."""
    inner = _extract_oneof(payload, BS_FIELD)
    if inner is None:
        return None
    try:
        return _decode_message(inner, BS_FIELDS)
    except ValueError:
        return None


def decode_node_event(payload):
    """Decode a NodeEvent from a DataEnvelope payload; None if the
    payload is not a NodeEvent message."""
    inner = _extract_oneof(payload, NE_FIELD)
    if inner is None:
        return None
    try:
        return _decode_message(inner, NE_FIELDS)
    except ValueError:
        return None


def bootstrap_pred(**expect):
    """Build a wait_for predicate over raw payloads: decoded BootstrapState
    must match every expected field (e.g. phase=PHASE_READY)."""
    def pred(payload):
        snap = decode_bootstrap(payload)
        if snap is None:
            return False
        for k, v in expect.items():
            if snap.get(k) != v:
                return False
        return True
    return pred


# ── Desktop-side bootstrap view (ADR-059 §8.3 subscriber rules) ────────


class DesktopView:
    """Client-side projection of the bootstrap stream.

    Implements the ADR-059 §8.3 subscription rules:
      * same instance_id → accept strictly-increasing version only
      * cross-instance snapshot with version == 1 (a fresh process's
        first snapshot) → adopt as the new baseline (restart detected)
      * any other cross-instance snapshot → reject (foreign / stale)

    The HTTP /api/bootstrap projection is the authoritative reset
    baseline; MQTT retained snapshots are applied on top of it.
    """

    def __init__(self):
        self.instance_id = None
        self.version = 0
        self.phase = PHASE_UNSPECIFIED
        self.phase_detail = ""
        self.issued_at_ms = 0

    @staticmethod
    def _phase_int(snap):
        p = snap.get("phase")
        if isinstance(p, int):
            return p
        return PHASE_BY_NAME.get(p, PHASE_UNSPECIFIED)

    def reset(self, snap):
        """Adopt `snap` (HTTP dict or decoded MQTT snapshot) as the
        authoritative baseline."""
        self.instance_id = snap["instance_id"]
        self.version = snap["version"]
        self.phase = self._phase_int(snap)
        self.phase_detail = snap.get("phase_detail", "")
        self.issued_at_ms = snap.get("issued_at_ms", 0)

    def apply(self, snap):
        """Apply a decoded MQTT BootstrapState. Returns (accepted, reason)."""
        if self.instance_id is None:
            self.reset(snap)
            return True, "first snapshot adopted as baseline"
        if snap["instance_id"] == self.instance_id:
            if snap["version"] > self.version:
                self.reset(snap)
                return True, f"same instance version {snap['version']} > {self.version}"
            return False, (
                f"same instance version {snap['version']} <= {self.version} "
                "(no regression)"
            )
        if snap["version"] == 1:
            self.reset(snap)
            return True, f"cross-instance switch to {snap['instance_id']} (fresh process)"
        return False, (
            f"cross-instance {snap['instance_id']} version {snap['version']} "
            "(foreign, reject)"
        )


# ── MQTT Client (Desktop Simulator) ────────────────────────────────────


class MqttClient:
    """Desktop-simulator MQTT client (paho VERSION2, same shape as
    smoke_test.py). Tracks received payloads per topic; subscribe()
    blocks until SUBACK so installs can never race the subscription."""

    def __init__(self, client_id=None):
        self.client_id = client_id or f"user:onboarding-e2e:{os.getpid()}:{time.time_ns()}"
        self.client = mqtt.Client(mqtt.CallbackAPIVersion.VERSION2, client_id=self.client_id)
        self.received = {}  # topic -> list of payloads
        self.connack_rc = None
        self.connected = False
        self._suback = threading.Event()
        self.client.on_connect = self._on_connect
        self.client.on_message = self._on_message
        self.client.on_subscribe = self._on_subscribe

    def _on_connect(self, client, userdata, flags, reason_code, properties):
        self.connack_rc = reason_code.value if hasattr(reason_code, "value") else reason_code
        self.connected = reason_code == 0 or (
            hasattr(reason_code, "is_failure") and not reason_code.is_failure
        )

    def _on_message(self, client, userdata, msg):
        self.received.setdefault(msg.topic, []).append(msg.payload)

    def _on_subscribe(self, client, userdata, mid, reason_codes, properties):
        self._suback.set()

    def connect(self, host="127.0.0.1", port=DEFAULT_MQTT_PORT, timeout=8):
        self.client.connect(host, port, 60)
        self.client.loop_start()
        deadline = time.time() + timeout
        while time.time() < deadline and self.connack_rc is None:
            time.sleep(0.05)
        if not self.connected:
            log("INFO", f"MQTT connect failed: rc={self.connack_rc}")
        return self.connected

    def disconnect(self):
        try:
            self.client.loop_stop()
            self.client.disconnect()
        except Exception:
            pass

    def subscribe(self, topic, qos=1):
        self._suback.clear()
        self.client.subscribe(topic, qos)
        if not self._suback.wait(timeout=8):
            log("INFO", f"subscribe {topic}: SUBACK timeout")
        self.received.setdefault(topic, [])

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


# ── Gateway lifecycle ──────────────────────────────────────────────────


class Gateway:
    """Spawns an isolated acowork-gateway under a fresh ACOWORK_HOME."""

    def __init__(self, bin_path, home, http_port, mqtt_port):
        self.bin = Path(bin_path)
        self.home = Path(home)
        self.http_port = http_port
        self.mqtt_port = mqtt_port
        self.base = f"http://127.0.0.1:{http_port}"
        self.proc = None
        # Isolated node ports so we don't collide with a long-running
        # production install on the same machine.
        self.proxy_port = 19829
        self.lsp_relay_port = 19828
        self._write_config()

    def _write_config(self):
        (self.home / "config").mkdir(parents=True, exist_ok=True)
        toml = (
            f"vault_dir = '{self.home}/config/vault'\n"
            f"packages_dir = '{self.home}/config/packages'\n"
            f"data_dir = '{self.home}/data'\n"
            'log_level = "info"\n'
            "dev_mode = true\n"
            'advertise_host = "127.0.0.1"\n'
            f"node_proxy_port = {self.proxy_port}\n"
            f"node_lsp_relay_port = {self.lsp_relay_port}\n"
            f"[http]\nport = {self.http_port}\n"
            f"[mqtt]\nenabled = true\nhost = \"127.0.0.1\""
            f"\nport = {self.mqtt_port}\n"
            "auth_enabled = false\n"
        )
        (self.home / "config" / "gateway.toml").write_text(toml)

    def start(self):
        log("INFO", f"Starting Gateway (http :{self.http_port}, mqtt :{self.mqtt_port})")
        # Isolate the spawned local node's home so a stale identity.json
        # from another install does not propagate. See smoke_test.py's
        # Gateway class for the same rationale.
        node_home = self.home / "node"
        env = {
            **os.environ,
            "ACOWORK_HOME": str(self.home),
            "ACOWORK_NODE_HOME": str(node_home),
        }
        self.proc = subprocess.Popen(
            [str(self.bin), "--daemon", "--log-level", "info", "--home", str(self.home)],
            env=env,
            # NOTE: do NOT pass `text=True` — the Gateway's tracing
            # subscriber emits ANSI colour escapes (e.g. `ESC[31m`) and
            # the Windows console default codec (cp936/gbk) raises
            # UnicodeDecodeError on those bytes. With `text=True`,
            # the drain thread crashes on the first ANSI byte, the
            # stdout pipe buffer fills, and the Gateway blocks on
            # `print` at T+~1s — killing the readiness probe window.
            # Writing raw bytes (errors='replace' in the decoder
            # below) keeps the drain thread alive.
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
        )
        # Spawn a thread to drain the gateway log so the pipe never
        # blocks (Windows Popen buffers can deadlock otherwise).
        # Failures here would just lose log lines, which is fine for
        # an e2e.
        def _drain():
            try:
                decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
                while True:
                    chunk = self.proc.stdout.read(4096)
                    if not chunk:
                        # EOF — gateway exited.
                        tail = decoder.decode(b"", final=True)
                        if tail:
                            for ln in tail.splitlines():
                                log("GATEWAY", ln.rstrip())
                        return
                    text = decoder.decode(chunk)
                    for ln in text.splitlines():
                        log("GATEWAY", ln.rstrip())
            except Exception:
                pass
        threading.Thread(target=_drain, daemon=True).start()
        with httpx.Client(timeout=3) as client:
            for _ in range(10):
                try:
                    r = client.get(f"{self.base}/health")
                    if r.status_code == 200:
                        ok(f"Gateway ready on :{self.http_port}")
                        return True
                except Exception:
                    pass
                time.sleep(1)
        fail("Gateway did not become ready within 10s")
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
        # Reap the local node + embed model runner + user service the
        # Gateway spawned for THIS instance. On Linux / macOS the smoke
        # harness uses `pgrep`; on Windows that command does not exist, so
        # we use psutil to enumerate processes by image name and filter on
        # command line. SIGTERM is also unrecognised on Windows, so the
        # terminate call falls back to SIGKILL when SIGTERM fails.
        home_str = str(self.home)
        for pat in ("acowork-node", "acowork-embed", "acowork-user"):
            for pid in _find_child_pids(pat, home_str):
                try:
                    os.kill(pid, signal.SIGTERM)
                    log("INFO", f"reaped orphaned {pat} (pid {pid})")
                except (ProcessLookupError, OSError):
                    pass


def _find_child_pids(pattern, home_str):
    """Return PIDs of acowork-node / acowork-embed / acowork-user children
    whose command line contains `home_str` (the per-instance ACOWORK_HOME
    or ACOWORK_NODE_HOME). Uses psutil so the script works on both
    Linux / macOS (where the smoke harness is CI-hosted) and Windows
    (where this e2e is normally debugged)."""
    try:
        import psutil
    except ImportError:
        return []
    pids = []
    for proc in psutil.process_iter(["name", "cmdline"]):
        try:
            name = proc.info.get("name") or ""
            cmd = " ".join(proc.info.get("cmdline") or [])
        except (psutil.NoSuchProcess, psutil.AccessDenied):
            continue
        if name.lower() != f"{pattern}.exe" and name.lower() != pattern:
            continue
        if home_str in cmd:
            pids.append(proc.pid)
    return pids


# ── Readiness / install helpers ────────────────────────────────────────


def wait_bootstrap_ready(http, base, timeout=TIMEOUT):
    """Readiness source of truth (ADR-059 §5.1): poll `GET /api/bootstrap`
    until phase == READY. Returns the snapshot dict or None."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            r = http.get(f"{base}/api/bootstrap")
            if r.status_code == 200:
                snap = r.json()
                if snap.get("phase") == "READY":
                    return snap
        except Exception:
            pass
        time.sleep(0.3)
    return None


def install_package(http, base, package_path):
    """POST /api/agents/install once; returns (status_code, json_body)."""
    with open(package_path, "rb") as f:
        r = http.post(
            f"{base}/api/agents/install",
            files={"package": (package_path.name, f, "application/octet-stream")},
        )
    try:
        body = r.json()
    except Exception:
        body = {"raw": r.text[:300]}
    return r.status_code, body


def wait_operation_completed(mq, agent_id, op_id, timeout=TIMEOUT):
    """Wait for the NodeEvent whose request_id == op_id on the agent's
    event topic (ADR-059 §6.2 correlation). Returns the decoded event
    dict or None."""
    topic = f"acowork/nodes/local/agents/{agent_id}/events"
    deadline = time.time() + timeout
    while time.time() < deadline:
        for payload in mq.received.get(topic, []):
            ev = decode_node_event(payload)
            if ev and ev.get("request_id") == op_id:
                return ev
        time.sleep(0.1)
    return None


def install_and_await_ack(http, base, mq, package_path, timeout=TIMEOUT):
    """Full ADR-059 §6 install round trip:
      1. POST /api/agents/install → 202 OperationAck {operation_id, ...}
      2. wait NodeEvent(request_id == operation_id, status == 'ok')
    Returns True on success (emits ok/fail lines)."""
    status, body = install_package(http, base, package_path)
    if status == 409:
        fail(
            f"{package_path.name}: 409 dependency_not_ready "
            f"(phase={body.get('current_phase')}, detail={body.get('phase_detail')}) "
            "— install must be retried after READY"
        )
        return False
    if status != 202:
        fail(f"{package_path.name}: install HTTP {status}: {body}")
        return False
    op_id = body.get("operation_id")
    state = body.get("state")
    if not op_id:
        fail(f"{package_path.name}: 202 without operation_id: {body}")
        return False
    agent_id = package_path.stem
    ev = wait_operation_completed(mq, agent_id, op_id, timeout)
    if ev is None:
        fail(
            f"{package_path.name}: op {op_id} never completed on "
            f"acowork/nodes/local/agents/{agent_id}/events (ack state={state})"
        )
        return False
    if ev.get("status") != "ok":
        fail(
            f"{package_path.name}: op {op_id} failed: status={ev.get('status')} "
            f"message={ev.get('message')}"
        )
        return False
    ok(
        f"{package_path.name}: 202 ack op={op_id[:8]}… state={state} "
        f"→ NodeEvent status=ok (request_id matched)"
    )
    return True


def await_inventory(http, base, package_ids, timeout=TIMEOUT):
    """Poll GET /api/agents until every package is visible (async
    install completion is observed via retained inventory, ADR-055 §3.2)."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        r = http.get(f"{base}/api/agents")
        if r.status_code == 200:
            installed = {a.get("agent_id") for a in r.json()}
            if all(p in installed for p in package_ids):
                return True, installed
        time.sleep(0.5)
    return False, set()


# ── Test cases ─────────────────────────────────────────────────────────


def test_cold_start(http, base, mq, packages):
    """Cold start on a clean HOME — the exact first-run onboarding
    sequence. Probes dependency_not_ready before READY, then asserts
    HTTP/MQTT bootstrap consistency, the §8.3 subscriber rules, and the
    operation_id install round trip for every package."""
    print("\n── TC-01: cold start — bootstrap handshake ──")

    # 1. Fire an install the moment the Gateway answers (before waiting
    #    for READY). Two outcomes are both acceptable (ADR-059 §6.3):
    #    * 409 dependency_not_ready — node.local not ready yet; the
    #      client must retry after phase = READY
    #    * 202 OperationAck — bootstrap already READY when the probe
    #      landed (no 409 window on this machine)
    probe_pkg = REPO_ROOT / "agents" / "agent-packages" / f"{packages[0]}.agent"
    probe_installed = None
    status, body = install_package(http, base, probe_pkg)
    if status == 409:
        # The structured error body (ADR-059 §6.3) carries the
        # machine-readable code under `structured.code`.
        if body.get("structured", {}).get("code") != "dependency_not_ready":
            fail(f"early install: expected code dependency_not_ready, got {body}")
            return False
        ok(
            f"early install (pre-READY) answered dependency_not_ready "
            f"(phase={body.get('structured', {}).get('current_phase')}, "
            f"detail={body.get('structured', {}).get('phase_detail')})"
        )
    elif status == 202:
        probe_installed = packages[0]
        ok(
            f"early install landed 202 (op={body.get('operation_id')}) — "
            "node.local was already ready, no 409 window this run"
        )
    else:
        fail(f"early install: expected 409 or 202, got HTTP {status}: {body}")
        return False

    # 2. Readiness via HTTP — the authoritative projection.
    snap = wait_bootstrap_ready(http, base)
    if snap is None:
        fail("bootstrap never reached READY via GET /api/bootstrap")
        return False
    if not snap.get("instance_id") or snap.get("version", 0) < 1:
        fail(f"bootstrap snapshot malformed: {snap}")
        return False
    ok(
        f"bootstrap READY via HTTP: instance={snap['instance_id'][:8]}… "
        f"version={snap['version']} detail={snap['phase_detail']}"
    )

    # 3. HTTP/MQTT consistency: the retained snapshot must carry the
    #    same instance_id and, once READY has stabilised, the same
    #    version (ADR-059 §5.4).
    retained = mq.wait_for(
        BOOTSTRAP_TOPIC, timeout=TIMEOUT,
        predicate=bootstrap_pred(phase=PHASE_READY, instance_id=snap["instance_id"]),
    )
    if retained is None:
        fail("no READY BootstrapState on retained topic acowork/global/bootstrap")
        return False
    mq_snap = decode_bootstrap(retained)
    if mq_snap["version"] != snap["version"]:
        fail(
            f"HTTP/MQTT version mismatch: HTTP={snap['version']} "
            f"MQTT={mq_snap['version']}"
        )
        return False
    ok(
        f"HTTP/MQTT bootstrap consistent: instance={mq_snap['instance_id'][:8]}… "
        f"version={mq_snap['version']} phase=READY"
    )

    # 4. §8.3 subscriber rules on the Desktop-side view.
    view = DesktopView()
    view.reset(snap)
    ok("DesktopView baseline reset from HTTP snapshot")
    checks = [
        # (snapshot, must_accept, what)
        ({"instance_id": snap["instance_id"], "version": snap["version"] - 1,
          "phase": PHASE_BOOTING}, False, "same-instance stale version rejected"),
        ({"instance_id": snap["instance_id"], "version": snap["version"],
          "phase": PHASE_READY}, False, "same-instance equal version does not regress"),
        ({"instance_id": snap["instance_id"], "version": snap["version"] + 1,
          "phase": PHASE_READY}, True, "same-instance newer version accepted"),
        ({"instance_id": "foreign-instance", "version": 9,
          "phase": PHASE_READY}, False, "cross-instance foreign snapshot rejected"),
        ({"instance_id": "fresh-instance", "version": 1,
          "phase": PHASE_BOOTING}, True, "cross-instance version==1 switches baseline"),
    ]
    for snap_in, must_accept, what in checks:
        acc, why = view.apply(snap_in)
        if acc != must_accept:
            fail(f"§8.3: {what} (got accept={acc}, {why})")
            return False
        ok(f"§8.3: {what} ({why})")
    view.reset(snap)
    ok("DesktopView baseline restored to HTTP snapshot")

    # 5. Operation-id install round trip for every package.
    for package_id in packages:
        if package_id == probe_installed:
            ok(f"{package_id}: covered by the early-install probe (202)")
            continue
        pkg_path = REPO_ROOT / "agents" / "agent-packages" / f"{package_id}.agent"
        if not pkg_path.exists():
            fail(f"package missing on disk: {pkg_path}")
            return False
        if not install_and_await_ack(http, base, mq, pkg_path):
            return False

    # 6. Inventory holds exactly the packages that were installed.
    got, installed = await_inventory(http, base, packages)
    if not got:
        fail(f"inventory never showed all packages: {packages}")
        return False
    ok(f"inventory holds every installed package: {len(installed)} entries")
    return True


def test_reconnect(http, base, mq_port):
    """Reconnect with a fresh MQTT client against the SAME Gateway
    instance: the retained bootstrap snapshot must re-deliver with the
    same instance_id and a non-regressing version (ADR-059 §5.3), and
    the Desktop-side baseline must not switch."""
    print("\n── TC-03: MQTT reconnect — retained re-delivery ──")

    mq1 = MqttClient()
    if not mq1.connect(port=mq_port):
        fail("mq1 connect failed")
        return False
    mq1.subscribe(BOOTSTRAP_TOPIC)
    s1 = mq1.wait_for(BOOTSTRAP_TOPIC, timeout=TIMEOUT,
                      predicate=lambda p: decode_bootstrap(p) is not None)
    if s1 is None:
        fail("mq1: no bootstrap snapshot received")
        mq1.disconnect()
        return False
    snap1 = decode_bootstrap(s1)
    ok(
        f"mq1 snapshot: instance={snap1['instance_id'][:8]}… "
        f"version={snap1['version']} phase={snap1['phase']}"
    )
    mq1.disconnect()

    mq2 = MqttClient()
    if not mq2.connect(port=mq_port):
        fail("mq2 connect failed")
        return False
    mq2.subscribe(BOOTSTRAP_TOPIC)
    s2 = mq2.wait_for(BOOTSTRAP_TOPIC, timeout=TIMEOUT,
                      predicate=lambda p: decode_bootstrap(p) is not None)
    if s2 is None:
        fail("mq2: no retained snapshot after reconnect")
        mq2.disconnect()
        return False
    snap2 = decode_bootstrap(s2)
    if snap2["instance_id"] != snap1["instance_id"]:
        fail(
            f"reconnect changed instance: {snap1['instance_id'][:8]}… → "
            f"{snap2['instance_id'][:8]}…"
        )
        mq2.disconnect()
        return False
    if snap2["version"] < snap1["version"]:
        fail(f"version regressed across reconnect: {snap1['version']} → {snap2['version']}")
        mq2.disconnect()
        return False
    ok(
        f"mq2 retained re-delivery: same instance, "
        f"version {snap2['version']} >= {snap1['version']}"
    )

    # The Desktop-side view must not switch baseline across a reconnect.
    view = DesktopView()
    view.reset(snap1)
    acc, why = view.apply(snap2)
    if view.instance_id != snap1["instance_id"]:
        fail("reconnect switched the DesktopView baseline — must stay on same instance")
        mq2.disconnect()
        return False
    ok(f"DesktopView baseline stable across reconnect ({why})")
    mq2.disconnect()
    return True


def test_warm_restart(gw_bin, home, http_port, mq_port, packages):
    """Warm restart on the SAME HOME: the Gateway process is stopped and
    started again with the same data dirs. The new process must mint a
    FRESH instance_id (ADR-059 §5.1 — never persisted), the DesktopView
    must drop the previous session's baseline + in-flight operations
    (§8.3), and the on-disk package registry must re-surface."""
    print("\n── TC-02: warm restart — instance_id rotation + cache drop ──")

    gw = Gateway(gw_bin, home, http_port, mq_port)
    if not gw.start():
        gw.stop()
        return False
    http = httpx.Client(timeout=15)
    base = f"http://127.0.0.1:{http_port}"
    mq = MqttClient()
    if not mq.connect(port=mq_port):
        fail("mq connect failed (first boot)")
        http.close()
        gw.stop()
        return False
    mq.subscribe(BOOTSTRAP_TOPIC)
    mq.subscribe(NODE_EVENTS_FILTER)

    snap_a = wait_bootstrap_ready(http, base)
    if snap_a is None:
        fail("first boot never reached READY")
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    instance_a = snap_a["instance_id"]

    # Install one package with the operation-id round trip.
    pkg = REPO_ROOT / "agents" / "agent-packages" / f"{packages[0]}.agent"
    if not install_and_await_ack(http, base, mq, pkg):
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    got, installed = await_inventory(http, base, [packages[0]])
    if not got:
        fail(f"inventory missing {packages[0]} before restart: {installed}")
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    ok(f"pre-restart: {packages[0]} installed under instance {instance_a[:8]}…")

    # Stop, then restart on the SAME home.
    mq.disconnect()
    http.close()
    gw.stop()
    if not gw.start():
        gw.stop()
        return False
    http = httpx.Client(timeout=15)
    mq = MqttClient()
    if not mq.connect(port=mq_port):
        fail("mq connect failed (second boot)")
        http.close()
        gw.stop()
        return False
    mq.subscribe(BOOTSTRAP_TOPIC)

    snap_b = wait_bootstrap_ready(http, base)
    if snap_b is None:
        fail("second boot never reached READY")
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    instance_b = snap_b["instance_id"]
    if instance_b == instance_a:
        fail("instance_id persisted across restart — must rotate per process (ADR-059 §5.1)")
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    ok(f"instance_id rotated across restart: {instance_a[:8]}… → {instance_b[:8]}…")

    # The previous session's cache must be dropped: snapshots of the old
    # instance are foreign once the baseline is B (ADR-059 §8.3).
    view = DesktopView()
    view.reset(snap_b)
    ok("DesktopView baseline reset to the new instance")
    acc, why = view.apply({"instance_id": instance_a, "version": 99,
                           "phase": PHASE_READY, "phase_detail": "", "issued_at_ms": 0})
    if acc:
        fail("stale snapshot from the previous instance accepted — cache not dropped")
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    ok(f"previous-instance snapshot rejected ({why})")
    acc, why = view.apply({"instance_id": instance_b,
                           "version": snap_b["version"] + 1,
                           "phase": PHASE_READY, "phase_detail": "", "issued_at_ms": 0})
    if not acc:
        fail(f"current-instance update rejected after restart ({why})")
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    ok(f"current-instance update accepted ({why})")

    # The retained topic must now carry the new instance.
    retained = mq.wait_for(
        BOOTSTRAP_TOPIC, timeout=TIMEOUT,
        predicate=bootstrap_pred(instance_id=instance_b),
    )
    if retained is None:
        fail("no bootstrap snapshot for the new instance on MQTT")
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    mq_snap = decode_bootstrap(retained)
    ok(
        f"MQTT snapshot for new instance: version={mq_snap['version']} "
        f"phase={mq_snap['phase']}"
    )

    # Inventory recovery: the on-disk package registry must re-surface.
    got, installed = await_inventory(http, base, [packages[0]])
    if not got:
        fail(f"inventory did not recover after restart: {installed}")
        mq.disconnect()
        http.close()
        gw.stop()
        return False
    ok(f"inventory recovered after restart: {packages[0]} present")

    mq.disconnect()
    http.close()
    gw.stop()
    return True


# ── Scenario runner / main ─────────────────────────────────────────────


def run_scenario(name, args):
    home = Path(tempfile.mkdtemp(prefix=f"acowork-onboarding-{name}-"))
    print(f"\n{'=' * 60}\nScenario: {name}  (ACOWORK_HOME: {home})\n{'=' * 60}")
    gw = Gateway(args.gw_bin, home, DEFAULT_HTTP_PORT, DEFAULT_MQTT_PORT)
    http = httpx.Client(timeout=15)
    mq = MqttClient()
    try:
        if name == "cold":
            if not gw.start():
                fail("Gateway did not start (cold)")
                return
            if not mq.connect(port=DEFAULT_MQTT_PORT):
                fail("MQTT connect failed (cold)")
                return
            mq.subscribe(BOOTSTRAP_TOPIC)
            mq.subscribe(NODE_EVENTS_FILTER)
            test_cold_start(http, f"http://127.0.0.1:{DEFAULT_HTTP_PORT}", mq, args.packages)
        elif name == "reconnect":
            if not gw.start():
                fail("Gateway did not start (reconnect)")
                return
            test_reconnect(http, f"http://127.0.0.1:{DEFAULT_HTTP_PORT}", DEFAULT_MQTT_PORT)
        elif name == "warm":
            # The warm scenario manages its own Gateway lifecycle (it
            # must stop and restart on the same home).
            test_warm_restart(
                args.gw_bin, home, DEFAULT_HTTP_PORT, DEFAULT_MQTT_PORT, args.packages
            )
        else:
            fail(f"unknown scenario: {name}")
    finally:
        mq.disconnect()
        http.close()
        gw.stop()
        if not args.keep_home:
            import shutil
            shutil.rmtree(home, ignore_errors=True)


def main():
    parser = argparse.ArgumentParser(
        description="Onboarding bootstrap-handshake E2E (ADR-059)"
    )
    parser.add_argument("--gateway-bin", default=None, help="path to acowork-gateway binary")
    parser.add_argument(
        "--packages",
        nargs="*",
        default=DEFAULT_PACKAGES,
        help="agent package basenames to install (without .agent suffix)",
    )
    parser.add_argument(
        "--scenario",
        choices=["cold", "warm", "reconnect", "all"],
        default="all",
        help="scenario(s) to run (default: all)",
    )
    parser.add_argument(
        "--keep-home",
        action="store_true",
        help="do not delete ACOWORK_HOME on exit (debug inspection)",
    )
    args = parser.parse_args()

    args.gw_bin = Path(args.gateway_bin) if args.gateway_bin else DEFAULT_GATEWAY_BIN
    if not args.gw_bin.exists():
        fail(f"gateway binary missing: {args.gw_bin}")
        sys.exit(1)

    print("=" * 60)
    print("Onboarding Bootstrap-Handshake E2E (ADR-059)")
    print(f"  Gateway: {args.gw_bin}")
    print(f"  Packages: {args.packages}")
    print(f"  Scenario: {args.scenario}")
    print("=" * 60)

    scenarios = ["cold", "warm", "reconnect"] if args.scenario == "all" else [args.scenario]
    for sc in scenarios:
        run_scenario(sc, args)

    total = passed + failed + skipped
    print(f"\n{'=' * 60}")
    print(f"Results: {passed} passed, {failed} failed, {skipped} skipped ({total} total)")
    if failed > 0:
        print("ONBOARDING E2E FAILED")
        sys.exit(1)
    print("ONBOARDING E2E PASSED")
    sys.exit(0)


if __name__ == "__main__":
    main()
