#!/usr/bin/env bash
# build_core.sh - Cross-platform rebuild (and optional start) Gateway + Runtime
# Usage: ./dev/common/build_core.sh [OPTIONS]
#   (no args)                Build release (default)
#   --debug                  Build debug
#   --release                Build release
#   --start                  Build release + start Gateway
#   --stop                   Build release + stop Gateway
#   --start --debug          Build debug + start Gateway
#   --stop --debug           Build debug + stop Gateway
#   --local                  With --start: bind Gateway to 127.0.0.1 only (default)
#   --remote                 With --start: bind Gateway to 0.0.0.0 and advertise
#                            the detected LAN IP. Implies --start.
#   --skip-embed             Skip building the embedding runtime
#   -h, --help               Show this help
#
# Profile selection: --debug/--release flag > $ACOWORK_BUILD_PROFILE > release
# In debug profile, $ACOWORK_GATEWAY_LOG_LEVEL is auto-exported to "debug" so
# any gateway process spawned from this script inherits verbose logging.
# Supports: Linux, macOS, Windows (Git Bash, WSL, MSYS2)

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
GRAY='\033[0;37m'
NC='\033[0m' # No Color

# Determine workspace root
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"
CORE_DIR="$WORKSPACE_ROOT/core"

# Detect OS
OS="unknown"
case "$(uname -s)" in
    Linux*)     OS="linux";;
    Darwin*)    OS="macos";;
    CYGWIN*)    OS="windows";;
    MINGW*)     OS="windows";;
    MSYS*)      OS="windows";;
    *)          OS="unknown";;
esac

# Parse arguments
PROFILE="release"
START_GATEWAY=true
STOP_GATEWAY=false
SKIP_EMBED=false
NETWORK_MODE="default"   # default|local|remote
for arg in "$@"; do
    case "$arg" in
        --debug)      PROFILE="debug" ;;
        --release)    PROFILE="release" ;;
        --start)      START_GATEWAY=true ;;
        --stop)       STOP_GATEWAY=true ;;
        --local)
            [ "$NETWORK_MODE" = "remote" ] && { echo -e "${RED}ERROR: --local and --remote are mutually exclusive.${NC}"; exit 1; }
            NETWORK_MODE="local" ;;
        --remote)
            [ "$NETWORK_MODE" = "local" ] && { echo -e "${RED}ERROR: --local and --remote are mutually exclusive.${NC}"; exit 1; }
            NETWORK_MODE="remote"; START_GATEWAY=true ;;
        --skip-embed) SKIP_EMBED=true ;;
        -h|--help)
            cat <<'EOF'
Usage: ./dev/common/build_core.sh [OPTIONS]

Options:
  --debug           Build debug (auto-enables ACOWORK_GATEWAY_LOG_LEVEL=debug)
  --release         Build release (default)
  --start           Build release + start Gateway
  --stop            Build release + stop Gateway
  --start --debug   Build debug + start Gateway
  --stop --debug    Build debug + stop Gateway
  --local           With --start: bind Gateway to 127.0.0.1 only (default)
  --remote          With --start: bind Gateway to 0.0.0.0 (LAN-reachable) and
                    advertise the detected LAN IP. Implies --start.
  --skip-embed      Skip the embedding runtime build
  -h, --help        Show this help

Environment:
  ACOWORK_BUILD_PROFILE   debug|release  (overridden by --debug/--release)

Default behavior: build release, then start the gateway daemon in background.
EOF
            exit 0
            ;;
        *) echo -e "${RED}Unknown option: $arg${NC}"; exit 1 ;;
    esac
done

# --local / --remote are mutually exclusive and require --start (or no --stop).
if [ "$NETWORK_MODE" != "default" ] && [ "$STOP_GATEWAY" = "true" ]; then
    echo -e "${RED}ERROR: --local/--remote cannot be combined with --stop (no Gateway is started).${NC}"
    exit 1
fi
if [ "$NETWORK_MODE" = "local" ] && [ "$START_GATEWAY" = "false" ]; then
    echo -e "${YELLOW}WARN: --local has no effect without --start; ignoring.${NC}"
    NETWORK_MODE="default"
fi

# Env var fallback for profile (CLI flag wins).
if [ -n "$ACOWORK_BUILD_PROFILE" ]; then
    env_profile="$(echo "$ACOWORK_BUILD_PROFILE" | tr '[:upper:]' '[:lower:]' | xargs)"
    case "$env_profile" in
        debug|release) PROFILE="$env_profile" ;;
        *) echo -e "${YELLOW}WARN: ignoring unknown ACOWORK_BUILD_PROFILE='$env_profile' (expected 'debug' or 'release')${NC}" ;;
    esac
fi

# Runtime env linkage: debug profile auto-enables gateway verbose logging for
# any child process spawned from this script.
if [ "$PROFILE" = "debug" ]; then
    export ACOWORK_GATEWAY_LOG_LEVEL="debug"
fi

