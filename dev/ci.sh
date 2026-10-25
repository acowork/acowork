#!/bin/bash
# CI script for ACowork.AI
# Usage: ./dev/ci.sh [check|clippy|test|integration|smoke|all]

set -e

MODE=${1:-all}

# All cargo commands run against the core workspace; the red-line check
# below uses paths relative to the workspace root.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/../core"

echo "=== ACowork.AI CI ==="

run_check() {
    echo "Running cargo check..."
    cargo check --all
}

# ADR-055 §6.20 dependency red line: acowork-node MUST NOT depend on
# acowork-gateway (mirrors acowork-node/tests/dependency_redline.rs).
run_node_redline() {
    echo "Checking acowork-node dependency red line..."
    if grep -qE '^[[:space:]]*acowork-gateway[[:space:]]*=' acowork-node/Cargo.toml; then
        echo "ERROR: acowork-node depends on acowork-gateway (ADR-055 §6.20 red line violated)"
        exit 1
    fi
    echo "acowork-node dependency red line: OK"
}

# ADR-065 §7 #1 red line: ErrorKind::MqttState MUST NOT appear outside
# acowork-mqtt-session. Reintroducing this literal in any consumer
# crate (Node / Gateway / Desktop / Runtime) recreates the original
# wake-60s bug — the local adapter fails to unwrap the inner
# StateError::Io and re-classifies ECONNRESET as fatal E4 ConfigError.
run_mqtt_redline() {
    echo "Checking MQTT ErrorKind::MqttState red line (ADR-065 §7 #1)..."
    # Scan only Rust source. Skip the shared crate (single source of truth)
    # and the mqtt-session Cargo target directory if present.
    local offenders
    offenders=$(cd "$SCRIPT_DIR/.." && grep -rnE 'ErrorKind::MqttState' \
        --include='*.rs' \
        apps/ core/ \
        | grep -vE '^(apps|core)/acowork-mqtt-session/' \
        | grep -vE '/target/' \
        || true)
    if [ -n "$offenders" ]; then
        echo "ERROR: ErrorKind::MqttState literal found outside acowork-mqtt-session (ADR-065 §7 #1):"
        echo "$offenders"
        exit 1
    fi
    echo "MQTT ErrorKind red line: OK"
}

# ADR-009 §5.4 red line: Gateway = communication + resource management +
# reverse proxy. It MUST NOT turn an installed agent's `install_path` into a
# filesystem root. Since ADR-055 that path is a *node-local* path, so such
# access silently works on a single-machine setup and breaks (5xx) the moment
# Gateway and Node are on different machines — that is the V-A/V-B bug class:
# a second parser in the Gateway reading a directory that is not there.
#
# Ceiling, not allowlist. These are the surviving legitimate call sites
# (ADR-009 §2.2 install-time package management, the Gateway-owned avatar
# cache and its publish flow, reporting `{install_path}/workspace` back to the
# Runtime). Counts may only go DOWN: the moment one goes up, or a hit appears
# in a file that is not listed here, the boundary has been re-crossed.
# Lower a number in the same commit that removes a call site.
ADR009_FS_CEILING="
gateway/mod.rs:2
http/agents.rs:4
mqtt/dispatch.rs:3
"
run_gateway_fs_redline() {
    echo "Checking ADR-009 Gateway filesystem-isolation red line..."
    local root="$SCRIPT_DIR/../core/acowork-gateway/src"
    local hits
    hits=$(grep -rnE 'install_path' --include='*.rs' "$root" \
        | grep -E 'Path::new|PathBuf::from|\.join\(|fs::read|fs::write|canonicalize|\.exists\(' \
        | grep -vE 'install_path: ' \
        | grep -vE ':[0-9]+:[[:space:]]*//' \
        || true)

    local failed=0 rel count ceiling files f
    files=$(printf '%s\n' "$hits" | grep -oE "^$root/[^:]+" | sort -u || true)
    while IFS= read -r f; do
        [ -z "$f" ] && continue
        rel="${f#"$root"/}"
        count=$(printf '%s\n' "$hits" | grep -cF "$f:" || true)
        ceiling=$(printf '%s\n' "$ADR009_FS_CEILING" | grep -E "^$rel:" | cut -d: -f2 || true)
        if [ -z "$ceiling" ]; then
            echo "ERROR: ${rel}: ${count} install_path filesystem access(es) — not on the ADR-009 §5.4 allowlist."
            failed=1
        elif [ "$count" -gt "$ceiling" ]; then
            echo "ERROR: ${rel}: ${count} install_path filesystem access(es), ceiling is ${ceiling}."
            failed=1
        fi
    done <<< "$files"

    # ADR-084 §决策 1 (appended to this red line by the migration plan):
    # the user domain's data — `accounts.json`, the derived profile view, the
    # `users/` chat trees, avatars — belongs to `acowork-user`. The Gateway
    # reaches it only through `http/user_proxy.rs` and the snapshot pull; a
    # direct path reference here is the same bug class as an `install_path`
    # read (silently right on one machine, wrong the moment data_dir moves).
    local user_hits
    user_hits=$(grep -rnE '(accounts\.json|user_profiles\.json|assets/avatars|\.join\("users"\)|\.join\("chats"\)|\.join\("avatars"\))' \
        --include='*.rs' "$root" \
        | grep -vE ':[0-9]+:[[:space:]]*//' \
        || true)
    if [ -n "$user_hits" ]; then
        echo "ERROR: user-domain data path referenced in Gateway source (ADR-084 §决策 1):"
        echo "$user_hits"
        failed=1
    fi

    if [ "$failed" -ne 0 ]; then
        echo ""
        echo "Agent-private data must be proxied through the Runtime, not read by the Gateway:"
        echo "  core/acowork-gateway/src/http/proxy.rs  ->  core/acowork-runtime/src/http/"
        echo "User-domain data must be proxied through acowork-user, not read by the Gateway:"
        echo "  core/acowork-gateway/src/http/user_proxy.rs  ->  core/acowork-user/"
        exit 1
    fi
    echo "Gateway filesystem red line: OK"
}

