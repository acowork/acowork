# dev/

Build, packaging, CI, and deployment scripts. **None of this ships in a
release artifact** — it exists so a developer can go from a clean clone to a
running stack, and so CI has one entry point per check.

A script earns a place here by being **reproducible**: a build step, a
packaging step, a CI gate, or a regression test tied to a design doc or ADR.
One-off probes, debug helpers, and ad-hoc experiments do not belong here — see
[What is deliberately absent](#what-is-deliberately-absent).

## Prerequisites

| Tool | Needed for |
|---|---|
| Rust (stable toolchain) | everything under `core/` |
| Node.js + npm | `apps/acowork-desktop`, `apps/acowork-mobile` |
| Python 3 + `pillow` | frontend smoke tests, store logos |
| PowerShell 7 (`pwsh`) | the `.ps1` scripts on Windows |

ONNX Runtime is **not** a manual prerequisite — `setup_ort.*` installs it into
`.ort/`, and `ort_env.js` makes Tauri auto-detect it on every platform.

## Layout

Scripts are grouped by **scope**, not by file type:

| Folder | Contains |
|---|---|
| [`common/`](common/) | Cross-platform build steps that ship a `.sh` + `.ps1` twin (`build_core`, `setup_ort`, `start_node`, `build-agent`, `gen_icons`). The `.sh` serves both Linux and macOS, so the twins live together rather than under an OS folder. |
| [`desktop/windows/`](desktop/windows/) · [`desktop/macos/`](desktop/macos/) · [`desktop/linux/`](desktop/linux/) | Per-OS desktop packaging (`package_desktop_*`), plus the macOS one-click build and the Windows stop-latency test. |
| [`android/`](android/) | Mobile build/sign helpers — `build_mobile.sh` / `build_mobile.ps1` are twins covering the same APK build (bash on macOS/Linux, PowerShell on Windows), and `sign_release.ps1` drives keystore creation, `gradle.properties`, and the signed release build. Authoritative flow: [apps/acowork-mobile/README.md](../apps/acowork-mobile/README.md). |
| `ci.sh`, `ort_env.js` (repo root of `dev/`) | Kept at the top level because they are referenced by path from `AGENTS.md`, `apps/acowork-desktop/package.json`, and the ADR corpus. |
| [`deploy/`](deploy/), [`e2e_frontend_smoke/`](e2e_frontend_smoke/) | Server deployment units and the frontend smoke suite (unchanged locations). |

## First build

```bash
./dev/common/setup_ort.sh           # Linux / macOS
./dev/common/setup_ort.ps1          # Windows
./dev/common/build_core.sh          # or build_core.ps1 on Windows
```

`build_core.*` builds the Gateway, Runtime, and Node Agent in the release
profile. `-Debug` / `--debug` switches to the debug profile and auto-raises
`ACOWORK_GATEWAY_LOG_LEVEL` to `debug` for every process it spawns.

## Build and run

| Script | What it does |
|---|---|
| [`build_core.ps1`](common/build_core.ps1) · [`build_core.sh`](common/build_core.sh) | Build Gateway + Runtime + Node Agent. `-Start` stops the old Gateway and starts a new one; `-Start -Remote` binds `0.0.0.0` and advertises the detected LAN IP instead of `127.0.0.1` — this is how you make the Gateway reachable from a phone on the same network. |
| [`build_macos.sh`](desktop/macos/build_macos.sh) | macOS one-click build: skips the slow `setup_ort.sh` download, pulls the Apple Silicon ONNX Runtime with `download-ort,coreml`, installs Homebrew `pkg-config`/`cmake`. |
| [`start_node.ps1`](common/start_node.ps1) · [`start_node.sh`](common/start_node.sh) | Start a local `acowork-node` daemon joined to a remote Gateway (`<host>:<port>`). |
| [`ort_env.js`](ort_env.js) | Invoked by `tauri.conf.json`'s `beforeDevCommand`; not run by hand. Probes `.ort/`, then the Cargo cache, and injects `--features download-ort[,coreml\|cuda\|directml]` when nothing is installed. |

## Packaging

| Script | What it does |
|---|---|
| [`package_desktop_windows.ps1`](desktop/windows/package_desktop_windows.ps1) | Windows installer (`.msi` / NSIS bundle). `-ReinstallOrt`, `-NoMirror`. |
| [`package_desktop_macos.sh`](desktop/macos/package_desktop_macos.sh) | macOS `.app` / `.dmg`. |
| [`package_desktop_linux.sh`](desktop/linux/package_desktop_linux.sh) | Linux `.deb` / `.rpm` / AppImage. |
| [`build_mobile.sh`](android/build_mobile.sh) · [`build_mobile.ps1`](android/build_mobile.ps1) | Android APK. Twins — bash on macOS / Linux, PowerShell on Windows, same steps and overlays either way. See [android/README.md](android/README.md). |

App icons come straight from the Tauri CLI — there is no wrapper pipeline.
[`common/gen_icons.ps1`](common/gen_icons.ps1) · [`common/gen_icons.sh`](common/gen_icons.sh)
runs `npx tauri icon assets/app-icon.svg` for **both** apps in one shot and
enforces ownership: the mobile app keeps `icons/android/` + `icons/ios/` (plus
the PNGs its `bundle.icon` references), the desktop app drops them (a desktop
bundle never ships mobile icons) — `tauri icon` has no per-platform switch, so
this split is the whole point of the script. To change the brand icon: update
`assets/app-icon.svg`, run the script. For mobile it also syncs
`icons/android/*` into the checked-out `gen/android` res so the next
`tauri android build` picks them up — do **not** re-run `tauri android init`,
which would clobber the hand-written signing block.

## Agent packages

| Script | What it does |
|---|---|
| [`build-agent.ps1`](common/build-agent.ps1) · [`build-agent.sh`](common/build-agent.sh) | Zip an `.agent` directory into a signed package, generating signing keys on first use. Pass an agent directory, or use `-All` to build every agent under `examples/`. |

## CI and tests

[`ci.sh`](ci.sh) is the single entry point; `./dev/ci.sh all` is what runs
before a release.

| Mode | Runs |
|---|---|
| `check` | `cargo check --all` |
| `clippy` | `cargo clippy --all-targets -- -D warnings` |
| `test` | `cargo test` (unit + integration) |
| `integration` | cross-process suites (relay full path, PM remote) |
| `smoke` | frontend smoke suite — boots a real Gateway + Runtime and drives them over HTTP/MQTT exactly as the Desktop would |
| `all` | all of the above, in order |

Two red lines are enforced here and are not optional:

- `run_gateway_fs_redline` — the Gateway must not read agent-private data off
  the filesystem; it goes through Runtime HTTP only. Rationale and the frozen
  ceilings are in [ADR-009 §5](../docs/adr/zh/ADR-009-gateway-workspace-isolation.md).
- `acowork-node` must not depend on `acowork-gateway` (ADR-055 §6.20).

Tests that live outside the Cargo test harness:

| Test | What it covers |
|---|---|
| [`e2e_frontend_smoke/smoke_test.py`](e2e_frontend_smoke/smoke_test.py) | The `smoke` mode above. Covers the cases enumerated in `docs/plan/zh/e2e-frontend-smoke-test.md`, plus the PM member feature end-to-end across processes. |
| [`e2e_frontend_smoke/onboarding_installs_all_agents.py`](e2e_frontend_smoke/onboarding_installs_all_agents.py) | ADR-059 first-run bootstrap handshake regression. `GET /api/bootstrap` is the readiness source of truth, **not** `/health`. |
| [`e2e_stop_test.ps1`](desktop/windows/e2e_stop_test.ps1) | Stop-response latency after the control/data channel split: open a WebSocket, stream a long message over HTTP, send `stop` on the first delta, measure the latency. Referenced by [ADR-077](../docs/adr/zh/ADR-077-system-agent-demotion-to-default-agent.md). |

## Deployment

[`deploy/relay/`](deploy/relay/) — self-hosting the relay server:

| File | What it does |
|---|---|
| [`acowork-relay.service`](deploy/relay/acowork-relay.service) | systemd unit. Add `--log-level debug` when diagnosing a wedged device tunnel: yamux logs its stream lifecycle and ping/pong RTT at debug, and that is the only evidence a stalled pipe leaves behind (design doc 24 §5.2). |
| [`certbot-wildcard.sh`](deploy/relay/certbot-wildcard.sh) | Wildcard certificate for `*.<relay-domain>` — the relay serves every device from its own subdomain, so one wildcard cert covers all of them. |

See the root [README § Relay server](../README.md) for the deployment walkthrough.

## What is deliberately absent

Scratch scripts do not live in version control. Keep one-off probes, debug
helpers, and ad-hoc experiments in `dev/tmp/` (git-ignored) or outside the
repository, and delete them once the investigation is over. `*.log` is ignored
as well.