# ponytail: best-effort LAN IP detection. The Gateway CLI's --advertise-host
# wins over the auto-detect fallback (which only WARNs on miss). Ceiling:
# VPN-only / containerized / multi-NIC setups — operator should set
# [advertise_host] in gateway.toml for a deterministic value.
detect_lan_ip() {
    case "$OS" in
        macos)
            # en0 = default Wi-Fi on Apple hardware; fall back to en1.
            ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true
            ;;
        linux)
            # UDP-connect trick: ask the kernel what source it would use for
            # 1.1.1.1 — no packets sent, just route lookup.
            ip -4 route get 1.1.1.1 2>/dev/null \
                | awk '/src/ {for(i=1;i<=NF;i++) if($i=="src"){print $(i+1); exit}}'
            ;;
        windows)
            # Git Bash / MSYS2 — defer to PowerShell. Mirrors build_core.ps1:
            # prefer the IPv4 of the interface that owns the default route
            # (virtual adapters such as VMware VMnet / WSL never own it, so
            # this cannot pick a non-LAN-reachable address), then fall back
            # to the first non-loopback, non-APIPA address.
            powershell -NoProfile -Command "
                \$r = Get-NetRoute -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue |
                      Sort-Object RouteMetric, InterfaceMetric | Select-Object -First 1
                \$ip = if (\$r) {
                    Get-NetIPAddress -AddressFamily IPv4 -InterfaceIndex \$r.InterfaceIndex -ErrorAction SilentlyContinue |
                        Where-Object { \$_.IPAddress -ne '127.0.0.1' } | Select-Object -First 1 -ExpandProperty IPAddress
                }
                if (-not \$ip) {
                    \$ip = Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue |
                        Where-Object { \$_.IPAddress -ne '127.0.0.1' -and \$_.IPAddress -notmatch '^169\.254\.' } |
                        Select-Object -First 1 -ExpandProperty IPAddress
                }
                \$ip
            " 2>/dev/null | tr -d '\r\n ' || true
            ;;
        *) echo "" ;;
    esac
}

# Build the gateway bind/advertise args from $NETWORK_MODE. The Gateway CLI
# (core/acowork-gateway/src/cli.rs) accepts:
#   --addr HOST:PORT          HTTP bind   (default 127.0.0.1:19876)
#   --mqtt-addr HOST:PORT     MQTT bind   (default 127.0.0.1:19875)
#   --advertise-host HOST     IP distributed to Node Agents / Desktop
GATEWAY_ARGS=()
if [ "$NETWORK_MODE" = "local" ]; then
    # Pin the loopback advertise host (same as the Desktop app) so
    # published endpoints stay loopback-reachable on a loopback-bound
    # Gateway instead of leaking a LAN IP it cannot serve.
    GATEWAY_ARGS=(--addr 127.0.0.1:19876 --mqtt-addr 127.0.0.1:19875 --advertise-host 127.0.0.1)
elif [ "$NETWORK_MODE" = "remote" ]; then
    # Bind 0.0.0.0 unconditionally — that is the whole point of --remote;
    # the advertise host is best-effort and only appended when detected
    # (the Gateway auto-detects a good value itself, via its UDP route probe).
    GATEWAY_ARGS=(--addr 0.0.0.0:19876 --mqtt-addr 0.0.0.0:19875)
    LAN_IP="$(detect_lan_ip || true)"
    if [ -z "$LAN_IP" ]; then
        echo -e "${YELLOW}WARN: could not detect a non-loopback IPv4 address for --remote; binding 0.0.0.0 and letting the Gateway auto-detect the advertise host. Set [advertise_host] in gateway.toml for a deterministic value.${NC}"
    else
        GATEWAY_ARGS+=(--advertise-host "$LAN_IP")
        echo -e "${CYAN}Remote mode: Gateway will bind 0.0.0.0 and advertise $LAN_IP${NC}"
    fi
fi

TARGET_DIR="$WORKSPACE_ROOT/target/$PROFILE"

echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}ACowork Core Rebuild & Restart Script${NC}"
echo -e "${CYAN}OS: $OS   Profile: $PROFILE   Network: $NETWORK_MODE${NC}"
echo -e "${CYAN}========================================${NC}"
echo ""

# Function to stop process by name
stop_process() {
    local proc_name="$1"
    local display_name="$2"
    
    if [ "$OS" = "windows" ]; then
        # Windows: use taskkill or PowerShell
        local pids=$(powershell -Command "Get-Process -Name '$proc_name' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id" 2>/dev/null || true)
        if [ -n "$pids" ]; then
            echo -e "${GRAY}  Found $display_name processes: $pids${NC}"
            powershell -Command "Stop-Process -Name '$proc_name' -Force -ErrorAction SilentlyContinue" 2>/dev/null || true
            echo -e "${GREEN}  $display_name stopped.${NC}"
        else
            echo -e "${GRAY}  No $display_name process running.${NC}"
        fi
    else
        # Linux/macOS: use pkill or kill
        local pids=$(pgrep -f "$proc_name" 2>/dev/null || true)
        if [ -n "$pids" ]; then
            echo -e "${GRAY}  Found $display_name processes: $pids${NC}"
            pkill -f "$proc_name" 2>/dev/null || true
            # Wait for process to actually terminate
            sleep 1
            echo -e "${GREEN}  $display_name stopped.${NC}"
        else
            echo -e "${GRAY}  No $display_name process running.${NC}"
        fi
    fi
}