# ADR-024 / session-listing cache: the on-disk meta layout
# (`conversations/meta/{session_id}.json`) has exactly one owner —
# core/acowork-runtime/src/conversation.rs — because session listings are
# served from an in-memory cache kept in sync by `write_session_meta` and
# `remove_session_meta`. A new place that builds or mutates a meta path can
# silently desync that cache: list a session whose file is gone, or hide one
# that exists.
#
# Ceiling, not allowlist-of-truth. These are the surviving legitimate call
# sites: test fixtures that need a meta file whose fields the real writer
# would refuse to emit. Counts may only go DOWN — lower a number in the same
# commit that removes a call site, and add nothing new.
META_LAYOUT_CEILING="
acowork-runtime/src/agent/session/session_manager.rs:1
acowork-runtime/src/http/server.rs:3
acowork-runtime/tests/conversation_session_tokens.rs:3
# Negative assertion only: the SQLite e2e test asserts the legacy JSON meta
# directory was never created, i.e. it checks the *absence* of the layout this
# lint protects. Pre-existing, unrelated to ADR-084.
acowork-runtime/tests/session_meta_sqlite_e2e.rs:1
"
run_meta_layout_redline() {
    echo "Checking session meta layout-ownership red line (ADR-024)..."
    local root="$SCRIPT_DIR/../core"
    local hits
    hits=$(grep -rnE 'join\("meta"\)|META_DIR' --include='*.rs' "$root" \
        | grep -vE '/target/' \
        | grep -vE "^$root/acowork-runtime/src/conversation\.rs:" \
        || true)

    local failed=0 rel count ceiling files f
    files=$(printf '%s\n' "$hits" | grep -oE "^$root/[^:]+" | sort -u || true)
    while IFS= read -r f; do
        [ -z "$f" ] && continue
        rel="${f#"$root"/}"
        count=$(printf '%s\n' "$hits" | grep -cF "$f:" || true)
        ceiling=$(printf '%s\n' "$META_LAYOUT_CEILING" | grep -E "^$rel:" | cut -d: -f2 || true)
        if [ -z "$ceiling" ]; then
            echo "ERROR: ${rel}: ${count} session-meta layout access(es) — not on the ADR-024 allowlist."
            failed=1
        elif [ "$count" -gt "$ceiling" ]; then
            echo "ERROR: ${rel}: ${count} session-meta layout access(es), ceiling is ${ceiling}."
            failed=1
        fi
    done <<< "$files"

    if [ "$failed" -ne 0 ]; then
        echo ""
        echo "Meta files must be written and removed through conversation.rs, or the"
        echo "cached session listing goes stale:"
        echo "  core/acowork-runtime/src/conversation.rs  ->  write_session_meta / remove_session_meta"
        exit 1
    fi
    echo "Session meta layout red line: OK"
}

