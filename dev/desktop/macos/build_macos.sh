#!/usr/bin/env bash
# build_macos.sh - macOS one-click build script
# Solves:
#   1. Skip setup_ort.sh GitHub download (slow on some networks)
#   2. Auto-download Apple Silicon optimized ONNX Runtime via --features download-ort,coreml
#   3. Auto-copy ONNX Runtime from Cargo cache to .ort/
#   4. Auto-configure Homebrew + pkg-config + cmake
#
# Usage:
#   ./dev/desktop/macos/build_macos.sh               # Default Apple Silicon optimized, release
#   ./dev/desktop/macos/build_macos.sh --debug       # Debug build (auto-enables ACOWORK_GATEWAY_LOG_LEVEL=debug)
#   ./dev/desktop/macos/build_macos.sh --release     # Release build (explicit, default)
#   ./dev/desktop/macos/build_macos.sh --cpu         # CPU only (best compatibility)
#   ./dev/desktop/macos/build_macos.sh --start       # Build + start Gateway in daemon mode after build
#   ./dev/desktop/macos/build_macos.sh --stop        # Explicit stop of existing processes before build
#   ./dev/desktop/macos/build_macos.sh --local       # With --start: bind Gateway to 127.0.0.1 only (default)
#   ./dev/desktop/macos/build_macos.sh --remote      # With --start: bind Gateway to 0.0.0.0 and advertise
#                                      # the detected LAN IP. Implies --start.
#   ./dev/desktop/macos/build_macos.sh --skip-embed  # Skip embed
#   ./dev/desktop/macos/build_macos.sh --help
#
# Profile selection: --debug/--release flag > $ACOWORK_BUILD_PROFILE > release

set -e

# ── Colors ──────────────────────────────────────────────────────────────────
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
GRAY='\033[0;37m'
NC='\033[0m'

# ── Paths ───────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$(dirname "$(dirname "$SCRIPT_DIR")")")"
CORE_DIR="$WORKSPACE_ROOT/core"

# ── Defaults ────────────────────────────────────────────────────────────────
ARCH="$(uname -m)"
USE_GPU=true            # Apple Silicon auto-enable CoreML
SKIP_EMBED=false
START_GATEWAY=false     # --start: start Gateway in daemon mode after build
STOP_GATEWAY=false      # --stop: explicit intent to stop existing processes
                        # before build (mirrors build_core.sh semantics;
                        # the actual stop step is also run by default when
                        # --start is given)
NETWORK_MODE="default"  # default|local|remote
SHOW_HELP=false
PROFILE="release"

# ── Parse arguments ─────────────────────────────────────────────────────────
for arg in "$@"; do
    case "$arg" in
        --debug)      PROFILE="debug" ;;
        --release)    PROFILE="release" ;;
        --cpu)        USE_GPU=false ;;
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
            cat << 'EOF'
Usage: ./dev/desktop/macos/build_macos.sh [OPTIONS]

Options:
  --debug           Build debug (auto-enables ACOWORK_GATEWAY_LOG_LEVEL=debug)
  --release         Build release (default)
  --cpu             Use CPU-only ONNX Runtime (no CoreML acceleration)
  --start           Build + start Gateway in daemon mode after build
  --stop            Explicit stop of existing Gateway/Runtime/Embed/LSP Relay
                    before build (mirrors build_core.sh semantics)
  --local           With --start: bind Gateway to 127.0.0.1 only (default)
  --remote          With --start: bind Gateway to 0.0.0.0 (LAN-reachable) and
                    advertise the detected LAN IP. Implies --start.
  --skip-embed      Skip building the embedding runtime entirely
  --help, -h        Show this help

Environment:
  ACOWORK_BUILD_PROFILE   debug|release  (overridden by --debug/--release)

