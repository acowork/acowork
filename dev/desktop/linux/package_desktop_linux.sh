#!/usr/bin/env bash
# package_desktop_linux.sh - Build ACowork Desktop package for Linux

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(dirname "$(dirname "$(dirname "$SCRIPT_DIR")")")"
DESKTOP_DIR="$WORKSPACE_ROOT/apps/acowork-desktop"
BIN_DIR="$DESKTOP_DIR/src-tauri/bin"

if [ "$(uname -s)" != "Linux" ]; then
    echo -e "${RED}This script is for Linux only.${NC}"
    exit 1
fi

echo -e "${CYAN}========================================${NC}"
echo -e "${CYAN}ACowork Desktop Package (Linux)${NC}"
echo -e "${CYAN}========================================${NC}"
echo ""

find_ort_lib() {
    local found=""
    for ort_dir in "$WORKSPACE_ROOT"/.ort/onnxruntime-linux-*; do
        [ -d "$ort_dir" ] || continue
        if [ -f "$ort_dir/lib/libonnxruntime.so" ]; then
            found="$ort_dir/lib/libonnxruntime.so"
            break
        fi
    done
    echo "$found"
}

ORT_LIB="$(find_ort_lib)"
if [ -z "$ORT_LIB" ]; then
    "$SCRIPT_DIR/../../common/setup_ort.sh"
    ORT_LIB="$(find_ort_lib)"
fi

if [ -z "$ORT_LIB" ]; then
    echo -e "${RED}ONNX Runtime library not found. Run ./dev/common/setup_ort.sh first.${NC}"
    exit 1
fi

export ORT_LIB_LOCATION="$(dirname "$ORT_LIB")"
export ORT_DYLIB_PATH="$ORT_LIB"
export ORT_PREFER_DYNAMIC_LINK=1
export LD_LIBRARY_PATH="$ORT_LIB_LOCATION:${LD_LIBRARY_PATH:-}"

mkdir -p "$BIN_DIR"
cp "$ORT_LIB" "$BIN_DIR/libonnxruntime.so"
echo -e "${GREEN}Bundled ORT library: $ORT_LIB${NC}"

# Bundle LSP Relay binary (sibling of acowork-gateway, ADR-019).
# The Gateway locates it via `current_exe().parent().join("acowork-lsp-relay")`,
# so without this copy the Tauri app's Gateway supervisor will fail to spawn LSP
# and Monaco / runtime codebase tool will silently lose all LSP features.
LSP_RELAY_BIN="$WORKSPACE_ROOT/target/release/acowork-lsp-relay"
if [ -f "$LSP_RELAY_BIN" ]; then
    cp "$LSP_RELAY_BIN" "$BIN_DIR/acowork-lsp-relay"
    echo -e "${GREEN}Bundled LSP Relay binary: $LSP_RELAY_BIN${NC}"
else
    echo -e "${YELLOW}WARN: acowork-lsp-relay not found at $LSP_RELAY_BIN.${NC}"
    echo -e "${YELLOW}      Run ./dev/common/build_core.sh (release) first.${NC}"
    echo -e "${YELLOW}      Without it, Gateway startup will fail with:${NC}"
    echo -e "${YELLOW}        acowork-lsp-relay binary not found${NC}"
fi

# Bundle Node Agent binary (sibling of acowork-gateway, ADR-055 §6.11).
# The Gateway locates it via `current_exe().parent().join("acowork-node")`;
# without this copy node 'local' never enrolls and agent installs fail with
# 503 "Node 'local' has never enrolled (offline)".
NODE_BIN="$WORKSPACE_ROOT/target/release/acowork-node"
if [ -f "$NODE_BIN" ]; then
    cp "$NODE_BIN" "$BIN_DIR/acowork-node"
    echo -e "${GREEN}Bundled Node Agent binary: $NODE_BIN${NC}"
else
    echo -e "${YELLOW}WARN: acowork-node not found at $NODE_BIN.${NC}"
    echo -e "${YELLOW}      Run ./dev/common/build_core.sh (release) first.${NC}"
    echo -e "${YELLOW}      Without it, Gateway startup will fail with:${NC}"
    echo -e "${YELLOW}        acowork-node binary not found — node topology disabled${NC}"