# ADR-096 red line: no request path / tool read may take a full read of a
# session log (`messages.jsonl` / `{sid}.jsonl`) — not a `read_to_string` of
# the whole file, nor a per-line `count_jsonl_lines` scan to derive a row
# count, seq, idempotency id or compaction summary. Every such statistic is
# served from write-time derived state (session meta, the in-memory log index,
# the `conversation_state` table). A full pass is legitimate only inside a
# once-per-process first-touch reconcile / open-calibration / migration /
# startup-backfill function — never per request, never per append.
#
# The two log-owner files are scanned. `awk` tracks the enclosing `fn` and the
# test module, and fails on a `read_to_string` / `count_jsonl_lines` in any
# other function body. The whitelist is the set of first-touch / migration /
# sidecar / meta loaders that legitimately read a whole (small) file once;
# shrinking it is fine, growing it on a request path is the violation.
SESSION_FULL_READ_WHITELIST='^(reconcile|calibrate|migrate|scan_all_sessions|load_sidecar|rebuild|backfill|read_index|read_participants_index|read_projects_index|load)'
run_session_full_read_redline() {
    echo "Checking session full-read red line (ADR-096)..."
    local root="$SCRIPT_DIR/../core"
    local failed=0
    local files=(
        "acowork-user/src/chat_sessions.rs"
        "acowork-runtime/src/conversation.rs"
    )
    local rel offenders
    for rel in "${files[@]}"; do
        offenders=$(awk -v whitelist="$SESSION_FULL_READ_WHITELIST" -v rel="$rel" '
            { iscomment = ($0 ~ /^[[:space:]]*(\/\/|\/\/\/)/) }
            /#\[cfg\(test\)\]/ || /^[[:space:]]*mod tests/ { intest = 1 }
            !iscomment && match($0, /fn[[:space:]]+[A-Za-z0-9_]+/) {
                fn = substr($0, RSTART, RLENGTH)
                sub(/^fn[[:space:]]+/, "", fn)
            }
            !intest && !iscomment && (/read_to_string/ || /count_jsonl_lines/) {
                if (fn !~ whitelist) print rel ":" NR ": " $0
            }
        ' "$root/$rel" || true)
        if [ -n "$offenders" ]; then
            echo "ERROR: full session-log read outside a first-touch/migration function ($rel):"
            echo "$offenders"
            failed=1
        fi
    done
    if [ "$failed" -ne 0 ]; then
        echo ""
        echo "Request paths must read windowed ranges from derived state"
        echo "(session meta total, the in-memory log index, conversation_state) —"
        echo "never a full read_to_string / count_jsonl_lines of the log. Only"
        echo "once-per-process reconcile / calibrate / migrate / scan functions"
        echo "may scan the whole file (ADR-096)."
        exit 1
    fi
    echo "Session full-read red line: OK"
}

# ADR-076 §决策 3/4 (+ADR-084 §决策 7): identity injection has exactly two
# trusted writers, both deriving the identity from a verified token —
# `auth_middleware` (`x-user-id`, consumed by the Runtime) and `user_proxy`
# (`X-Auth-*`, consumed by the user service). Inbound headers are forwarded
# verbatim, so any other write of either family is a client-asserted identity
# that could claim another user's sessions or admin role. Both the constants
# and the raw literals may only appear in those two files (definition sites
# and their tests) — matching the literals closes the hole where a new writer
# spells the header out as a string and bypasses the lint.
run_gateway_auth_scope_redline() {
    echo "Checking ADR-076 identity-injection red line..."
    local root="$SCRIPT_DIR/../core/acowork-gateway/src"
    local offenders
    offenders=$(grep -rnE '(USER_SCOPE_HEADER|"x-user-id"|AUTH_USER_HEADER|AUTH_ROLE_HEADER|AUTH_AS_USER_HEADER|"x-auth-)' --include='*.rs' "$root" \
        | grep -vE "^$root/http/auth_middleware\.rs:" \
        | grep -vE "^$root/http/user_proxy\.rs:" \
        || true)
    if [ -n "$offenders" ]; then
        echo "ERROR: identity header (constant or literal) referenced outside auth_middleware.rs / user_proxy.rs (ADR-076 §决策 4):"
        echo "$offenders"
        echo "The Gateway reverse proxy must never assert a user identity itself —"
        echo "the scope is injected once, in auth_middleware, from the verified token."
        exit 1
    fi
    echo "Identity-injection red line: OK"
}

# ADR-076 §决策 12: the account API exists only under `AUTH_MODE=multi_user`.
# Its routes must stay behind an `is_multi_user()` branch, so `local` mode
# returns 404 rather than exposing a gated-but-registered surface.
#
# Since ADR-084 the registrations live in the user service, not the Gateway —
# the Gateway registers no account route at all, it reverse-proxies the whole
# surface. Scanned file is therefore `acowork-user/src/http/mod.rs`; pointing
# the lint at the Gateway after the move would pass vacuously.
run_user_auth_mode_redline() {
    echo "Checking ADR-076 auth-mode routing red line..."
    local routes="$SCRIPT_DIR/../core/acowork-user/src/http/mod.rs"
    # Every `auth_routes()` / `account_api` registration line must sit inside a
    # branch whose scrutinee (`state.is_multi_user()`) is within the preceding
    # 4 lines.
    local offenders
    offenders=$(awk '
        { ctx[NR % 5] = $0 }
        /auth_api::auth_routes|account_api::/ {
            ok = 0
            for (i = 1; i <= 4; i++) {
                if (ctx[(NR - i) % 5] ~ /is_multi_user|AuthMode|auth_mode/) ok = 1
            }
            if (!ok) print FILENAME ":" NR ": " $0
        }
    ' "$routes" || true)
    if [ -n "$offenders" ]; then
        echo "ERROR: auth routes registered without an auth_mode branch (ADR-076 §决策 12):"
        echo "$offenders"
        echo "Register them only when the account system is active — an unregistered"
        echo "route cannot be reached by a future auth-middleware mistake."
        exit 1
    fi
    echo "Auth-mode routing red line: OK"
}

# ADR-076 §决策 8: the user-to-user chat store (`src/chat.rs`) owns the pairing
# layout — `data_dir/users/{min(a,b)}/chats/{max(a,b)}/`. A second place
# deriving that path would re-implement the min/max ordering, and the first
# thing an order-dependent re-derivation gets wrong is *whose* messages you
# are reading. The path is built once, in `chat.rs` — since ADR-084 inside
# `acowork-user`, so the scanned root follows the code.
run_user_chat_path_redline() {
    echo "Checking ADR-076 chat-path ownership red line..."
    local root="$SCRIPT_DIR/../core/acowork-user/src"
    local offenders
    # `chat_sessions.rs` owns the design-27 Chat session store under
    # `chat-sessions/{id}/` (ADR-084 §决策 1 / design 27 §3.1) — a
    # different domain from the `users/…/chats/{min__max}` pair tree this
    # red line guards; its file layout is defined by that file itself, so
    # its `messages.jsonl` is not a hand-rolled pair path.
    offenders=$(grep -rnE '\.join\("(users|chats|files|conversation\.json|messages\.jsonl)"\)' \
        --include='*.rs' "$root" \
        | grep -vE "^$root/chat\.rs:" \
        | grep -vE "^$root/chat_sessions\.rs:" \
        || true)
    if [ -n "$offenders" ]; then
        echo "ERROR: chat pairing path constructed outside chat.rs (ADR-076 §决策 8):"
        echo "$offenders"
        echo "Derive it via chat::pair_dir / chat_id — a hand-rolled path can"
        echo "disagree with the canonical min__max ordering."
        exit 1
    fi
    echo "Chat-path ownership red line: OK"
}

# ADR-084 §决策 1/3 suggested ceilings:
#   - `acowork-user` code must not reach into the Gateway's data directory
#     (the mirror of the fs red line above): all user data lives under the
#     service's own data dir, written by nobody else;
#   - the Gateway must never reference the Ed25519 *private* key file. It only
#     loads the public half (via the supervisor's `/health` data_dir), so a
#     private-key reference here would break the "verify, never sign"
#     property of §决策 3;
#   - the `X-Auth-*` header literals live in `acowork_core::auth` only — the
#     user service must import them, never hand-roll its own copy (migration
#     plan §5.3, header single-source rule).
run_user_boundary_redline() {
    echo "Checking ADR-084 user-domain boundary red lines..."
    local failed=0
    local gw_data_hits
    gw_data_hits=$(grep -rnE 'acowork-gateway[\\/]+data' --include='*.rs' \
        "$SCRIPT_DIR/../core/acowork-user" \
        | grep -vE ':[0-9]+:[[:space:]]*//' \
        || true)
    if [ -n "$gw_data_hits" ]; then
        echo "ERROR: acowork-user source references the Gateway data directory (ADR-084 §决策 1):"
        echo "$gw_data_hits"
        failed=1
    fi
    local key_hits
    key_hits=$(grep -rnE 'ed25519\.key' --include='*.rs' \
        "$SCRIPT_DIR/../core/acowork-gateway/src" \
        | grep -vE ':[0-9]+:[[:space:]]*//' \
        || true)
    if [ -n "$key_hits" ]; then
        echo "ERROR: Gateway source references the Ed25519 private key file (ADR-084 §决策 3):"
        echo "$key_hits"
        failed=1
    fi
    local hdr_hits
    hdr_hits=$(grep -rnE '"x-auth-' --include='*.rs' \
        "$SCRIPT_DIR/../core/acowork-user" \
        | grep -vE ':[0-9]+:[[:space:]]*//' \
        || true)
    if [ -n "$hdr_hits" ]; then
        echo "ERROR: acowork-user source hand-rolls an X-Auth-* header literal (single-source rule):"
        echo "$hdr_hits"
        echo "Import the constants from acowork_core::auth instead."
        failed=1
    fi
    if [ "$failed" -ne 0 ]; then
        echo ""
        echo "The user data dir and the signing key belong to acowork-user; the"
        echo "Gateway reaches the user domain only through user_proxy / snapshot pull."
        exit 1
    fi
    echo "User-domain boundary red lines: OK"
}

# ADR-087 D5: the permission middleware is the single enforcement point for
# every /api/agents/{id}/** and /api/nodes/{id}/** route. Its classifier
# answers unregistered routes with the fail-closed AgentManage tier, but a
# route that bypasses the classifier (e.g. a future non-/api mount of an
# agent handler) would silently skip the gate. Invariant: every route
# pattern registered on the Gateway router that addresses an agent or node
# resource must live under /api/agents/ or /api/nodes/ (or be an
# explicitly whitelisted machine endpoint).
run_permission_route_redline() {
    echo "Checking ADR-087 permission-route registration red line..."
    local root="$SCRIPT_DIR/../core/acowork-gateway/src"
    local offenders
    # (1) Prefix check: every `.route("...")` literal that mentions
    # agents/{id} or nodes/{id} but is NOT under the /api prefix the
    # classifier walks — such a route would be invisible to ADR-087.
    offenders=$(grep -rhoE '\.route\("[^"]+"' --include='*.rs' "$root"         | sed -E 's/\.route\("//'         | grep -E '^(/agents/|/nodes/|v1/agents|v1/nodes)'         || true)
    if [ -n "$offenders" ]; then
        echo "ERROR: route registered outside /api — invisible to the ADR-087"
        echo "permission classifier (http/permission.rs extract_target):"
        echo "$offenders"
        echo "Register under /api/agents/ or /api/nodes/, or extend the"
        echo "classifier deliberately in the same change."
        exit 1
    fi

    # (2) Enumeration assertion (ADR-087 §9.2): every `{rest}` head
    # segment registered under /api/agents/{id}/ must be a KNOWN segment.
    # A new sub-route forces an explicit update of the list below — the
    # update IS the review moment where the author must state the tier
    # (classify() default-denies unknown segments to AgentManage, so this
    # red line guards the *visibility* of new routes, not their safety).
    local known_segments="avatar avatar-config avatar-file claim clone config cron debug files git guests health interactions latest-session lsp-endpoint manifest mcp-ensure mcp-servers mcp-tools memory messages model owner permissions prompts providers publish rag search sessions shell-risk-rules skills start status stop token-stats tools upgrade visibility workspaces {*rest}"
    local segments seg
    segments=$(grep -rhoE '\.route\("/api/agents/\{id\}/[^"{]*' --include='*.rs' "$root"         | sed -E 's|.*\{id\}/||; s|/.*$||'         | grep -v '^$' | sort -u)
    # Also the bare catch-all wildcard (proxy forwards `{*rest}`).
    if grep -rq '\.route("/api/agents/{id}/{\*rest}"' --include='*.rs' "$root"; then
        segments=$(printf '%s\n{*rest}\n' "$segments" | sort -u)
    fi
    for seg in $segments; do
        case " $known_segments " in
            *" $seg "*) ;;
            *)
                echo "ERROR: new /api/agents/{id}/$seg route is not in the"
                echo "permission red-line enumeration. Add it to"
                echo "known_segments in dev/ci.sh AND give it an explicit"
                echo "tier in http/permission.rs classify() (or confirm it"
                echo "belongs in the default-deny AgentManage bucket)."
                exit 1
                ;;
        esac
    done

    # (3) Node side: /api/nodes/{id}/... head segments must be known too
    # (owner / guests / visibility are the attribution trio; `claim` is the
    # ADR-087 D7 ownerless-claim route, handler-enforced via `Tier::Claim`;
    # `lsp` is the ADR-089 relay passthrough and `lsp-admin` the ADR-091
    # registry proxy — both gated in http/lsp_admin_proxy.rs; anything else
    # is NodeManage by the route_requirement default).
    local known_node_segments="claim owner guests visibility permissions lsp lsp-admin"
    local nsegs nseg
    nsegs=$(grep -rhoE '\.route\("/api/nodes/\{id\}/[^"{]*' --include='*.rs' "$root"         | sed -E 's|.*\{id\}/||; s|/.*$||'         | grep -v '^$' | sort -u)
    for nseg in $nsegs; do
        case " $known_node_segments " in
            *" $nseg "*) ;;
            *)
                echo "ERROR: new /api/nodes/{id}/$nseg route is not in the"
                echo "permission red-line enumeration (known_node_segments)."
                exit 1
                ;;
        esac
    done

    echo "Permission-route red line: OK"
}