Examples:
  ./dev/desktop/macos/build_macos.sh               # Apple Silicon + CoreML, release (recommended)
  ./dev/desktop/macos/build_macos.sh --debug       # Debug build
  ./dev/desktop/macos/build_macos.sh --start       # Build + start Gateway in daemon mode
  ./dev/desktop/macos/build_macos.sh --stop        # Build with explicit stop of existing processes
  ./dev/desktop/macos/build_macos.sh --remote      # Start in LAN-reachable mode (advertise IP)
  ./dev/desktop/macos/build_macos.sh --cpu         # CPU only (Intel Mac or compatibility)
  ./dev/desktop/macos/build_macos.sh --skip-embed  # Skip embed, build only Gateway + Runtime
EOF
            exit 0
            ;;
        *) echo -e "${RED}Unknown option: $arg${NC}"; exit 1 ;;
    esac
done

# ── Validate network-mode flags (MUST run after parsing) ────────────────────
if [ "$NETWORK_MODE" != "default" ] && [ "$STOP_GATEWAY" = "true" ]; then
    echo -e "${RED}ERROR: --local/--remote cannot be combined with --stop (no Gateway is started).${NC}"
    exit 1
fi
if [ "$NETWORK_MODE" = "local" ] && [ "$START_GATEWAY" = "false" ]; then
    echo -e "${YELLOW}WARN: --local has no effect without --start; ignoring.${NC}"
    NETWORK_MODE="default"
fi

# ponytail: best-effort LAN IP detection. The Gateway CLI's --advertise-host
# wins over the auto-detect fallback (which only WARNs on miss). Ceiling:
# VPN-only / multi-NIC / corporate-proxied setups — operator should set
# [advertise_host] in gateway.toml for a deterministic value.
detect_lan_ip() {
    # en0 = default Wi-Fi on Apple hardware; fall back to en1.
    ipconfig getifaddr en0 2>/dev/null || ipconfig getifaddr en1 2>/dev/null || true
}

# Build the gateway bind/advertise args from $NETWORK_MODE (MUST run after
# parsing). The Gateway CLI (core/acowork-gateway/src/cli.rs) accepts:
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

TARGET_DIR="$WORKSPACE_ROOT/target/$PROFILE"

# ── Total step count ──────────────────────────────────────────────────────────
#   --start : 10 (Stop, Gateway, Runtime, Embed, LSP Relay, Node, PM, Doc, User, Copy, Start)
#   else    : 9  (Stop, Gateway, Runtime, Embed, LSP Relay, Node, PM, Doc, User, Copy)
TOTAL_STEPS=9
if [ "$START_GATEWAY" = "true" ]; then
    TOTAL_STEPS=10
fi

# ── Header ──────────────────────────────────────────────────────────────────
echo -e "${CYAN}╔══════════════════════════════════════════════╗${NC}"
echo -e "${CYAN}║   ACowork.AI — macOS One-Click Build Script  ║${NC}"
echo -e "${CYAN}╚══════════════════════════════════════════════╝${NC}"
echo ""
echo -e "${GRAY}  Arch: $ARCH${NC}"
echo -e "${GRAY}  CoreML: $($USE_GPU && echo true || echo false)${NC}"
echo -e "${GRAY}  Profile: $PROFILE${NC}"
if [ "$START_GATEWAY" = "true" ]; then
    echo -e "${GRAY}  Mode:    Build + Start Gateway${NC}"
elif [ "$STOP_GATEWAY" = "true" ]; then
    echo -e "${GRAY}  Mode:    Build + Stop (explicit)${NC}"
else
    echo -e "${GRAY}  Mode:    Build Only${NC}"
fi
echo ""

# ── Step 0: Check required tools ────────────────────────────────────────────
echo -e "${YELLOW}[0/$TOTAL_STEPS] Checking development tools...${NC}"

# Homebrew
if ! command -v brew &>/dev/null; then
    echo -e "${RED}  ✗ Homebrew is not installed${NC}"
    echo -e "${YELLOW}  Install: /bin/bash -c \"\$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"${NC}"
    exit 1
fi
echo -e "${GREEN}  ✓ Homebrew $(brew --version | head -1)${NC}"

# pkg-config
if ! command -v pkg-config &>/dev/null; then
    echo -e "${YELLOW}  ⚠ pkg-config not installed, installing...${NC}"
    brew install pkg-config
