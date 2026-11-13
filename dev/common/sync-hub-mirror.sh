#!/usr/bin/env bash
#
# Open the ACoworkAgentHub mirror PR for a commit of THIS repo.
#
# The platform agents' source of truth is `agents/<name>-agent/` here. The hub
# publishes a byte-for-byte mirror of them under `agents/official/`, produced
# by its own `tools/sync-official.py`. So one agent change is two commits, in
# this order:
#
#   1. this repo  — edit the agent, bump `version` in its manifest.toml,
#                   rebuild its `.agent` (dev/common/build-agent.sh), commit.
#   2. the hub    — THIS SCRIPT. It runs the hub's generator against a
#                   committed ref of this repo and opens the PR it produces.
#
# Step 2 carries no judgement: if it does, the change belonged in step 1. The
# mirror is generated, never hand-edited — the hub's CI fails a hand-edit
# before the next sync silently overwrites it.
#
# Usage:
#   dev/common/sync-hub-mirror.sh                 # mirror the current HEAD
#   dev/common/sync-hub-mirror.sh <ref>           # mirror a specific ref
#   dev/common/sync-hub-mirror.sh --dry-run       # report, change nothing
#
# Environment:
#   ACOWORK_HUB   hub checkout (default: ../ACoworkAgentHub)
#   HUB_BASE      ref the mirror branch is cut from (default: origin/main).
#                 Override it to test against an unmerged tooling branch.
#   NO_PUSH=1     commit on the mirror branch, skip push and the PR
#
# Exit codes:
#   0  in sync, or a mirror PR/branch was produced
#   1  refused (dirty hub, nothing committed, gh failed)
#   2  environment (a repo path is wrong, a tool is missing)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MAIN_REPO="$(git -C "$SCRIPT_DIR/../.." rev-parse --show-toplevel)"
DRY_RUN=0
REF="HEAD"

for arg in "$@"; do
    case "$arg" in
        --dry-run) DRY_RUN=1 ;;
        -h|--help) sed -n '2,32p' "${BASH_SOURCE[0]}"; exit 0 ;;
        -*) echo "ERROR: unknown flag '$arg'" >&2; exit 2 ;;
        *) REF="$arg" ;;
    esac
done

# Probe the interpreter rather than trust `command -v`: on Windows a `python3`
# stub sits on PATH and exits 49 without running anything. The hub's tools
# need tomllib, so 3.11+ is the real requirement.
PY="${PYTHON:-}"
if [ -z "$PY" ]; then
    for c in python3 python py; do
        if "$c" -c 'import sys; sys.exit(0 if sys.version_info >= (3,11) else 1)' 2>/dev/null; then
            PY="$c"; break
        fi
    done
fi
[ -n "$PY" ] || { echo "ERROR: no Python 3.11+ found (the hub's tools need tomllib)" >&2; exit 2; }

SHA="$(git -C "$MAIN_REPO" rev-parse --verify "${REF}^{commit}" 2>/dev/null)" \
    || { echo "ERROR: '$REF' is not a commit in $MAIN_REPO" >&2; exit 1; }
SHORT="${SHA:0:12}"

HUB="${ACOWORK_HUB:-$MAIN_REPO/../ACoworkAgentHub}"
HUB="$(cd "$HUB" 2>/dev/null && pwd)" \
    || { echo "ERROR: hub checkout not found at $ACOWORK_HUB (set ACOWORK_HUB)" >&2; exit 2; }
[ -f "$HUB/tools/sync-official.py" ] \
    || { echo "ERROR: $HUB has no tools/sync-official.py — that is not the hub repo" >&2; exit 2; }

echo "main repo : $MAIN_REPO @ $SHORT"
echo "hub repo  : $HUB"

# The generator rewrites whole package directories. Refuse to run it over a
# hub checkout with uncommitted work in the areas it owns — a sync that
# silently eats someone's in-flight edit is worse than a sync that does not
# run, and the message says exactly what to do about it.
DIRTY="$(git -C "$HUB" status --porcelain --untracked-files=no -- agents/ tools/sync-official.py)"

if [ "$DRY_RUN" = 1 ]; then
    # Read-only: a dirty hub is worth naming, because the differences reported
    # below are measured against the working tree the check reads.
    [ -z "$DIRTY" ] || echo "NOTE: the hub checkout has uncommitted changes; the check reads the working tree"
    echo "--- dry run: what the hub would republish ---"
    "$PY" "$HUB/tools/sync-official.py" --main-repo "$MAIN_REPO" --main-ref "$SHA" --check
    exit $?
fi

# The write path is the one that must not run over someone's work: the
# generator rewrites whole package directories, so an in-flight hub edit
# would be eaten silently.
if [ -n "$DIRTY" ]; then
    echo "ERROR: the hub checkout has uncommitted changes in agents/ or the sync tool." >&2
    echo "       Commit or stash them first: git -C $HUB status --short -- agents/" >&2
    exit 1
fi

BASE="${HUB_BASE:-$(git -C "$HUB" rev-parse --verify origin/main 2>/dev/null || git -C "$HUB" rev-parse HEAD)}"
BRANCH="mirror/sync-$SHORT"
PREV_BRANCH="$(git -C "$HUB" rev-parse --abbrev-ref HEAD)"