# ADR-091 D11/D15 red line: the LSP server registry is the Node's state.
# The relay is a stateless protocol bridge — it reads the registry the Node
# wrote, probes servers and relays the WebSocket. Invariants:
#   (1) no install-state vocabulary anywhere in the relay crate: that schema
#       is owned by core::lsp and written only by the Node (a relay-side copy
#       would race the Node's atomic rewrite, and since the relay reloads the
#       registry only on restart the drift survives until the next restart);
#   (2) no `/api/lsp/install` route in the relay source: the install executor
#       moved to the Node with D15. Tests may still hit the path to assert the
#       404, so this check scans src only;
#   (3) filesystem writes only inside test fixtures. Ceiling, not allowlist:
#       these are the surviving `#[cfg(test)]` call sites. Counts may only go
#       DOWN — lower a number in the same commit that removes a call site.
LSP_RELAY_WRITE_CEILING="
config.rs:3
project_root.rs:7
"
run_lsp_registry_redline() {
    echo "Checking ADR-091 LSP relay statelessness red line..."
    local crate="$SCRIPT_DIR/../core/acowork-lsp-relay"
    local root="$crate/src"
    local failed=0

    local vocab
    vocab=$(grep -rnE '(install_state|LspInstallRecord|LspRegistryFile|LspSource)' --include='*.rs' "$crate"         | grep -vE ':[0-9]+:[[:space:]]*(//|///)'         || true)
    if [ -n "$vocab" ]; then
        echo "ERROR: LSP install-state vocabulary in the relay crate (ADR-091 D11):"
        echo "$vocab"
        failed=1
    fi

    local routes
    routes=$(grep -rnE '"/api/lsp/install' --include='*.rs' "$root" || true)
    if [ -n "$routes" ]; then
        echo "ERROR: install route registered in the relay (ADR-091 D15):"
        echo "$routes"
        echo "Installs run on the Node: POST /lsp/servers/{name}/install."
        failed=1
    fi

    local files f rel count ceiling
    files=$(grep -rlE '(std::fs::write|fs::write\(|File::create|OpenOptions::new)' --include='*.rs' "$root" || true)
    while IFS= read -r f; do
        [ -z "$f" ] && continue
        rel="${f#"$root"/}"
        count=$(grep -cE '(std::fs::write|fs::write\(|File::create|OpenOptions::new)' "$f" || true)
        ceiling=$(printf '%s\n' "$LSP_RELAY_WRITE_CEILING" | grep -E "^$rel:" | cut -d: -f2 || true)
        if [ -z "$ceiling" ]; then
            echo "ERROR: ${rel}: ${count} filesystem write(s) — not on the ADR-091 relay ceiling list."
            failed=1
        elif [ "$count" -gt "$ceiling" ]; then
            echo "ERROR: ${rel}: ${count} filesystem write(s), ceiling is ${ceiling}."
            failed=1
        fi
    done <<< "$files"

    if [ "$failed" -ne 0 ]; then
        echo ""
        echo "The registry and its install executor are Node state:"
        echo "  core/acowork-node/src/lsp_registry.rs  (single writer, atomic rewrite)"
        echo "  core/acowork-node/src/lsp_install.rs   (script runner)"
        echo "The relay only reads the registry the Node wrote."
        exit 1
    fi
    echo "LSP relay statelessness red line: OK"
}