fi
echo -e "${GREEN}  ✓ pkg-config $(pkg-config --version)${NC}"

# cmake
if ! command -v cmake &>/dev/null; then
    echo -e "${YELLOW}  ⚠ cmake not installed, installing...${NC}"
    brew install cmake
fi
echo -e "${GREEN}  ✓ cmake $(cmake --version | head -1 | awk '{print $3}')${NC}"

# Rust toolchain
if ! command -v cargo &>/dev/null; then
    echo -e "${RED}  ✗ Rust is not installed${NC}"
    echo -e "${YELLOW}  Install: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh${NC}"
    exit 1
fi
RUST_VER=$(rustc --version | awk '{print $2}')
echo -e "${GREEN}  ✓ Rust $RUST_VER${NC}"

# Check if nightly (project requires)
if ! rustc --version | grep -q nightly; then
    echo -e "${YELLOW}  ⚠ Currently on stable, recommend switching to nightly${NC}"
    echo -e "${GRAY}    rustup default nightly${NC}"
fi

# Node.js
if ! command -v node &>/dev/null; then
    echo -e "${YELLOW}  ⚠ Node.js not installed, recommend using nvm to install 20.x${NC}"
    echo -e "${GRAY}    curl -o- https://raw.githubusercontent.com/nvm-sh/nvm/v0.40.1/install.sh | bash${NC}"
    echo -e "${GRAY}    nvm install 20 && nvm use 20${NC}"
else
    echo -e "${GREEN}  ✓ Node.js $(node --version)${NC}"
fi

# Cargo mirror (mirror for faster access)
if [ ! -f "$HOME/.cargo/config.toml" ]; then
    echo -e "${YELLOW}  ⚠ Configuring Cargo mirror...${NC}"
    mkdir -p "$HOME/.cargo"
    cat > "$HOME/.cargo/config.toml" << 'EOF'
[source.crates-io]
replace-with = "ustc"

[source.ustc]
registry = "sparse+https://mirrors.ustc.edu.cn/crates.io-index/"

[net]
git-fetch-with-cli = true
EOF
    echo -e "${GREEN}  ✓ Cargo mirror configured (USTC)${NC}"
fi

echo ""

# ── Step 1: Stop old processes ──────────────────────────────────────────────
echo -e "${YELLOW}[1/$TOTAL_STEPS] Stopping old processes...${NC}"
# LSP Relay is included because it runs in its own process group (see
# core/acowork-gateway/src/lifecycle/lsp_relay.rs: cmd.process_group(0)),
# so a Gateway shutdown does NOT cascade termination to it — we must kill it
# explicitly to avoid leaving an orphan binding port 19878, which would
# otherwise be attached by the new gateway via attach_existing_lsp_relay()
# but owned by a now-dead parent. Node Agent is included too (ADR-055 §6.11):
# it is spawned by the Gateway, so a killed Gateway can orphan it. PM is a
# standalone process (ADR-064) spawned by the Gateway supervisor; it self-exits
# via the ADR-018 watchdog but the poll can lag, so kill it explicitly. Doc
# mirrors PM (ADR-064): standalone process `acowork-doc` on port 18081, same
# watchdog caveat applies. User likewise mirrors PM/Doc (ADR-084): standalone
# process `acowork-user` on port 18083.
for proc in acowork-gateway acowork-runtime acowork-embed acowork-lsp-relay acowork-node acowork-pm acowork-doc acowork-user acowork-memory; do
    pids=$(pgrep -f "$proc" 2>/dev/null || true)
    if [ -n "$pids" ]; then
        pkill -f "$proc" 2>/dev/null || true
        echo -e "${GRAY}  Stopped $proc: $pids${NC}"
    fi
done
# Free embed port
if command -v fuser &>/dev/null; then
    fuser -k 18080/tcp 2>/dev/null || true
fi
# Free LSP Relay port 19878 (see process_group note above).
if command -v fuser &>/dev/null; then
    fuser -k 19878/tcp 2>/dev/null || true
