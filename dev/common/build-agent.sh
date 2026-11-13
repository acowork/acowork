#!/usr/bin/env bash
#
# ACowork Agent Package Builder and Signer
# Usage: ./build-agent.sh <agent-dir> [output-dir]
#
# This script:
# 1. Creates a .agent ZIP package from the agent directory
# 2. Generates signing keys (if not exist)
# 3. Signs the package
# 4. Outputs to the specified directory

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$(dirname "$SCRIPT_DIR")")"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Default values
OUTPUT_DIR="${PROJECT_ROOT}/agents/agent-packages"
KEY_DIR="${PROJECT_ROOT}/agents/.signing-keys"

# Parse arguments
if [ $# -lt 1 ]; then
    echo -e "${RED}Error: Agent directory path is required${NC}"
    echo "Usage: $0 <agent-dir> [output-dir]"
    exit 1
fi

AGENT_DIR="$1"
if [ $# -ge 2 ]; then
    OUTPUT_DIR="$2"
fi

# Validate agent directory
if [ ! -d "$AGENT_DIR" ]; then
    echo -e "${RED}Error: Agent directory does not exist: ${AGENT_DIR}${NC}"
    exit 1
fi

if [ ! -f "${AGENT_DIR}/manifest.toml" ]; then
    echo -e "${RED}Error: manifest.toml not found in ${AGENT_DIR}${NC}"
    exit 1
fi

# Step 1 shells out to `zip`. Git Bash on Windows does not ship it, and without
# this check the failure lands after the cargo build, where it reads like a
# compile problem. On Windows use dev/common/build-agent.ps1 instead.
if ! command -v zip >/dev/null 2>&1; then
    echo -e "${RED}Error: 'zip' is not on PATH — this builder needs it (Git Bash on Windows does not ship it).${NC}"
    echo -e "${YELLOW}Use the PowerShell builder instead: dev/common/build-agent.ps1 -AgentDir ${AGENT_DIR}${NC}"
    exit 1
fi

# Read agent ID from manifest
AGENT_ID=$(grep '^agent_id' "${AGENT_DIR}/manifest.toml" | head -1 | sed 's/.*= *"\(.*\)".*/\1/' | tr -d '"')
AGENT_VERSION=$(grep '^version' "${AGENT_DIR}/manifest.toml" | head -1 | sed 's/.*= *"\(.*\)".*/\1/' | tr -d '"')

if [ -z "$AGENT_ID" ]; then
    echo -e "${RED}Error: Could not read agent id from manifest.toml${NC}"
    exit 1
fi

echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}ACowork Agent Package Builder${NC}"
echo -e "${GREEN}========================================${NC}"
echo -e "Agent ID:      ${YELLOW}${AGENT_ID}${NC}"
echo -e "Agent Version: ${YELLOW}${AGENT_VERSION}${NC}"
echo -e "Output Dir:    ${YELLOW}${OUTPUT_DIR}${NC}"
echo -e "Key Dir:       ${YELLOW}${KEY_DIR}${NC}"
echo ""

# Create output directory
mkdir -p "$OUTPUT_DIR"

# Convert to absolute paths
OUTPUT_DIR="$(cd "$OUTPUT_DIR" && pwd)"

# Step 1: Create unsigned .agent package (ZIP)
UNSIGNED_PKG="${OUTPUT_DIR}/${AGENT_ID}-${AGENT_VERSION}.unsigned.agent"
SIGNED_PKG="${OUTPUT_DIR}/${AGENT_ID}.agent"

echo -e "${GREEN}[1/4]${NC} Creating unsigned package..."
cd "$AGENT_DIR"
zip -r "$UNSIGNED_PKG" manifest.toml prompts/ skills/ assets/ -x "*.DS_Store" -x "*/__MACOSX/*"
echo -e "      Created: ${UNSIGNED_PKG}"
echo ""

# Step 2: Generate signing keys if not exist
if [ ! -d "$KEY_DIR" ] || [ ! -f "${KEY_DIR}/developer.key" ]; then
    echo -e "${GREEN}[2/4]${NC} Generating signing keys..."
    mkdir -p "$KEY_DIR"
    
    # Build and run keygen
    cd "$PROJECT_ROOT/core"
    cargo run --release --bin acowork-keygen -- --type developer --output-dir "$KEY_DIR" 2>&1 | grep -v "Compiling\|Finished\|Running\|warning"
    
    echo -e "      Keys generated in: ${KEY_DIR}"
else
    echo -e "${GREEN}[2/4]${NC} Signing keys already exist"
fi
echo ""

# Step 3: Sign the package
echo -e "${GREEN}[3/4]${NC} Signing package..."
cd "$PROJECT_ROOT/core"
cargo run --release --bin acowork-sign -- \
    --input "$UNSIGNED_PKG" \
    --key "$KEY_DIR" \
    --output "$SIGNED_PKG" \
    --key-type developer 2>&1 | grep -v "Compiling\|Finished\|Running\|warning"

echo -e "      Signed: ${SIGNED_PKG}"
echo ""

# Step 4: Verify the signature
echo -e "${GREEN}[4/4]${NC} Verifying signature..."
cd "$PROJECT_ROOT/core"
VERIFY_OUTPUT=$(cargo run --release --bin acowork-verify -- "$SIGNED_PKG" 2>&1 | grep -v "Compiling\|Finished\|Running\|warning")
echo -e "      ${VERIFY_OUTPUT}"
echo ""

# Cleanup unsigned package
rm -f "$UNSIGNED_PKG"

# Summary
echo -e "${GREEN}========================================${NC}"
echo -e "${GREEN}Build Complete!${NC}"
echo -e "${GREEN}========================================${NC}"
echo -e "Package: ${YELLOW}${SIGNED_PKG}${NC}"
echo -e "Size:    ${YELLOW}$(du -h "$SIGNED_PKG" | cut -f1)${NC}"
echo -e ""

# Show package contents
echo -e "${GREEN}Package Contents:${NC}"
unzip -l "$SIGNED_PKG" | tail -n +4 | head -n -2
echo ""

echo -e "${GREEN}Done!${NC}"