# Step 1: Stop running processes (only when we are about to start a new one)
if [ "$START_GATEWAY" = "true" ] || [ "$STOP_GATEWAY" = "true" ]; then
    echo -e "${YELLOW}[1/8] Stopping running Gateway, Runtime, Embed, LSP Relay, Node Agent, PM, Doc, and User processes...${NC}"
    stop_process "acowork-gateway" "Gateway"
    stop_process "acowork-runtime" "Runtime"
    stop_process "acowork-embed"  "Embed"
    # The LSP Relay runs in its own process group (see
    # core/acowork-gateway/src/lifecycle/lsp_relay.rs: cmd.process_group(0)),
    # so a Gateway shutdown does NOT cascade a SIGHUP/SIGTERM to it — we must
    # explicitly kill it to avoid leaving an orphan binding port 19878, which
    # would otherwise be attached by the new gateway via
    # attach_existing_lsp_relay() but owned by a now-dead parent.
    stop_process "acowork-lsp-relay" "LSP Relay"
    # The Node Agent is spawned by the Gateway (ADR-055 §6.11) and is a
    # child of the Gateway's process group; kill it explicitly so a node left
    # behind by a killed Gateway does not keep a stale broker connection.
    stop_process "acowork-node" "Node Agent"
    # The PM service is a standalone process (ADR-064) spawned by the Gateway
    # supervisor. It self-exits via the ADR-018 watchdog when the Gateway dies,
    # but the watchdog poll can lag — kill it explicitly so the stop step is
    # idempotent and port 18082 is released before the next start.
    stop_process "acowork-pm" "PM"
    # The Doc service mirrors the PM pattern (ADR-064). It is a standalone
    # process (`acowork-doc`) spawned by the Gateway supervisor and listens on
    # port 18081 by default. The ADR-018 watchdog self-exit can lag, so kill
    # it explicitly to keep the stop step idempotent and to release 18081.
    stop_process "acowork-doc" "Doc"
    # The User service mirrors the PM/Doc pattern (ADR-084). It is a standalone
    # process (`acowork-user`) spawned by the Gateway supervisor and listens on
    # port 18083 by default. The ADR-018 watchdog self-exit can lag, so kill it
    # explicitly to keep the stop step idempotent and to release 18083.
    stop_process "acowork-user" "User"

    # Ensure embed port is released before starting a new gateway.
    # On Unix, pkill may not have finished releasing port 18080 within the
    # 1s sleep; the new gateway spawns its own embed immediately and if the
    # old one is still binding, the new embed panics with AddrInUse.
    if [ "$OS" = "linux" ] || [ "$OS" = "macos" ]; then
        if command -v fuser &>/dev/null; then
            fuser -k 18080/tcp 2>/dev/null || true
        fi
        # Wait up to 3s for the port to be released
        waited=0
        while command -v ss &>/dev/null && ss -tlnp 2>/dev/null | grep -q ":18080 "; do
            sleep 0.5
            waited=$((waited + 1))
            if [ $waited -ge 6 ]; then
                echo -e "${RED}  WARNING: Port 18080 still in use after 3s${NC}"
                break
            fi
        done

        # Free LSP Relay port 19878 (see note above about process_group(0)).
        # Independent counter so embed-port wait doesn't pre-empt the relay-port wait.
        if command -v fuser &>/dev/null; then
            fuser -k 19878/tcp 2>/dev/null || true
        fi
        lsp_waited=0
        while command -v ss &>/dev/null && ss -tlnp 2>/dev/null | grep -q ":19878 "; do
            sleep 0.5
            lsp_waited=$((lsp_waited + 1))
            if [ $lsp_waited -ge 6 ]; then
                echo -e "${RED}  WARNING: Port 19878 still in use after 3s${NC}"
                break
            fi
        done

        # Free PM port 18082 (ADR-064 standalone process). The PM supervisor
        # auto-increments on conflict, but a stale process from a killed Gateway
        # would otherwise hold the default port and shift PM to 18083+.
        if command -v fuser &>/dev/null; then
            fuser -k 18082/tcp 2>/dev/null || true
        fi
        pm_waited=0
        while command -v ss &>/dev/null && ss -tlnp 2>/dev/null | grep -q ":18082 "; do
            sleep 0.5
            pm_waited=$((pm_waited + 1))
            if [ $pm_waited -ge 6 ]; then
                echo -e "${RED}  WARNING: Port 18082 still in use after 3s${NC}"
                break
            fi
        done

        # Free Doc port 18081 (ADR-064 standalone process). Same rationale as
        # the PM port block above: a stale doc from a killed Gateway would
        # hold the default port and shift the new doc to 18082+.
        if command -v fuser &>/dev/null; then
            fuser -k 18081/tcp 2>/dev/null || true
        fi
        doc_waited=0
        while command -v ss &>/dev/null && ss -tlnp 2>/dev/null | grep -q ":18081 "; do
            sleep 0.5
            doc_waited=$((doc_waited + 1))
            if [ $doc_waited -ge 6 ]; then
                echo -e "${RED}  WARNING: Port 18081 still in use after 3s${NC}"
                break
            fi
        done

        # Free User port 18083 (ADR-084 standalone process). Same rationale as
        # the PM/Doc port blocks above: a stale user service from a killed
        # Gateway would hold the default port and shift the new one to 18084+.
        if command -v fuser &>/dev/null; then
            fuser -k 18083/tcp 2>/dev/null || true
        fi
        user_waited=0
        while command -v ss &>/dev/null && ss -tlnp 2>/dev/null | grep -q ":18083 "; do
            sleep 0.5
            user_waited=$((user_waited + 1))
            if [ $user_waited -ge 6 ]; then
                echo -e "${RED}  WARNING: Port 18083 still in use after 3s${NC}"
                break
            fi
        done
    fi
    echo ""
