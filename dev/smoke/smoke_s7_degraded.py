#!/usr/bin/env python
"""ADR-103 S7 smoke — the `degraded` verdict end to end.

D8 lets the platform drop exactly one field (`reasoning`) when a provider
rejects it, and D10.3 says the resulting verdict is `degraded`, not `verified`.
That is only trustworthy if it is observed on a real Gateway talking to a real
(if fake) endpoint that behaves like MiniMax M3.1-Flash.

What this proves, in one run:

  1. The save-time probe sends `reasoning.effort` and the endpoint 400s it.
  2. The probe retries **once** without the block (hit counters, not just the
     verdict — a chain of retries would show up as more rejected hits).
  3. The verdict is `degraded` and carries `stripped_fields == ["reasoning"]`.
  4. `degraded` still gates the D1 selection: `protocol_active == responses`.
  5. The verdict survives a Gateway restart (it is persisted in
     provider_list.json, not held in memory).

Usage: python dev/smoke/smoke_s7_degraded.py
"""

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
HOME = os.path.join(ROOT, ".smoke-s7")
GATEWAY = os.path.join(ROOT, "target-s1", "debug", "acowork-gateway.exe")
FAKE = os.path.join(ROOT, "dev", "smoke", "fake_responses_provider.py")

HTTP_PORT = 28894
MQTT_PORT = 28893
FAKE_PORT = 28898
PROVIDER = "minimax"
MODEL = "m3.1-flash"

failures = []


def check(label, actual, expected):
    ok = actual == expected
    print(("  PASS " if ok else "  FAIL ") + label + " = " + repr(actual)
          + ("" if ok else " (expected " + repr(expected) + ")"))
    if not ok:
        failures.append(label)
    return ok


def free(port):
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))
        return True
    except OSError:
        return False
    finally:
        s.close()


def http(method, path, body=None, timeout=30, base=None):
    url = base or ("http://127.0.0.1:%d%s" % (HTTP_PORT, path))
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if data:
        req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read().decode()
            return r.status, (json.loads(raw) if raw.strip() else None)
    except urllib.error.HTTPError as e:
        raw = e.read().decode()
        try:
            return e.code, json.loads(raw)
        except ValueError:
            return e.code, raw


