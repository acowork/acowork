#!/usr/bin/env bash
# Build an unsigned .app package from an application source directory
# (design doc 34 §6.12). Does NOT sign — keys stay out of CI.
#
# Usage:
#   dev/common/build-app.sh <app-src-dir> --version 1.0.1 --platform windows-x86_64
#   [--output ./build/app-1.0.1.unsigned.app]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <app-src-dir> --version <ver> --platform <os-arch> [--output <path>]" >&2
  exit 2
fi

APP_DIR="$(cd "$1" && pwd)"
shift

VERSION=""
PLATFORM=""
OUTPUT=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --version) VERSION="$2"; shift 2 ;;
    --platform) PLATFORM="$2"; shift 2 ;;
    --output) OUTPUT="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ -z "$VERSION" || -z "$PLATFORM" ]]; then
  echo "--version and --platform are required" >&2
  exit 2
fi

MANIFEST="$APP_DIR/app.toml"
if [[ ! -f "$MANIFEST" ]]; then
  echo "missing app.toml in $APP_DIR" >&2
  exit 1
fi

# Ensure [package].platforms mentions the requested platform (soft check via grep).
if ! grep -q "$PLATFORM" "$MANIFEST"; then
  echo "warning: platform '$PLATFORM' not found in app.toml platforms list" >&2
fi

APP_ID="$(grep -E '^id\s*=' "$MANIFEST" | head -1 | sed -E 's/.*=\s*"?([^"]+)"?.*/\1/' | tr -d ' ')"
if [[ -z "$APP_ID" ]]; then
  echo "could not parse id from app.toml" >&2
  exit 1
fi

if [[ -z "$OUTPUT" ]]; then
  mkdir -p "$REPO_ROOT/build"
  OUTPUT="$REPO_ROOT/build/${APP_ID}-${VERSION}.unsigned.app"
fi

# Prefer a built acowork-pack if present; otherwise cargo-run the packager helper.
PACK_BIN="$REPO_ROOT/core/target/debug/acowork-pack-app"
if [[ ! -x "$PACK_BIN" ]]; then
  PACK_BIN="$REPO_ROOT/core/target/release/acowork-pack-app"
fi

if [[ -x "$PACK_BIN" ]]; then
  "$PACK_BIN" --app-dir "$APP_DIR" --output "$OUTPUT"
else
  (cd "$REPO_ROOT/core" && cargo run -p acowork-sign --bin acowork-pack-app --quiet -- \
      --app-dir "$APP_DIR" --output "$OUTPUT")
fi

echo "wrote unsigned package: $OUTPUT"