fi

# Step 2: Build Gateway
echo -e "${YELLOW}[2/8] Building Gateway ($PROFILE mode)...${NC}"
cd "$CORE_DIR"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-gateway)
else
    cargo_args=(cargo build -p acowork-gateway)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/gateway_build.log; then
    if grep -q "error\[" /tmp/gateway_build.log 2>/dev/null; then
        echo -e "${RED}  Gateway build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  Gateway build completed.${NC}"
else
    echo -e "${RED}  Gateway build failed.${NC}"
    exit 1
fi
echo ""

# Step 3: Build Runtime
echo -e "${YELLOW}[3/8] Building Runtime ($PROFILE mode)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-runtime)
else
    cargo_args=(cargo build -p acowork-runtime)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/runtime_build.log; then
    if grep -q "error\[" /tmp/runtime_build.log 2>/dev/null; then
        echo -e "${RED}  Runtime build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  Runtime build completed.${NC}"
else
    echo -e "${RED}  Runtime build failed.${NC}"
    exit 1
fi
echo ""

# Step 3.5: Build Embedding Runtime
#
# detect_ort_local probes .ort/onnxruntime-*/lib for a local ONNX Runtime
# install and exports ORT_LIB_LOCATION / ORT_DYLIB_PATH /
# ORT_PREFER_DYNAMIC_LINK before invoking cargo. Run
# dev/common/setup_ort.sh first to download ORT (the script will offer to
# do that interactively when ORT is missing and a terminal is attached).
#
# Users can skip this step entirely with: ./dev/common/build_core.sh --skip-embed

# Probe .ort/onnxruntime-*/lib and export ORT_* env vars on success.
# Returns 0 when a usable library was found, 1 otherwise. Exposed as a
# function because it must run twice in the missing-ORT path: once before
# the setup_ort prompt and once after (setup_ort's exports live in its own
# child shell and do not propagate back to this script).
detect_ort_local() {
    for ort_dir in "$WORKSPACE_ROOT"/.ort/onnxruntime-*; do
        [ -d "$ort_dir" ] || continue
        local local_lib=""
        case "$OS" in
            macos)   local_lib="$ort_dir/lib/libonnxruntime.dylib" ;;
            windows) local_lib="$ort_dir/lib/onnxruntime.dll" ;;
            *)       local_lib="$ort_dir/lib/libonnxruntime.so" ;;
        esac
        if [ -f "$local_lib" ]; then
            export ORT_LIB_LOCATION="$ort_dir/lib"
            export ORT_DYLIB_PATH="$local_lib"
            export ORT_PREFER_DYNAMIC_LINK=1
            echo -e "${GREEN}  Detected local ORT: $ort_dir/lib${NC}"
            return 0
        fi
    done
    return 1
}

if [ "$SKIP_EMBED" = "true" ]; then
    echo -e "${YELLOW}[3.5/8] Skipping Embedding Runtime (--skip-embed).${NC}"
