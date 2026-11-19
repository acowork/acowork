#!/usr/bin/env bash
# Pack the doc-clone fixture into an unsigned .app for ADR-116 install smoke.
# Usage (repo root):
#   dev/common/pack-doc-clone.sh [--output PATH]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FIXTURE="$REPO_ROOT/dev/fixtures/doc-clone-app"
STAGING="$(mktemp -d "${TMPDIR:-/tmp}/doc-clone-pack.XXXXXX")"
OUTPUT=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --output) OUTPUT="$2"; shift 2 ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
done

cleanup() { rm -rf "$STAGING"; }
trap cleanup EXIT

if [[ -z "$OUTPUT" ]]; then
  mkdir -p "$REPO_ROOT/build"
  OUTPUT="$REPO_ROOT/build/doc-clone-1.0.0.unsigned.app"
fi

# Locate acowork-doc binary (debug preferred for local smoke).
find_doc_bin() {
  local name="acowork-doc"
  if [[ "${OS:-}" == "Windows_NT" ]] || uname -s 2>/dev/null | grep -Eiq 'mingw|msys|cygwin'; then
    name="acowork-doc.exe"
  fi
  local candidates=(
    "$REPO_ROOT/core/target/debug/$name"
    "$REPO_ROOT/core/target/release/$name"
    "$REPO_ROOT/target/debug/$name"
    "$REPO_ROOT/target/release/$name"
  )
  # Also look next to a running Gateway if present on PATH's dirname.
  if command -v acowork-gateway >/dev/null 2>&1; then
    local gw
    gw="$(command -v acowork-gateway)"
    candidates+=("$(dirname "$gw")/$name")
  fi
  for c in "${candidates[@]}"; do
    if [[ -f "$c" ]]; then
      echo "$c"
      return 0
    fi
  done
  return 1
}

DOC_BIN="$(find_doc_bin)" || {
  echo "acowork-doc binary not found; build it first:" >&2
  echo "  cd core && cargo build -p acowork-doc" >&2
  exit 1
}

BIN_NAME="$(basename "$DOC_BIN")"
mkdir -p "$STAGING/bin"
cp "$DOC_BIN" "$STAGING/bin/$BIN_NAME"
# Preserve a non-.exe twin name so Unix entry declarations resolve on Windows packs.
if [[ "$BIN_NAME" == *.exe ]]; then
  cp "$DOC_BIN" "$STAGING/bin/${BIN_NAME%.exe}" 2>/dev/null || true
fi

cp "$FIXTURE/app.toml" "$STAGING/app.toml"
# Pin package.entry to the actual binary name we just copied.
if command -v python >/dev/null 2>&1; then
  python - "$STAGING/app.toml" "bin/$BIN_NAME" <<'PY'
import pathlib, re, sys
path = pathlib.Path(sys.argv[1])
entry = sys.argv[2]
text = path.read_text(encoding="utf-8")
text2, n = re.subn(r'(?m)^entry\s*=\s*".*"\s*$', f'entry = "{entry}"', text, count=1)
if n != 1:
    raise SystemExit("failed to rewrite package.entry")
path.write_text(text2, encoding="utf-8")
PY
fi

PACK_BIN=""
for c in \
  "$REPO_ROOT/core/target/debug/acowork-pack-app" \
  "$REPO_ROOT/core/target/debug/acowork-pack-app.exe" \
  "$REPO_ROOT/core/target/release/acowork-pack-app" \
  "$REPO_ROOT/core/target/release/acowork-pack-app.exe"
do
  if [[ -x "$c" || -f "$c" ]]; then PACK_BIN="$c"; break; fi
done

if [[ -n "$PACK_BIN" ]]; then
  "$PACK_BIN" --app-dir "$STAGING" --output "$OUTPUT"
else
  (cd "$REPO_ROOT/core" && cargo run -p acowork-sign --bin acowork-pack-app --quiet -- \
      --app-dir "$STAGING" --output "$OUTPUT")
fi

echo "wrote $OUTPUT"
echo "source binary: $DOC_BIN"