fi
# Free PM port 18082 (ADR-064 standalone process).
if command -v fuser &>/dev/null; then
    fuser -k 18082/tcp 2>/dev/null || true
fi
# Free Doc port 18081 (ADR-064 standalone process).
if command -v fuser &>/dev/null; then
    fuser -k 18081/tcp 2>/dev/null || true
fi
# Free User port 18083 (ADR-084 standalone process).
if command -v fuser &>/dev/null; then
    fuser -k 18083/tcp 2>/dev/null || true
fi
sleep 1
echo -e "${GREEN}  ✓ Process cleanup complete${NC}"
echo ""

# ── Step 2: Build Gateway ───────────────────────────────────────────────────
echo -e "${YELLOW}[2/$TOTAL_STEPS] Building Gateway ($PROFILE)...${NC}"
cd "$CORE_DIR"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-gateway)
else
    cargo_args=(cargo build -p acowork-gateway)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Gateway compiled successfully${NC}"
else
    echo -e "${RED}  ✗ Gateway compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 3: Build Runtime ───────────────────────────────────────────────────
echo -e "${YELLOW}[3/$TOTAL_STEPS] Building Runtime ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-runtime)
else
    cargo_args=(cargo build -p acowork-runtime)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Runtime compiled successfully${NC}"
else
    echo -e "${RED}  ✗ Runtime compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 4: Build Embed ─────────────────────────────────────────────────────
if [ "$SKIP_EMBED" = "true" ]; then
    echo -e "${YELLOW}[4/$TOTAL_STEPS] Skipping Embed (--skip-embed)${NC}"
    echo ""
else
    echo -e "${YELLOW}[4/$TOTAL_STEPS] Building Embed ($PROFILE, auto-download ONNX Runtime)...${NC}"

    # Determine which feature to use
    EMBED_FEATURES="download-ort"
    if [ "$USE_GPU" = "true" ] && [ "$ARCH" = "arm64" ]; then
        EMBED_FEATURES="download-ort,coreml"
        echo -e "${GRAY}  Using Apple Silicon CoreML acceleration${NC}"
    else
        echo -e "${GRAY}  Using CPU mode${NC}"
    fi

    if [ "$PROFILE" = "release" ]; then
        cargo_args=(cargo build --release -p acowork-embed --features "$EMBED_FEATURES")
    else
        cargo_args=(cargo build -p acowork-embed --features "$EMBED_FEATURES")
    fi
    if "${cargo_args[@]}" 2>&1 | tail -30; then
        echo -e "${GREEN}  ✓ Embed compiled successfully${NC}"
    else
        echo -e "${RED}  ✗ Embed compile failed${NC}"
        exit 1
    fi

    # Step 4.1: Copy downloaded ONNX Runtime to .ort/ (for subsequent scripts)
    echo -e "${YELLOW}  [4.1] Syncing ONNX Runtime to .ort/...${NC}"

    ORT_TARGET_DIR="$WORKSPACE_ROOT/.ort/onnxruntime-osx-aarch64-latest/lib"
    mkdir -p "$ORT_TARGET_DIR"

    # Find ONNX Runtime in Cargo cache
    FOUND_LIB=$(find "$HOME/.cargo/registry/cache" -maxdepth 6 \
        -name "libonnxruntime.dylib" -type f 2>/dev/null | head -1)

    if [ -z "$FOUND_LIB" ]; then
        # Also look for .so or .a
        FOUND_LIB=$(find "$HOME/.cargo/registry/cache" -maxdepth 6 \
            \( -name "libonnxruntime.dylib" -o -name "libonnxruntime.so" \) -type f 2>/dev/null | head -1)
    fi

    if [ -n "$FOUND_LIB" ]; then
        cp "$FOUND_LIB" "$ORT_TARGET_DIR/"
        echo -e "${GREEN}  ✓ Copied to $ORT_TARGET_DIR${NC}"
        echo -e "${GRAY}    Source: $FOUND_LIB${NC}"
    else
        echo -e "${YELLOW}  ⚠ ONNX Runtime not found in Cargo cache, but embed compiled successfully${NC}"
        echo -e "${GRAY}    This is usually fine — cargo statically linked the library into the binary${NC}"
    fi
    echo ""