# Embedding ONNX Runtime compatibility red line.
#
# `ort` resolves its API table at runtime with GetApi(ORT_API_VERSION). If the
# compiled api-N is newer than the installed libonnxruntime, GetApi returns null
# and `ort` PANICS inside its `Once`-backed global init — poisoning it for the
# whole process. Every subsequent load then fails with a misleading
# "Mutex poisoned" (the poison of ort's own internal Mutex, not ours).
#
# The compiled api level must therefore never exceed the ORT minor version the
# project still supports (ORT_VERSION_LEGACY in dev/common/setup_ort.sh, the legacy
# glibc 2.31 build). GetApi is backward compatible, so a LOW api level runs
# fine on a newer runtime — this is a one-sided constraint.
run_ort_api_redline() {
    echo "Checking ONNX Runtime API-level compatibility red line..."
    local cargo_toml="$SCRIPT_DIR/../core/Cargo.toml"
    local setup_ort="$SCRIPT_DIR/common/setup_ort.sh"

    local api_feature
    api_feature=$(grep -oE '"api-[0-9]+"' "$cargo_toml" | head -1 | grep -oE '[0-9]+')
    if [ -z "$api_feature" ]; then
        echo "ERROR: could not find an api-N feature for ort in core/Cargo.toml."
        exit 1
    fi

    local legacy
    legacy=$(grep -oE '^ORT_VERSION_LEGACY="[0-9]+\.[0-9]+' "$setup_ort" | grep -oE '[0-9]+\.[0-9]+$')
    if [ -z "$legacy" ]; then
        echo "ERROR: could not read ORT_VERSION_LEGACY from dev/common/setup_ort.sh."
        exit 1
    fi

    # ORT 1.19.2 defines ORT_API_VERSION 19 — the api-N feature number is
    # the API level, which tracks the runtime MINOR version.
    local legacy_minor="${legacy#*.}"
    if [ "$api_feature" -gt "$legacy_minor" ]; then
        echo "ERROR: ort api-${api_feature} is newer than ONNX Runtime ${legacy}"
        echo "(ORT_VERSION_LEGACY in dev/common/setup_ort.sh, the oldest supported runtime)."
        echo "Loading any embedding model will panic in ort's global init and then"
        echo "fail as 'Mutex poisoned'. Lower the api-N feature, or raise"
        echo "ORT_VERSION_LEGACY and the minimum glibc it implies."
        exit 1
    fi

    echo "ONNX Runtime API red line: OK (ort api-${api_feature} <= ORT ${legacy})"
}