fi

# Bundle PM service binary (sibling of acowork-gateway, ADR-064).
# The Gateway supervisor locates it via `current_exe().parent().join("acowork-pm")`;
# without this copy the PM supervisor logs "acowork-pm binary not found" and
# `/api/pm/*` returns 503 (project management unavailable).
PM_BIN="$WORKSPACE_ROOT/target/release/acowork-pm"
if [ -f "$PM_BIN" ]; then
    cp "$PM_BIN" "$BIN_DIR/acowork-pm"
    echo -e "${GREEN}Bundled PM service binary: $PM_BIN${NC}"
else
    echo -e "${YELLOW}WARN: acowork-pm not found at $PM_BIN.${NC}"
    echo -e "${YELLOW}      Run ./dev/common/build_core.sh (release) first.${NC}"
    echo -e "${YELLOW}      Without it, /api/pm/* returns 503 (PM unavailable).${NC}"
fi

# Bundle Doc service binary (sibling of acowork-gateway, ADR-064).
# Mirrors the PM service above: the Gateway supervisor locates it via
# `current_exe().parent().join("acowork-doc")`; without this copy the Doc
# supervisor logs "acowork-doc binary not found" and `/api/doc/*` returns 503
# (document library unavailable).
DOC_BIN="$WORKSPACE_ROOT/target/release/acowork-doc"
if [ -f "$DOC_BIN" ]; then
    cp "$DOC_BIN" "$BIN_DIR/acowork-doc"
    echo -e "${GREEN}Bundled Doc service binary: $DOC_BIN${NC}"
else
    echo -e "${YELLOW}WARN: acowork-doc not found at $DOC_BIN.${NC}"
    echo -e "${YELLOW}      Run ./dev/common/build_core.sh (release) first.${NC}"
    echo -e "${YELLOW}      Without it, /api/doc/* returns 503 (Doc unavailable).${NC}"
fi

# Bundle User service binary (sibling of acowork-gateway, ADR-084).
# Mirrors the Doc service above: the Gateway supervisor locates it via
# `current_exe().parent().join("acowork-user")`; without this copy the User
# supervisor logs "acowork-user binary not found" and every user-domain route
# (`/api/auth/*`, `/api/users/*`, `/api/user/*`) returns 503 (accounts,
# profiles and user chat unavailable).
USER_BIN="$WORKSPACE_ROOT/target/release/acowork-user"
if [ -f "$USER_BIN" ]; then
    cp "$USER_BIN" "$BIN_DIR/acowork-user"
    echo -e "${GREEN}Bundled User service binary: $USER_BIN${NC}"
else
    echo -e "${YELLOW}WARN: acowork-user not found at $USER_BIN.${NC}"
    echo -e "${YELLOW}      Run ./dev/common/build_core.sh (release) first.${NC}"
    echo -e "${YELLOW}      Without it, /api/auth/*, /api/users/* and /api/user/* return 503.${NC}"
fi

# Bundle Memory service binary (sibling of acowork-gateway, ADR-101).
# Mirrors the User service above: the Gateway supervisor locates it via
# `current_exe().parent().join("acowork-memory")`; without this copy the
# memory supervisor logs "acowork-memory binary not found" and the memory
# panel plus the agent's `memory_store` / `memory_recall` tools are gone
# (`/api/memory/*` and `/api/agents/{id}/memory/*` return 503).
MEMORY_BIN="$WORKSPACE_ROOT/target/release/acowork-memory"
if [ -f "$MEMORY_BIN" ]; then
    cp "$MEMORY_BIN" "$BIN_DIR/acowork-memory"
    echo -e "${GREEN}Bundled Memory service binary: $MEMORY_BIN${NC}"
else
    echo -e "${YELLOW}WARN: acowork-memory not found at $MEMORY_BIN.${NC}"
    echo -e "${YELLOW}      Run ./dev/common/build_core.sh (release) first.${NC}"
    echo -e "${YELLOW}      Without it, /api/memory/* returns 503 (memory unavailable).${NC}"
fi

cd "$DESKTOP_DIR"
npm run tauri build

