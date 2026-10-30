#!/usr/bin/env python
"""ADR-103 S6 smoke — the `/api/models/{provider}` data contract.

The frontend renders three protocol surfaces (the `Ⓡ` chip, the chat `R`
badge, the yellow "selected but not enabled" mark) from four fields that
`GET /api/models/{provider}` projects:

    shape              offline declaration (a UI hint, never a gate input)
    protocol_selected  what the user picked  (null = follow the data)
    protocol_active    what the D1 gate actually enabled
    protocol_probe     the D10 verdict

This runs a real Gateway against `dev/smoke/fake_responses_provider.py` and
asserts the four fields in the three states the UI has to distinguish:

    declared   offline says Responses, never probed, not enabled -> gray `R`
    verified   probe passes            -> selected==active==responses, verified
    failed     endpoint 404s           -> selected kept, active falls back (yellow)

There is no fourth "platform switch off" state: ADR-103 §4.3 removed the
global `llm.responses_api_enabled` switch, so a hand-picked `responses` row is
gated by its own probe verdict and nothing else.

`shape` is absent from the shipped catalog today (the dataset work that fills
it is separate), so the script seeds a small synthetic catalog into the
isolated data dir — that is the only way to exercise the declared tier end to
end rather than only in unit tests.

Usage: python dev/smoke/smoke_s6_models_contract.py
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
HOME = os.path.join(ROOT, ".smoke-s6")
GATEWAY = os.path.join(ROOT, "target-s1", "debug", "acowork-gateway.exe")
FAKE = os.path.join(ROOT, "dev", "smoke", "fake_responses_provider.py")

HTTP_PORT = 28896
MQTT_PORT = 28895
PROVIDER = "deepseek"
MODEL = "deepseek-chat"
# A provider the catalog does not know at all, serving the same model id.
CUSTOM = "custom-smoke"
DECLARED_ONLY = "deepseek-lite"
FAKE_PORTS = {"healthy": 28899, "missing": 28897}

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


def http(method, path, body=None, timeout=30):
    url = "http://127.0.0.1:%d%s" % (HTTP_PORT, path)
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


def start_fake(mode, port):
    env = dict(os.environ, FAKE_MODE=mode)
    p = subprocess.Popen([sys.executable, FAKE, str(port)], env=env,
                         stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    wait_port(port)
    return p


def config_path():
    return os.path.join(HOME, "config", "gateway.toml")


def home_path(*parts):
    return os.path.join(HOME, *parts).replace("\\", "/")


def write_config():
    """Write the isolated boot config.

    The TOML is written in full (vault_dir / packages_dir / data_dir are
    required, not defaulted), pointed at the isolated HOME.
    """
    path = config_path()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        f.write(
            'vault_dir = "%s"\npackages_dir = "%s"\ndata_dir = "%s"\n'
            # dev_mode auto-unlocks the vault at cold start; without it the
            # first provider save is a 500 "Vault is locked".
            "dev_mode = true\n\n"
            '[http]\nhost = "127.0.0.1"\nport = %d\n\n'
            '[mqtt]\nhost = "127.0.0.1"\nport = %d\n'
            % (
                home_path("config", "vault"),
                home_path("packages"),
                home_path("data"),
                HTTP_PORT,
                MQTT_PORT,
            )
        )


def seed_offline_catalog():
    """Declare `shape: responses` for our models.

    `enrich_model_from_offline` matches by model id across all providers, so a
    synthetic provider entry is enough to drive the D11 declared tier.
    """
    data_dir = home_path("data")
    os.makedirs(data_dir, exist_ok=True)
    catalog = {
        "providers": {
            PROVIDER: {
                "id": PROVIDER,
                "name": "DeepSeek (synthetic catalog entry)",
                "api": "https://api.deepseek.com",
                "models": {
                    # `reasoning_options` is what `enrich_model_from_offline`
                    # lifts into the `reasoning_efforts` projection; the
                    # `toggle` entry must be ignored (it carries no levels).
                    MODEL: {
                        "id": MODEL,
                        "shape": "responses",
                        "reasoning_options": [
                            {"type": "toggle"},
                            {"type": "effort", "values": ["low", "medium", "high", "xhigh", "max"]},
                        ],
                    },
                    DECLARED_ONLY: {"id": DECLARED_ONLY, "shape": "responses"},
                },
            }
        }
    }
    with open(os.path.join(data_dir, "offline_providers.json"), "w", encoding="utf-8") as f:
        json.dump(catalog, f)


def start_gateway():
    """Boot an isolated Gateway on explicit ports.

    The bind addresses are passed on the command line rather than relying on
    the TOML alone: on Windows two processes can both bind `127.0.0.1:19876`
    and `0.0.0.0:19876`, so a smoke instance that misses its config file would
    silently steal the developer's desktop traffic. Explicit ports make
    hermeticity a property of the invocation.
    """
    log = open(os.path.join(ROOT, "target-s1", "smoke-s6-gateway.log"), "ab")
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
    if proc is None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=15)
    except subprocess.TimeoutExpired:
        proc.kill()


def save_provider(base_url, models):
    return http("POST", "/api/providers", {
        "provider": PROVIDER,
        "name": "Acme",
        "keys": [{"alias": "", "key": "sk-smoke"}],
        "base_url": base_url + "/v1",
        "models": models,
        "model_protocols": {MODEL: "responses"},
        "model_base_urls": {MODEL: base_url + "/v1"},
    })


def update_provider(base_url, models):
    return http("PUT", "/api/providers/" + PROVIDER, {
        "base_url": base_url + "/v1",
        "models": models,
        "model_protocols": {MODEL: "responses"},
        "model_base_urls": {MODEL: base_url + "/v1"},
    })


def rows(provider=PROVIDER):
    status, body = http("GET", "/api/models/" + provider)
    if status != 200:
        raise RuntimeError("GET /api/models/%s -> %s %s" % (provider, status, body))
    models = body.get("models") if isinstance(body, dict) else body
    return {m.get("id"): m for m in models or []}


def projection(row):
    probe = row.get("protocol_probe") or {}
    return {
        "shape": row.get("shape"),
        "protocol_selected": row.get("protocol_selected"),
        "protocol_active": row.get("protocol_active"),
        "probe.state": probe.get("state"),
    }


def main():
    if not os.path.exists(GATEWAY):
        sys.exit("missing %s — build it first (CARGO_TARGET_DIR=target-s1)" % GATEWAY)
    for port in [HTTP_PORT, MQTT_PORT] + list(FAKE_PORTS.values()):
        if not free(port):
            sys.exit("port %d is busy" % port)

    if os.path.isdir(HOME):
        shutil.rmtree(HOME)
    write_config()
    seed_offline_catalog()

    healthy = start_fake("healthy", FAKE_PORTS["healthy"])
    missing = start_fake("missing", FAKE_PORTS["missing"])
    gateway = None
    try:
        gateway = start_gateway()

        print("\n[1] verified: the probe passes on save")
        status, body = save_provider("http://127.0.0.1:%d" % FAKE_PORTS["healthy"], [MODEL])
        check("POST /api/providers status", status, 201)
        if status != 201:
            print("    body:", body)
        probes = (body or {}).get("probes") or []
        check("save returned one probe", len(probes), 1)
        four = projection(rows()[MODEL])
        check("shape (offline declaration)", four["shape"], "responses")
        check("protocol_selected", four["protocol_selected"], "responses")
        check("protocol_active", four["protocol_active"], "responses")
        check("protocol_probe.state", four["probe.state"], "verified")

        print("\n[2] failed: the Responses endpoint 404s")
        status, _ = update_provider("http://127.0.0.1:%d" % FAKE_PORTS["missing"], [MODEL])
        check("PUT /api/providers status", status, 200)
        four = projection(rows()[MODEL])
        # The user's choice survives (that is what the yellow badge explains),
        # but nothing is enabled until it verifies.
        check("shape", four["shape"], "responses")
        check("protocol_selected survives", four["protocol_selected"], "responses")
        check("protocol_active falls back", four["protocol_active"], "openai")
        check("protocol_probe.state", four["probe.state"], "failed")

        print("\n[3] declared: offline hint only, never selected, never probed")
        # A second model the user never selected — the gray `R` tier. It rides
        # along in the same write; nothing about it is probed.
        status, _ = update_provider("http://127.0.0.1:%d" % FAKE_PORTS["healthy"],
                                    [MODEL, DECLARED_ONLY])
        check("PUT /api/providers status", status, 200)
        declared = projection(rows()[DECLARED_ONLY])
        check("shape", declared["shape"], "responses")
        check("protocol_selected is null", declared["protocol_selected"], None)
        check("protocol_active is openai", declared["protocol_active"], "openai")
        check("no probe verdict", declared["probe.state"], None)

        print("\n[4] reasoning_efforts: the catalog's declared levels reach the API")
        # The frontend shows exactly these levels in the effort dropdown and
        # the Runtime clamps a chosen level into them, so the projection has
        # to survive to `/api/models` untouched.
        proj = rows()
        check("declared levels", proj[MODEL].get("reasoning_efforts"),
              ["low", "medium", "high", "xhigh", "max"])
        check("undeclared model stays unknown", proj[DECLARED_ONLY].get("reasoning_efforts"), None)

        print("\n[5] a custom provider does NOT inherit another endpoint's levels")
        # Same model id, different endpoint: the catalog's level set describes
        # the endpoint it was declared for, not this one, so the UI must fall
        # back to the generic set instead of showing (and clamping to) a
        # capability this endpoint never claimed.
        status, _ = http("POST", "/api/providers", {
            "provider": CUSTOM,
            "name": "Custom Acme",
            "keys": [{"alias": "", "key": "sk-smoke"}],
            "base_url": "http://127.0.0.1:%d/v1" % FAKE_PORTS["healthy"],
            "models": [MODEL],
        })
        check("POST /api/providers (custom) status", status, 201)
        custom_row = rows(CUSTOM).get(MODEL) or {}
        check("custom provider: no declared levels", custom_row.get("reasoning_efforts"), None)
    finally:
        stop(gateway)
        stop(healthy)
        stop(missing)

    print("\n" + ("SMOKE_OK" if not failures else "SMOKE_FAILED: " + ", ".join(failures)))
    return 0 if not failures else 1


if __name__ == "__main__":
    sys.exit(main())