else
    echo -e "${YELLOW}[3.5/8] Building Embedding Runtime ($PROFILE mode)...${NC}"

    # Auto-detect local ONNX Runtime install under .ort/
    if [ -z "$ORT_LIB_LOCATION" ]; then
        detect_ort_local || true
    fi
    
    # Fallback: Auto-detect from Cargo cache if download-ort feature was used
    if [ -z "$ORT_LIB_LOCATION" ]; then
        echo -e "${YELLOW}  .ort/ not found, searching Cargo registry cache...${NC}"
        for cached_ort in $(find ~/.cargo/registry/cache -maxdepth 5 -type d \( -name "onnxruntime-osx-aarch64-*" -o -name "onnxruntime-osx-x64-*" \) 2>/dev/null); do
            [ -d "$cached_ort" ] || continue
            if [ "$OS" = "macos" ]; then
                lib_path="$cached_ort/lib/libonnxruntime.dylib"
            else
                lib_path="$cached_ort/lib/libonnxruntime.so"
            fi
            if [ -f "$lib_path" ]; then
                echo -e "${GREEN}  Found ONNX Runtime in Cargo cache: $lib_path${NC}"
                # Create a symlink in .ort/ so build_core.sh can use it
                WORKSPACE_ROOT_ORT="$WORKSPACE_ROOT/.ort/onnxruntime-osx-aarch64-latest"
                mkdir -p "$WORKSPACE_ROOT_ORT/lib"
                cp "$lib_path" "$WORKSPACE_ROOT_ORT/lib/"
                echo -e "${GREEN}  Copied to $WORKSPACE_ROOT_ORT${NC}"
                export ORT_LIB_LOCATION="$WORKSPACE_ROOT_ORT/lib"
                export ORT_DYLIB_PATH="$lib_path"
                export ORT_PREFER_DYNAMIC_LINK=1
                break
            fi
        done
    fi
    if [ -z "$ORT_LIB_LOCATION" ]; then
        # Offer to run setup_ort.sh in-place. Guards:
        #   - `[ -t 0 ]`: only prompt with a real terminal. CI / piped stdin
        #     keeps the historical hard-exit behavior (never block on input).
        #   - OS check: setup_ort.sh disclaims Windows (Git Bash/WSL) in its
        #     own header, so there we point to the PowerShell path instead.
        if [ -t 0 ] && [ "$OS" != "windows" ]; then
            read -r -p "  Run ./dev/common/setup_ort.sh now? [Y/n] " ort_reply || ort_reply="n"
            case "$ort_reply" in
                ""|y|Y|yes|YES|Yes)
                    echo -e "${CYAN}  Running setup_ort.sh ...${NC}"
                    if bash "$SCRIPT_DIR/setup_ort.sh"; then
                        # setup_ort's exports do not cross the process
                        # boundary; re-probe .ort/ to pick up the install.
                        detect_ort_local || true
                    else
                        echo -e "${RED}  setup_ort.sh failed. Check network/mirror, or run it manually.${NC}"
                    fi
                    ;;
                *)
                    echo -e "${GRAY}  Skipped setup_ort.sh.${NC}"
                    ;;
            esac
        elif [ "$OS" = "windows" ]; then
            echo -e "${YELLOW}  On Windows use .\\dev\\common\\build_core.ps1 (setup_ort.sh is not supported under Git Bash/WSL).${NC}"
        fi
    fi
    if [ -z "$ORT_LIB_LOCATION" ]; then
        echo -e "${RED}  ONNX Runtime not found. Run ./dev/common/setup_ort.sh first.${NC}"
        if [ "$PROFILE" = "release" ]; then
            echo -e "${RED}  Alternative: cargo build --release -p acowork-embed --features download-ort${NC}"
        else
            echo -e "${RED}  Alternative: cargo build -p acowork-embed --features download-ort${NC}"
        fi
        exit 1
    fi

    if [ "$PROFILE" = "release" ]; then
        cargo_args=(cargo build --release -p acowork-embed)
    else
        cargo_args=(cargo build -p acowork-embed)
    fi
    if "${cargo_args[@]}" 2>&1 | tee /tmp/embed_build.log; then
        if grep -q "error\[" /tmp/embed_build.log 2>/dev/null; then
            echo -e "${RED}  Embedding Runtime build failed with errors.${NC}"
            exit 1
        fi
        echo -e "${GREEN}  Embedding Runtime build completed.${NC}"
    else
        echo -e "${RED}  Embedding Runtime build failed.${NC}"
        exit 1
    fi

fi # end SKIP_EMBED check
rm -f /tmp/embed_build.log
echo ""

# Step 3.6: Build LSP Relay (standalone binary, sibling of acowork-gateway)
#
# The Gateway spawns `acowork-lsp-relay` as a sibling process (ADR-019). It
# locates the binary via `current_exe().parent().join(...)` — see
# core/acowork-gateway/src/lifecycle/lsp_relay.rs::spawn_lsp_relay. If the
# sibling binary is missing, Gateway startup fails with:
#   GatewayError::Lifecycle("acowork-lsp-relay binary not found at ...")
#
# We unconditionally build (no --skip-lsp-relay flag) because every Gateway
# needs an LSP Relay process to serve the runtime codebase tool and the
# desktop Monaco client.
echo -e "${YELLOW}[3.6/8] Building LSP Relay ($PROFILE mode)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-lsp-relay)
else
    cargo_args=(cargo build -p acowork-lsp-relay)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/lsp_relay_build.log; then
    if grep -q "error\[" /tmp/lsp_relay_build.log 2>/dev/null; then
        echo -e "${RED}  LSP Relay build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  LSP Relay build completed.${NC}"
else
    echo -e "${RED}  LSP Relay build failed.${NC}"
    exit 1
fi
rm -f /tmp/lsp_relay_build.log
echo ""

# Step 3.7: Build Node Agent (standalone binary, sibling of acowork-gateway)
#
# ADR-055 §6.11: the Gateway supervises a local Node Agent (`acowork-node`),
# located via `current_exe().parent().join("acowork-node")` — so the binary
# MUST sit next to acowork-gateway. Without it the Gateway silently disables
# the node topology ("acowork-node binary not found — local node agent
# disabled"), node 'local' never enrolls, and agent installs fail with 503
# "Node 'local' has never enrolled (offline)".
echo -e "${YELLOW}[3.7/8] Building Node Agent ($PROFILE mode)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-node)
else
    cargo_args=(cargo build -p acowork-node)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/node_build.log; then
    if grep -q "error\[" /tmp/node_build.log 2>/dev/null; then
        echo -e "${RED}  Node Agent build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  Node Agent build completed.${NC}"
else
    echo -e "${RED}  Node Agent build failed.${NC}"
    exit 1
fi
rm -f /tmp/node_build.log
echo ""