fi

# ── Step 4.5: Build LSP Relay ───────────────────────────────────────────────
#
# The Gateway spawns `acowork-lsp-relay` as a sibling process (ADR-019). It
# locates the binary via `current_exe().parent().join(...)` — see
# core/acowork-gateway/src/lifecycle/lsp_relay.rs::spawn_lsp_relay. If the
# sibling binary is missing, Gateway startup fails with:
#   GatewayError::Lifecycle("acowork-lsp-relay binary not found at ...")
#
# Unconditional: every Gateway needs an LSP Relay process to serve the
# runtime codebase tool and the desktop Monaco client.
echo -e "${YELLOW}[4.5/$TOTAL_STEPS] Building LSP Relay ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-lsp-relay)
else
    cargo_args=(cargo build -p acowork-lsp-relay)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ LSP Relay compiled successfully${NC}"
else
    echo -e "${RED}  ✗ LSP Relay compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 4.6: Build Node Agent ───────────────────────────────────────────────
#
# ADR-055 §6.11: the Gateway supervises a local Node Agent (`acowork-node`),
# located via `current_exe().parent().join("acowork-node")` — so the binary
# MUST sit next to acowork-gateway. Without it the Gateway silently disables
# the node topology ("acowork-node binary not found — local node agent
# disabled"), node 'local' never enrolls, and agent installs fail with 503
# "Node 'local' has never enrolled (offline)".
echo -e "${YELLOW}[4.6/$TOTAL_STEPS] Building Node Agent ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-node)
else
    cargo_args=(cargo build -p acowork-node)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Node Agent compiled successfully${NC}"
else
    echo -e "${RED}  ✗ Node Agent compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 4.7: Build PM service ───────────────────────────────────────────────
#
# ADR-064: the PM service is a standalone process (`acowork-pm`), located via
# `current_exe().parent().join("acowork-pm")` — so the binary MUST sit next to
# acowork-gateway. Without it the Gateway supervisor logs "acowork-pm binary
# not found" and `/api/pm/*` returns 503 (project management unavailable).
echo -e "${YELLOW}[4.7/$TOTAL_STEPS] Building PM service ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-pm)
else
    cargo_args=(cargo build -p acowork-pm)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ PM service compiled successfully${NC}"
else
    echo -e "${RED}  ✗ PM service compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 4.8: Build Doc service ──────────────────────────────────────────────
#
# Mirrors the PM service above: the Doc service is a standalone process
# (`acowork-doc`), located via `current_exe().parent().join("acowork-doc")` —
# so the binary MUST sit next to acowork-gateway. Without it the Gateway
# supervisor logs "acowork-doc binary not found" and `/api/doc/*` returns 503
# (document library unavailable).
echo -e "${YELLOW}[4.8/$TOTAL_STEPS] Building Doc service ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-doc)
else
    cargo_args=(cargo build -p acowork-doc)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Doc service compiled successfully${NC}"
else
    echo -e "${RED}  ✗ Doc service compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 4.9: Build User service ─────────────────────────────────────────
#
# Mirrors the Doc service above: the User service is a standalone process
# (`acowork-user`, ADR-084), located via `current_exe().parent().join("acowork-user")` —
# so the binary MUST sit next to acowork-gateway. Without it the Gateway
# supervisor logs "acowork-user binary not found" and every user-domain route
# (`/api/auth/*`, `/api/users/*`, `/api/user/*`) returns 503 (accounts,
# profiles and user chat unavailable).
echo -e "${YELLOW}[4.9/$TOTAL_STEPS] Building User service ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-user)
else
    cargo_args=(cargo build -p acowork-user)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ User service compiled successfully${NC}"
else
    echo -e "${RED}  ✗ User service compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 4.10: Build Memory service ────────────────────────────────────────