run_clippy() {
    echo "Running cargo clippy..."
    cargo clippy --all-targets -- -D warnings
    echo "Running cargo clippy for acowork-embed..."
    cargo clippy -p acowork-embed --all-targets -- -D warnings
}

run_test() {
    echo "Running cargo test..."
    # acowork-embed links to ONNX Runtime (dev/common/setup_ort.sh). On machines
    # without ORT installed, building its tests fails at link time — but
    # it isn't depended on by any other crate's tests, so test the rest of
    # the workspace first, then attempt the embed tests separately and
    # downgrade a link failure to a warning (matching run_smoke's policy).
    cargo test --workspace --exclude acowork-embed
    echo "Running acowork-embed tests..."
    if [ -x target/debug/acowork-embed ]; then
        echo "acowork-embed: reusing existing binary"
        cargo test -p acowork-embed --no-run 2>&1 \
            | grep -vE 'ORT_LIB_LOCATION|Downloading.*onnx' || true
    else
        cargo test -p acowork-embed --features download-ort \
            || echo "WARNING: acowork-embed tests skipped (ORT not configured)"
    fi
}

run_integration() {
    echo "=== Running node control-plane e2e tests (ADR-055 Phase 2) ==="
    cargo test -p acowork-gateway --test node_control_plane_e2e -- --test-threads=1
    echo "=== Running gateway settings API tests ==="
    cargo test -p acowork-gateway --test settings_api -- --test-threads=1
    echo "=== Running node wire-protocol golden tests ==="
    cargo test -p acowork-core --test node_proto_golden
    echo "=== Running acowork-node dependency red-line test ==="
    cargo test -p acowork-node --test dependency_redline
}