# Step 3.8: Build PM service (standalone binary, sibling of acowork-gateway)
#
# ADR-064: the PM service is a standalone process (`acowork-pm`), located via
# `current_exe().parent().join("acowork-pm")` — so the binary MUST sit next to
# acowork-gateway. Without it the Gateway supervisor logs "acowork-pm binary
# not found" and `/api/pm/*` returns 503.
echo -e "${YELLOW}[3.8/8] Building PM service ($PROFILE mode)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-pm)
else
    cargo_args=(cargo build -p acowork-pm)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/pm_build.log; then
    if grep -q "error\[" /tmp/pm_build.log 2>/dev/null; then
        echo -e "${RED}  PM service build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  PM service build completed.${NC}"
else
    echo -e "${RED}  PM service build failed.${NC}"
    exit 1
fi
rm -f /tmp/pm_build.log
echo ""

# Step 3.9: Build Doc service (standalone binary, sibling of acowork-gateway)
#
# Mirrors the PM service above: the Doc service is a standalone process
# (`acowork-doc`), located via `current_exe().parent().join("acowork-doc")` —
# so the binary MUST sit next to acowork-gateway. Without it the Gateway
# supervisor logs "acowork-doc binary not found" and `/api/doc/*` returns 503
# (document library unavailable).
echo -e "${YELLOW}[3.9/8] Building Doc service ($PROFILE mode)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-doc)
else
    cargo_args=(cargo build -p acowork-doc)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/doc_build.log; then
    if grep -q "error\[" /tmp/doc_build.log 2>/dev/null; then
        echo -e "${RED}  Doc service build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  Doc service build completed.${NC}"
else
    echo -e "${RED}  Doc service build failed.${NC}"
    exit 1
fi
rm -f /tmp/doc_build.log
echo ""

# Step 3.10: Build User service (standalone binary, sibling of acowork-gateway)
#
# Mirrors the Doc service above: the User service is a standalone process
# (`acowork-user`, ADR-084), located via `current_exe().parent().join("acowork-user")` —
# so the binary MUST sit next to acowork-gateway. Without it the Gateway
# supervisor logs "acowork-user binary not found" and every user-domain route
# (`/api/auth/*`, `/api/users/*`, `/api/user/*`) returns 503 (accounts,
# profiles and user chat unavailable).
echo -e "${YELLOW}[3.10/8] Building User service ($PROFILE mode)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-user)
else
    cargo_args=(cargo build -p acowork-user)
fi
if "${cargo_args[@]}" 2>&1 | tee /tmp/user_build.log; then
    if grep -q "error\[" /tmp/user_build.log 2>/dev/null; then
        echo -e "${RED}  User service build failed with errors.${NC}"
        exit 1
    fi
    echo -e "${GREEN}  User service build completed.${NC}"
else
    echo -e "${RED}  User service build failed.${NC}"
    exit 1
fi
rm -f /tmp/user_build.log
echo ""

# Step 4: Copy offline_providers.json from assets to target dir
#
# The gateway (and embed) read this from `{exe_dir}/offline_providers.json`.
# Whoever distributes the binary (this script for dev, the package installer
# for release, the Tauri bundler for desktop) is responsible for placing it
# there.
#
# We only stage into the directory matching the active profile — the previous
# "stage to both target/release and target/debug" pattern required `mkdir -p`
# to avoid the silent stray-file behavior of `cp` (and the silent wrong-target
# behavior of PowerShell `Copy-Item`).
echo -e "${YELLOW}[4/8] Copying offline_providers.json to target/$PROFILE...${NC}"
OFFLINE_SRC="$WORKSPACE_ROOT/assets/offline_providers.json"
mkdir -p "$TARGET_DIR"
if [ -f "$OFFLINE_SRC" ]; then
    cp "$OFFLINE_SRC" "$TARGET_DIR/"
    echo -e "${GREEN}  Copied to $TARGET_DIR${NC}"
else
    echo -e "${RED}  WARNING: offline_providers.json not found at $OFFLINE_SRC${NC}"
fi

# Step 4.5: Copy embedding_models.json next to the gateway + embed binaries
#
# The gateway (and embed) read this from `{exe_dir}/embedding_models.json`.
# Whoever distributes the binary (this script for dev, the package installer
# for release, the Tauri bundler for desktop) is responsible for placing it
# there. Source of truth is core/acowork-embed/assets/embedding_models.json.
echo -e "${YELLOW}[4.5/8] Copying embedding_models.json to target/$PROFILE...${NC}"
EMBED_MODELS_SRC="$WORKSPACE_ROOT/core/acowork-embed/assets/embedding_models.json"
if [ -f "$EMBED_MODELS_SRC" ]; then
    cp "$EMBED_MODELS_SRC" "$TARGET_DIR/embedding_models.json"
    echo -e "${GREEN}  Copied to $TARGET_DIR${NC}"
else
    echo -e "${RED}  WARNING: embedding_models.json not found at $EMBED_MODELS_SRC${NC}"
fi

# Step 4.6: Copy offline_embedding_providers.json next to the gateway binary
#
# The gateway reads this from `{exe_dir}/offline_embedding_providers.json`
# (cloud embedding provider catalog). Missing file = empty catalog = the UI's
# cloud-provider section shows an empty list.