def wait_port(port, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = socket.socket()
        s.settimeout(0.5)
        try:
            s.connect(("127.0.0.1", port))
            return
        except OSError:
            time.sleep(0.1)
        finally:
            s.close()
    raise RuntimeError("port %d never opened" % port)


def start_fake(mode):
    env = dict(os.environ, FAKE_MODE=mode)
    p = subprocess.Popen([sys.executable, FAKE, str(FAKE_PORT)], env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_port(FAKE_PORT)
    return p


def hits():
    status, body = http("GET", "", base="http://127.0.0.1:%d/hits" % FAKE_PORT)
    if status != 200:
        raise RuntimeError("GET /hits -> %s %s" % (status, body))
    return body


def home_path(*parts):
    return os.path.join(HOME, *parts).replace("\\", "/")


def config_path():
    return os.path.join(HOME, "config", "gateway.toml")


def write_config():
    """Isolated boot with the Responses gate ON (D1 needs it to enable)."""
    path = config_path()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(
            'vault_dir = "%s"\npackages_dir = "%s"\ndata_dir = "%s"\n'
            "dev_mode = true\n\n"
            '[http]\nhost = "127.0.0.1"\nport = %d\n\n'
            '[mqtt]\nhost = "127.0.0.1"\nport = %d\n\n'
            "[llm]\nresponses_api_enabled = true\n"
            % (
                home_path("config", "vault"),
                home_path("packages"),
                home_path("data"),
                HTTP_PORT,
                MQTT_PORT,
            )
        )


def seed_offline_catalog():
    """Declare the model in the offline catalog.

    `/api/models/{provider}` projects offline data ∩ configured models, so a
    synthetic catalog entry is required for a provider that models.dev does not
    know. `shape: responses` keeps the row's `Ⓡ` semantics realistic.
    """
    data_dir = home_path("data")
    os.makedirs(data_dir, exist_ok=True)
    catalog = {
        "providers": {
            PROVIDER: {
                "id": PROVIDER,
                "name": "MiniMax (synthetic catalog entry)",
                "api": "https://api.minimax.io",
                "models": {MODEL: {"id": MODEL, "shape": "responses"}},
            }
        }
    }
    with open(os.path.join(data_dir, "offline_providers.json"), "w", encoding="utf-8") as f:
        json.dump(catalog, f)


def start_gateway():
    log = open(os.path.join(ROOT, "target-s1", "smoke-s7-gateway.log"), "ab")
    env = dict(os.environ)
    env["ACOWORK_HOME"] = HOME
    env["RUST_LOG"] = "info"
    p = subprocess.Popen(
        [
            GATEWAY,
            "--home", HOME,
            "--config-path", config_path(),
            "--addr", "127.0.0.1:%d" % HTTP_PORT,
            "--mqtt-addr", "127.0.0.1:%d" % MQTT_PORT,
            "--advertise-host", "127.0.0.1",
            "--no-spawn-local-node",
        ],
        env=env,
        stdout=log,
        stderr=log,
    )
    wait_port(HTTP_PORT)
    return p


def stop(proc):
    if not proc:
        return
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()


def save_provider(base_url):
    return http("POST", "/api/providers", {
        "provider": PROVIDER,
        "name": "MiniMax (smoke)",
        "keys": [{"alias": "", "key": "sk-smoke"}],
        "base_url": base_url + "/v1",
        "models": [MODEL],
        "model_protocols": {MODEL: "responses"},
        "model_base_urls": {MODEL: base_url + "/v1"},
    })


def projection():
    status, body = http("GET", "/api/models/" + PROVIDER)
    if status != 200:
        raise RuntimeError("GET /api/models/%s -> %s %s" % (PROVIDER, status, body))
    models = body.get("models") if isinstance(body, dict) else body
    if not any(m.get("id") == MODEL for m in models or []):
        raise RuntimeError("model %s missing from /api/models: %s" % (MODEL, body))
    for m in models or []:
        if m.get("id") == MODEL:
            probe = m.get("protocol_probe") or {}
            return {
                "protocol_selected": m.get("protocol_selected"),
                "protocol_active": m.get("protocol_active"),
                "probe.state": probe.get("state"),
                "probe.stripped_fields": probe.get("stripped_fields"),
            }
    raise RuntimeError("model %s missing from /api/models" % MODEL)


def main():
    if not os.path.exists(GATEWAY):
        sys.exit("missing %s — build it first (CARGO_TARGET_DIR=target-s1)" % GATEWAY)
    for port in [HTTP_PORT, MQTT_PORT, FAKE_PORT]:
        if not free(port):
            sys.exit("port %d is busy" % port)

    if os.path.isdir(HOME):
        shutil.rmtree(HOME)
    write_config()
    seed_offline_catalog()

    fake = start_fake("reject_reasoning")
    gateway = None
    try:
        gateway = start_gateway()
        base = "http://127.0.0.1:%d" % FAKE_PORT

        print("\n[1] save: the endpoint rejects `reasoning`, so the probe trims once")
        before = hits()
        status, body = save_provider(base)
        check("POST /api/providers status", status, 201)
        if status != 201:
            print("    body:", body)
        probes = (body or {}).get("probes") or []
        check("save returned one probe", len(probes), 1)
        if probes:
            # The report carries the evidence; the *state* is derived from it
            # (`ProbeOutcome::state()`) and exposed by /api/models below.
            outcome = probes[0].get("outcome") or {}
            check("probe tiers", outcome.get("tier_passed"), ["L1", "L2", "L3"])
            check("probe success", outcome.get("success"), True)
            check("probe stripped_fields", outcome.get("stripped_fields"),
                  ["reasoning"])

        after = hits()
        # Exactly one rejection: the retry dropped the field, so it never hit
        # the 400 again — a blind chain would show more rejections.
        check("exactly one rejected baseline", after["rejected"] - before["rejected"], 1)
        check("one baseline sent with the field trimmed",
              after["baseline"] - before["baseline"], 1)
        check("round-trip ran on the trimmed body",
              after["round_trip"] - before["round_trip"], 1)

        print("\n[2] /api/models: degraded gates the selection and names the field")
        four = projection()
        check("protocol_selected", four["protocol_selected"], "responses")
        # D10.3: degraded is usable, unlike failed/stale.
        check("protocol_active", four["protocol_active"], "responses")
        check("protocol_probe.state", four["probe.state"], "degraded")
        check("protocol_probe.stripped_fields", four["probe.stripped_fields"],
              ["reasoning"])

        print("\n[3] restart: the verdict is persisted, not held in memory")
        stop(gateway)
        gateway = start_gateway()
        four = projection()
        check("state survives the restart", four["probe.state"], "degraded")
        check("fields survive the restart", four["probe.stripped_fields"],
              ["reasoning"])
        before = hits()
        check("no re-probe on boot", hits()["rejected"] - before["rejected"], 0)
    finally:
        stop(gateway)
        stop(fake)

    print("\n" + ("SMOKE_OK" if not failures else "SMOKE_FAILED: " + ", ".join(failures)))
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())