# Frontend smoke suite (dev/e2e_frontend_smoke/smoke_test.py): boots a
# real Gateway + local node agent against temp homes and exercises the
# HTTP/MQTT surface the desktop app talks to (config, sessions, workspaces,
# memory, docs, settings + Phase 5a auth). Requires debug binaries.
run_smoke() {
    echo "=== Building debug binaries for smoke tests ==="
    # acowork-embed needs ONNX Runtime (dev/common/setup_ort.sh) which may not
    # be installed on this machine; it is spawned as a sidecar and not
    # depended on by the other crates, so rebuild everything else and
    # reuse an existing embed binary when ORT is unavailable.
    cargo build --workspace --bins --exclude acowork-embed
    if [ -x target/debug/acowork-embed ]; then
        echo "acowork-embed: reusing existing binary (ORT not configured)"
    else
        echo "acowork-embed: building with download-ort feature..."
        cargo build -p acowork-embed --features download-ort \
            || echo "WARNING: acowork-embed build failed (embedding unavailable in smoke)"
    fi
    echo "=== Running frontend smoke tests ==="
    python3 -u "$SCRIPT_DIR/e2e_frontend_smoke/smoke_test.py"
}

case "$MODE" in
    check)
        run_gateway_fs_redline
        run_user_boundary_redline
        run_meta_layout_redline
        run_session_full_read_redline
        run_gateway_auth_scope_redline
        run_user_auth_mode_redline
        run_user_chat_path_redline
        run_permission_route_redline
        run_lsp_registry_redline
        run_ort_api_redline
        run_check
        ;;
    clippy)
        run_clippy
        ;;
    test)
        run_test
        ;;
    integration)
        run_integration
        ;;
    smoke)
        run_smoke
        ;;
    all)
        run_node_redline
        run_mqtt_redline
        run_gateway_fs_redline
        run_user_boundary_redline
        run_meta_layout_redline
        run_session_full_read_redline
        run_gateway_auth_scope_redline
        run_user_auth_mode_redline
        run_user_chat_path_redline
        run_permission_route_redline
        run_lsp_registry_redline
        run_ort_api_redline
        run_check
        run_clippy
        run_test
        run_integration
        run_smoke
        ;;
    *)
        echo "Unknown mode: $MODE"
        echo "Usage: $0 [check|clippy|test|integration|smoke|all]"
        exit 1
        ;;
esac

echo "=== CI completed successfully ==="