#
# Mirrors the services above: the Memory service is a standalone process
# (`acowork-memory`, ADR-100), located via
# `current_exe().parent().join("acowork-memory")` — so the binary MUST sit
# next to acowork-gateway. Without it the Gateway supervisor logs
# "acowork-memory binary not found" and the memory panel plus the agent's
# `memory_store` / `memory_recall` tools are gone.
echo -e "${YELLOW}[4.10/$TOTAL_STEPS] Building Memory service ($PROFILE)...${NC}"
if [ "$PROFILE" = "release" ]; then
    cargo_args=(cargo build --release -p acowork-memory)
else
    cargo_args=(cargo build -p acowork-memory)
fi
if "${cargo_args[@]}" 2>&1 | tail -20; then
    echo -e "${GREEN}  ✓ Memory service compiled successfully${NC}"
else
    echo -e "${RED}  ✗ Memory service compile failed${NC}"
    exit 1
fi
echo ""

# ── Step 5: Copy resource files ─────────────────────────────────────────────
#
# The gateway (and embed) read these from `{exe_dir}/`. We only stage into the
# directory matching the active profile — staging to a directory that does not
# yet exist would either fail (`cp` here, which uses `mkdir -p` below) or, in
# the Windows `Copy-Item` equivalent, silently create a stray file.
echo -e "${YELLOW}[5/$TOTAL_STEPS] Copying resource files to $TARGET_DIR...${NC}"
mkdir -p "$TARGET_DIR"
OFFLINE_SRC="$WORKSPACE_ROOT/assets/offline_providers.json"
if [ -f "$OFFLINE_SRC" ]; then
    cp "$OFFLINE_SRC" "$TARGET_DIR/"
    echo -e "${GREEN}  ✓ offline_providers.json${NC}"
fi

# Copy embedding_models.json
EMBEDDING_MODELS_SRC="$CORE_DIR/acowork-embed/assets/embedding_models.json"
if [ -f "$EMBEDDING_MODELS_SRC" ]; then
    cp "$EMBEDDING_MODELS_SRC" "$TARGET_DIR/"
    echo -e "${GREEN}  ✓ embedding_models.json${NC}"
fi

# Copy offline_embedding_providers.json (cloud embedding provider catalog)
# The gateway reads this from `{exe_dir}/offline_embedding_providers.json`.
# Missing file = empty catalog = the UI's cloud-provider section shows no list.
EMBEDDING_PROVIDERS_SRC="$WORKSPACE_ROOT/assets/offline_embedding_providers.json"
if [ -f "$EMBEDDING_PROVIDERS_SRC" ]; then
    cp "$EMBEDDING_PROVIDERS_SRC" "$TARGET_DIR/"
    echo -e "${GREEN}  ✓ offline_embedding_providers.json${NC}"
fi
echo ""