echo -e "${YELLOW}[4.6/8] Copying offline_embedding_providers.json to target/$PROFILE...${NC}"

EMBEDDING_PROVIDERS_SRC="$WORKSPACE_ROOT/assets/offline_embedding_providers.json"
if [ -f "$EMBEDDING_PROVIDERS_SRC" ]; then
    cp "$EMBEDDING_PROVIDERS_SRC" "$TARGET_DIR/offline_embedding_providers.json"
    echo -e "${GREEN}  Copied to $TARGET_DIR${NC}"
else
    echo -e "${RED}  WARNING: offline_embedding_providers.json not found at $EMBEDDING_PROVIDERS_SRC${NC}"
fi

echo ""

# Step 5: Start Gateway (only when not --no-start)
if [ "$START_GATEWAY" = "true" ]; then
    # Resolve the gateway binary path up-front: both the first-boot setup
    # block further down (Step 4.7) and the actual `start` step (Step 5)
    # need it.
    GATEWAY_EXE=""
    if [ "$OS" = "windows" ]; then
        GATEWAY_EXE="$TARGET_DIR/acowork-gateway.exe"
    else
        GATEWAY_EXE="$TARGET_DIR/acowork-gateway"
    fi


    # Step 4.7: First-boot admin password setup (ADR-076 decision 12 v3).
    #
    # Mirrors dev/common/build_core.ps1:678-783. The Gateway starts in first-boot
    # restricted mode whenever its seeded `admin` account still has the
    # `$disabled$` placeholder hash -- every /api/* returns 403
    # setup_required until the operator sets a real password via the
    # in-process rpassword prompt or the `admin-setup` subcommand. The
    # in-process prompt is unreachable from this script: the daemon is
    # forked with `> /dev/null 2>&1 &` further down (Step 5), which strips
    # the daemon's TTY, so can_prompt_interactively() returns false and the
    # Gateway silently logs the setup_required warning before daemonising
    # (see core/acowork-gateway/src/cli.rs::can_prompt_interactively + the
    # v3 comment block above warn_first_boot_restricted). Detect that
    # state up-front, prompt the operator once *here* (where the parent
    # shell still has a TTY), and write the password via
    # `admin-setup --password-file` so the daemon boots clean instead of
    # dead-ending the Desktop at the SetupRequiredView gate.
    #
    # ADR-084: the account store moved to the user service's data dir
    # (`~/.acowork/acowork-user/accounts.json`); the old gateway path no
    # longer exists on fresh installs, so reading it would report "setup
    # required" forever.
    _ACCT_JSON="$HOME/.acowork/acowork-user/accounts.json"
    _NEEDS_SETUP=true
    if [ -f "$_ACCT_JSON" ]; then
        if command -v jq >/dev/null 2>&1; then
            # jq path: robust regardless of field ordering or future
            # schema additions; this is the happy path when jq is
            # installed.
            _admin_hash="$(jq -r '.accounts[] | select(.username == "admin") | .password_hash // empty' "$_ACCT_JSON" 2>/dev/null)"
            if [ -n "$_admin_hash" ] && [ "$_admin_hash" != '$disabled$' ]; then
                _NEEDS_SETUP=false
            fi
        else
            # Fallback grep path: assumes the current accounts.json field
            # ordering (username followed within 6 lines by
            # password_hash). Brittle if the user service ever reshuffles
            # fields, but works without a jq dependency.
            if ! grep -A 6 '"username": "admin"' "$_ACCT_JSON" 2>/dev/null \
                | grep -q '"password_hash": "\$disabled\$"'; then
                _NEEDS_SETUP=false
            fi
        fi
    fi

    if [ "$_NEEDS_SETUP" = "true" ]; then
        # Defensive: a missing exe here usually means cargo build above
        # failed and exited 1, but guard anyway in case the build step
        # was skipped.
        if [ ! -f "$GATEWAY_EXE" ]; then
            echo -e "${RED}ERROR: cannot run admin-setup -- gateway executable not found at: $GATEWAY_EXE${NC}"
            exit 1
        fi

        # Refuse non-interactive contexts: if the script's own stdio is
        # not a TTY (CI / pipe / redirect), `stty -echo; read` would
        # either fail or block forever, and we'd silently launch a
        # half-broken daemon. Degrade to a clear error instead.
        if [ ! -t 0 ] || [ ! -t 1 ]; then
            echo ""
            echo -e "${YELLOW}[setup] First-boot detected (admin password not set), but no interactive TTY is attached.${NC}"
            echo -e "${YELLOW}        Run admin-setup manually before starting the gateway:${NC}"
            echo -e "${YELLOW}          $GATEWAY_EXE --auth-mode multi_user admin-setup --password-file <pwfile>${NC}"
            echo ""
            exit 1
        fi

        echo ""
        echo -e "${YELLOW}[4.7/8] First-boot detected -- admin password has not been set yet.${NC}"
        echo -e "${YELLOW}        Without it, the Gateway would boot in restricted mode (every /api/* returns 403).${NC}"
        echo -e "${YELLOW}        This prompt only appears on a fresh install.${NC}"
        echo ""

        # Save terminal state and install a trap that always restores it
        # so a Ctrl-C mid-prompt can't leave the operator's shell in
        # raw/no-echo mode. We use `stty -echo` (not `read -s`) because
        # `read -s` does not mask the password on some terminals; the
        # Gateway's `can_prompt_interactively` comment in cli.rs covers
        # the same ground.
        _stty_orig="$(stty -g 2>/dev/null || true)"
        trap '[ -n "$_stty_orig" ] && stty "$_stty_orig" 2>/dev/null || true' EXIT INT TERM

        _PW1=""
        _PW2=""
        printf "New admin password: "
        stty -echo 2>/dev/null
        IFS= read -r _PW1 || _PW1=""
        stty echo 2>/dev/null
        printf "\n"

        printf "Confirm admin password: "
        stty -echo 2>/dev/null
        IFS= read -r _PW2 || _PW2=""
        stty echo 2>/dev/null
        printf "\n"

        # Restore terminal now that both reads are done; drop the trap
        # so an exit further down doesn't re-run the restore.
        if [ -n "$_stty_orig" ]; then
            stty "$_stty_orig" 2>/dev/null || true
        fi
        trap - EXIT INT TERM

        if [ -z "$_PW1" ]; then
            echo -e "${RED}ERROR: empty password rejected (also blocked by the Gateway's password policy).${NC}"
            unset _PW1 _PW2
            exit 1
        fi
        if [ "$_PW1" != "$_PW2" ]; then
            echo -e "${RED}ERROR: passwords do not match.${NC}"
            unset _PW1 _PW2
            exit 1
        fi

        # Hand the password to admin-setup via a one-shot temp file. The
        # subcommand has no --password CLI flag on purpose (would leak
        # the secret to shell history). mktemp creates a 0600 file in
        # $TMPDIR; we still immediately shred/remove it in the cleanup
        # trap, even on admin-setup failure.
        _tmp_pw="$(mktemp -t acowork-admin-pw.XXXXXX 2>/dev/null || mktemp)"
        cleanup_pw() {
            if [ -n "${_tmp_pw:-}" ] && [ -f "$_tmp_pw" ]; then
                if command -v shred >/dev/null 2>&1; then
                    shred -u "$_tmp_pw" 2>/dev/null || rm -f "$_tmp_pw"
                else
                    rm -f "$_tmp_pw"
                fi
            fi
            unset _PW1 _PW2 2>/dev/null || true
        }
        trap cleanup_pw EXIT INT TERM

        printf '%s' "$_PW1" > "$_tmp_pw"
        echo -e "${GRAY}  Setting admin password...${NC}"
        if ! "$GATEWAY_EXE" --auth-mode multi_user admin-setup --password-file "$_tmp_pw"; then
            echo -e "${RED}ERROR: admin-setup failed. Likely a password-policy violation -- check [multi_user].password_policy in gateway.toml.${NC}"
            cleanup_pw
            trap - EXIT INT TERM
            exit 1
        fi
        echo -e "${GREEN}  Admin password set.${NC}"
        cleanup_pw
        trap - EXIT INT TERM
        echo ""
    fi

    log_level="${ACOWORK_GATEWAY_LOG_LEVEL:-info}"
    echo -e "${YELLOW}[5/8] Starting Gateway in daemon mode (log level: $log_level)...${NC}"
    export ACOWORK_GATEWAY_DAEMON="true"

    if [ -f "$GATEWAY_EXE" ]; then
        if [ "${#GATEWAY_ARGS[@]}" -gt 0 ]; then
            echo -e "${GRAY}  Gateway args: ${GATEWAY_ARGS[*]}${NC}"
        fi
        if [ "$OS" = "windows" ]; then
            # Windows: start in background
            start //b //min "$GATEWAY_EXE" "${GATEWAY_ARGS[@]}" 2>/dev/null || "$GATEWAY_EXE" "${GATEWAY_ARGS[@]}" &
        else
            # Linux/macOS: start in background, suppress output
            "$GATEWAY_EXE" "${GATEWAY_ARGS[@]}" > /dev/null 2>&1 &
        fi
        echo -e "${GREEN}  Gateway started (PID: $!).${NC}"
    else
        echo -e "${RED}  Gateway executable not found at: $GATEWAY_EXE${NC}"
        exit 1
    fi

    echo ""
    echo -e "${CYAN}========================================${NC}"
    echo -e "${CYAN}Done! Gateway is running.${NC}"
    if [ "$NETWORK_MODE" = "remote" ] && [ -n "$LAN_IP" ]; then
        echo -e "${CYAN}HTTP API: http://${LAN_IP}:19876  (also reachable on the LAN)${NC}"
    else
        echo -e "${CYAN}HTTP API: http://127.0.0.1:19876${NC}"
    fi
    echo -e "${CYAN}========================================${NC}"
else
    echo ""
    echo -e "${CYAN}========================================${NC}"
    echo -e "${CYAN}Build complete (not started, --no-start).${NC}"
    echo -e "${CYAN}To start: $TARGET_DIR/acowork-gateway${NC}"
    echo -e "${CYAN}========================================${NC}"
fi

# Return to workspace root
cd "$WORKSPACE_ROOT"

# Cleanup temp files
rm -f /tmp/gateway_build.log /tmp/runtime_build.log