# The base ref decides WHICH GENERATOR RUNS, because the PR has to be
# checkable by the hub's own CI and that CI calls this tool. The script needs
# `--main-ref` to pin the claim and `--print-claims` to diff it, so a hub whose
# merged tooling predates them cannot be synced this way at all.
#
# Ask the PARSER, not the help text: the generator on origin/main prints
# `--main-ref` in its docstring (which argparse echoes into --help) and then
# rejects it in the parser, so grepping --help accepts a tool that cannot run.
# Here an accepted flag fails later for some other reason, which is fine —
# only argparse's own refusal means the tool is too old.
TMP_GEN="$(mktemp)"
trap 'rm -f "$TMP_GEN"' EXIT
accepts_flag() {
    local out
    # `--main-repo` is required, and argparse reports a missing required
    # argument before it reports an unrecognized one. Satisfy it, or every old
    # generator looks new.
    out="$("$PY" "$TMP_GEN" --main-repo . "$1" 2>&1 || true)"
    ! grep -q "unrecognized argument.*$1" <<<"$out"
}
for flag in --main-ref --print-claims --check; do
    git -C "$HUB" show "$BASE:tools/sync-official.py" > "$TMP_GEN" 2>/dev/null \
        || { echo "ERROR: $BASE has no tools/sync-official.py" >&2; exit 2; }
    if ! accepts_flag "$flag"; then
        echo "ERROR: the generator at $BASE does not accept $flag." >&2
        echo "       The mirror-sync tooling has not been merged into the hub's main yet." >&2
        echo "       Merge it first, or run the generator by hand on the branch that carries it:" >&2
        echo "         cd $HUB && $PY tools/sync-official.py --main-repo $MAIN_REPO --main-ref $SHA" >&2
        exit 1
    fi
done

git -C "$HUB" switch -c "$BRANCH" "$BASE" >/dev/null
# Anything below that fails must not leave the checkout on a branch nobody
# asked for, with the mirror half-written.
cleanup_branch() {
    git -C "$HUB" switch "$PREV_BRANCH" >/dev/null 2>&1 || true
    git -C "$HUB" reset --hard >/dev/null 2>&1 || true
    git -C "$HUB" branch -D "$BRANCH" >/dev/null 2>&1 || true
}

# The generator is the only writer here: it copies manifest.toml, prompts/,
# skills/ and assets/ verbatim and produces hub.json / README.md / LICENSE
# itself, recording $SHA as the claim.
( cd "$HUB" && "$PY" tools/sync-official.py --main-repo "$MAIN_REPO" --main-ref "$SHA" ) || {
    echo "ERROR: the generator failed; the hub checkout was left as it was." >&2
    cleanup_branch; exit 1; }

git -C "$HUB" add -A agents/
if git -C "$HUB" diff --cached --quiet; then
    git -C "$HUB" switch - >/dev/null 2>&1 || true
    git -C "$HUB" branch -D "$BRANCH" >/dev/null 2>&1 || true
    echo "OK: the hub mirror is already at $SHORT — nothing to publish"
    exit 0
fi

CHANGED="$(git -C "$HUB" diff --cached --name-only | sed -n 's|^agents/[^/]*/\([^/]*\)/.*|\1|p' | sort -u | tr '\n' ' ')"

# Prove the commit satisfies the hub's own gate before pushing it: the mirror
# must match the claim it is about to carry. A failure here means the
# generator produced something the hub would reject, so the branch is torn
# down rather than left half-written in someone's checkout.
if ! "$PY" "$HUB/tools/sync-official.py" --main-repo "$MAIN_REPO" --check >/dev/null; then
    echo "ERROR: the generated mirror does not pass the hub's own --check; branch torn down." >&2
    cleanup_branch
    exit 1
fi

git -C "$HUB" commit -q -F - <<MSG
chore(mirror): sync official agents from ACowork@${SHORT}

Generated by tools/sync-official.py against
${MAIN_REPO}@${SHA}. Packages touched: ${CHANGED%-}.

The main repo's agents/<name>-agent/ is the source of truth for these
bytes; nothing in agents/official/ is authored here. The content change
landed there first, and this commit only carries it to the registry, so
the published .agent files match what the app ships under the same
agent_id and an installed package stays upgradeable.
MSG

echo "OK: committed $(git -C "$HUB" rev-parse --short HEAD) on $BRANCH"

if [ "${NO_PUSH:-0}" = 1 ]; then
    echo "NO_PUSH=1 — branch left local. Next:"
    echo "  git -C $HUB push -u origin $BRANCH"
    echo "  gh pr create -R acowork/ACoworkAgentHub -B main -t 'mirror: sync official agents from ACowork@${SHORT}'"
    exit 0
fi

command -v gh >/dev/null 2>&1 || {
    echo "gh not found — branch $BRANCH is committed locally. Push and open the PR by hand." >&2
    exit 1
}

git -C "$HUB" push -q -u origin "$BRANCH"
gh pr create -R acowork/ACoworkAgentHub -B main -H "$BRANCH" \
    --title "mirror: sync official agents from ACowork@${SHORT}" \
    --body "**Generated — do not hand-edit.** Source: \`acowork/ACowork@${SHA}\`.

Packages: ${CHANGED%-}

Produced by \`tools/sync-official.py --main-repo ../ACowork --main-ref ${SHA}\`.
\`hub.json\` records the claim; \`--check\` verifies the mirror against it.

- [ ] \`Check the mirror matches the commit it claims\` is green"
echo "OK: PR opened against acowork/ACoworkAgentHub main"