# ── Step 6: Start Gateway (only with --start) ──────────────────────────────
#
# Mirrors build_core.sh's `--start` step. The macOS script does NOT include the
# runtime/embed startup here — Gateway itself owns those child processes via
# its lifecycle manager. We only spawn the gateway binary and let it daemonize
# (ACOWORK_GATEWAY_DAEMON=true makes it detach from the controlling tty).
if [ "$START_GATEWAY" = "true" ]; then
    # Resolve the gateway binary path up-front: both the first-boot setup
    # block below (Step 5.5) and the actual `start` step (Step 6) need it.
    GATEWAY_EXE="$TARGET_DIR/acowork-gateway"

    # Step 5.5: First-boot admin password setup (ADR-076 decision 12 v3).
    #
    # Mirrors dev/common/build_core.sh (Step 4.7) and dev/build_core.ps1:678-783.
    # The Gateway starts in first-boot restricted mode whenever its seeded
    # `admin` account still has the `$disabled$` placeholder hash -- every
    # /api/* returns 403 setup_required until the operator sets a real
    # password via the in-process rpassword prompt or the `admin-setup`
    # subcommand. The in-process prompt is unreachable from this script:
    # the daemon is forked with `> /dev/null 2>&1 &` further down (Step 6),
    # which strips the daemon's TTY, so can_prompt_interactively() returns
    # false and the Gateway silently logs the setup_required warning before
    # daemonising (see core/acowork-gateway/src/cli.rs::can_prompt_interactively
    # + the v3 comment block above warn_first_boot_restricted). Detect that
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
        echo -e "${YELLOW}[5.5/$TOTAL_STEPS] First-boot detected -- admin password has not been set yet.${NC}"
        echo -e "${YELLOW}          Without it, the Gateway would boot in restricted mode (every /api/* returns 403).${NC}"
        echo -e "${YELLOW}          This prompt only appears on a fresh install.${NC}"
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

    echo -e "${YELLOW}[6/$TOTAL_STEPS] Starting Gateway in daemon mode...${NC}"
    log_level="${ACOWORK_GATEWAY_LOG_LEVEL:-info}"
    echo -e "${GRAY}  Log level: $log_level   Network: $NETWORK_MODE${NC}"
    export ACOWORK_GATEWAY_DAEMON="true"

    if [ -f "$GATEWAY_EXE" ]; then
        if [ "${#GATEWAY_ARGS[@]}" -gt 0 ]; then
            echo -e "${GRAY}  Gateway args: ${GATEWAY_ARGS[*]}${NC}"
        fi
        "$GATEWAY_EXE" "${GATEWAY_ARGS[@]}" > /dev/null 2>&1 &
        gateway_pid=$!
        echo -e "${GREEN}  Gateway started (PID: $gateway_pid).${NC}"
    else
        echo -e "${RED}  Gateway executable not found at: $GATEWAY_EXE${NC}"
        exit 1
    fi
    echo ""
fi

# ── Final step: Done ─────────────────────────────────────────────────────────
echo -e "${YELLOW}[$TOTAL_STEPS/$TOTAL_STEPS] Done!${NC}"
echo ""
echo -e "${CYAN}Build artifacts:${NC}"
ls -lh "$TARGET_DIR/acowork-gateway" "$TARGET_DIR/acowork-runtime" "$TARGET_DIR/acowork-embed" "$TARGET_DIR/acowork-lsp-relay" "$TARGET_DIR/acowork-pm" "$TARGET_DIR/acowork-doc" "$TARGET_DIR/acowork-user" "$TARGET_DIR/acowork-memory" 2>/dev/null | awk '{print "  " $9 " (" $5 ")"}'
echo ""

if [ "$START_GATEWAY" = "true" ]; then
    echo -e "${CYAN}Gateway is running in daemon mode.${NC}"
    if [ "$NETWORK_MODE" = "remote" ] && [ -n "$LAN_IP" ]; then
        echo -e "${CYAN}HTTP API: http://${LAN_IP}:19876  (also reachable on the LAN)${NC}"
    else
        echo -e "${CYAN}HTTP API: http://127.0.0.1:19876${NC}"
    fi
    echo ""
fi

echo -e "${CYAN}Next steps:${NC}"
if [ "$START_GATEWAY" = "false" ]; then
    echo -e "  ${GREEN}Start services:${NC}"
    echo -e "    $TARGET_DIR/acowork-gateway &"
    echo -e "    $TARGET_DIR/acowork-runtime &"
    echo -e "    $TARGET_DIR/acowork-embed &"
    echo ""
fi
echo -e "  ${GREEN}Health check:${NC}"
echo -e "    curl http://127.0.0.1:19876/health"
echo ""
echo -e "  ${GREEN}Start Desktop App (browser mode):${NC}"
echo -e "    cd $WORKSPACE_ROOT/apps/acowork-desktop"
echo -e "    npm install"
echo -e "    npm run dev    # → http://localhost:5173"
echo ""
echo -e "  ${GREEN}Start full Tauri Desktop App:${NC}"
echo -e "    cd $WORKSPACE_ROOT/apps/acowork-desktop"
echo -e "    npm install"
echo -e "    npm run tauri dev"
echo ""
