#!/usr/bin/env python3
"""
Sync ACowork example agents into the ACoworkWebSite Agent Hub.

Single source of truth is `examples/*/manifest.toml` (PRD D4: metadata is
generated, never hand-copied). This script projects it into the static hub
payload and copies the signed package + avatar next to it.

Usage:
    python dev/common/sync-agent-hub.py [--site PATH] [--dry-run]

Exit codes:
    0  ok
    1  data error (missing manifest field / no matching package / bad TOML)
    2  environment error (missing repo, no python 3.11+ for tomllib)

Idempotent: re-run any time; output is deterministic apart from `generated_at`.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:
    sys.exit("[sync-agent-hub] FATAL: needs Python 3.11+ (tomllib). Exit 2")

SCHEMA_VERSION = 2

# role -> (category key, human label). Unknown roles fail loudly (PRD §6:
# derived fields are produced here or not at all — consumers must not guess).
CATEGORY_BY_ROLE = {
    "Product Manager": ("product", "Product"),
    "Project Manager": ("project", "Project"),
    "Quality Assurance Manager": ("quality", "Quality"),
    "Software Architect": ("engineering", "Engineering"),
    "Senior Software Engineer": ("engineering", "Engineering"),
    "Document Manager": ("knowledge", "Knowledge"),
}

# Permissions that can run commands or write outside a sandbox get escalated
# styling on the card (PRD R5). Keep in sync with manifest `[[permissions]]`.
HIGH_RISK_PERMISSIONS = {"Shell", "FilesystemWrite", "Sandbox"}


def fail(msg: str) -> None:
    print(f"[sync-agent-hub] ERROR: {msg}", file=sys.stderr)
    sys.exit(1)


def git_ref(repo: Path) -> str:
    try:
        out = subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=repo, capture_output=True, text=True, check=True
        )
        return out.stdout.strip()
    except Exception:
        return "unknown"


def read_skills(agent_dir: Path) -> list[str]:
    skills_dir = agent_dir / "skills"
    if not skills_dir.is_dir():
        return []
    names = sorted(
        d.name for d in skills_dir.iterdir()
        if d.is_dir() and (d / "SKILL.md").is_file()
    )
    return names


def build_entry(agent_dir: Path, packages_dir: Path) -> dict:
    manifest_path = agent_dir / "manifest.toml"
    try:
        m = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
    except Exception as e:
        fail(f"cannot parse {manifest_path}: {e}")

    required = ["agent_id", "version", "name", "display_name", "role", "description", "author"]
    missing = [k for k in required if not m.get(k)]
    if missing:
        fail(f"{agent_dir.name}/manifest.toml missing required field(s): {', '.join(missing)}")

    agent_id = m["agent_id"]

    if m["role"] not in CATEGORY_BY_ROLE:
        fail(
            f"{agent_dir.name}: role {m['role']!r} has no category mapping. "
            "Add it to CATEGORY_BY_ROLE — the hub must not guess."
        )
    category, category_label = CATEGORY_BY_ROLE[m["role"]]

    pkg = packages_dir / f"{agent_id}.agent"
    if not pkg.is_file():
        fail(
            f"signed package not found: {pkg.name}\n"
            f"    build it first:  ./dev/common/build-agent.sh examples/{agent_dir.name}"
        )

    payload = pkg.read_bytes()
    if not payload:
        fail(f"{pkg.name} is empty")

    avatar_src = None
    if m.get("avatar"):
        candidate = agent_dir / m["avatar"]
        if candidate.is_file():
            avatar_src = candidate
        else:
            print(
                f"[sync-agent-hub] WARN: {agent_dir.name}: avatar declared as "
                f"{m['avatar']!r} but file missing — falling back to initials tile",
                file=sys.stderr,
            )

    avatar_out = None
    if avatar_src:
        avatar_out = f"avatars/{agent_id}{avatar_src.suffix.lower()}"

    permissions = sorted({p.get("type") for p in m.get("permissions", []) if p.get("type")})
    if not permissions:
        print(
            f"[sync-agent-hub] WARN: {agent_dir.name} declares no permissions",
            file=sys.stderr,
        )

    capabilities = [
        {"key": k, "description": v.get("description", "")}
        for k, v in sorted((m.get("capabilities") or {}).items())
    ]

    return {
        "agent_id": agent_id,
        "version": m["version"],
        "name": m["name"],
        "display_name": m["display_name"],
        "role": m["role"],
        "description": m["description"],
        "author": m["author"],
        "category": category,
        "category_label": category_label,
        "runtime_version": m.get("runtime_version", "unknown"),
        "avatar": avatar_out,
        "permissions": permissions,
        "high_risk_permissions": sorted(
            set(permissions) & HIGH_RISK_PERMISSIONS
        ),
        "capabilities": capabilities,
        "skills": read_skills(agent_dir),
        "package": {
            "file": f"downloads/{agent_id}.agent",
            "size_bytes": len(payload),
            "sha256": hashlib.sha256(payload).hexdigest(),
        },
        "_avatar_src": str(avatar_src) if avatar_src else None,
    }


DATA_START = "<!-- agent-hub:data:start"
DATA_END = "<!-- agent-hub:data:end"


def write_embedded_snapshot(hub_dir: Path, doc: dict) -> bool:
    """Mirror the index into a <script type="application/json"> block.

    Purpose: the page fetches data/agents.json over HTTP, but a reader who
    opens agents/index.html straight off disk (file://) gets a CORS error and
    an empty page. The embedded snapshot is the fallback for that case.

    Both copies are generated from the same manifests by this script, so they
    cannot drift — but if the markers are ever renamed the page would silently
    keep serving a stale snapshot, so a missing marker is a hard error.
    """
    page = hub_dir / "index.html"
    if not page.is_file():
        print(f"[sync-agent-hub] WARN: {page} not found — no snapshot written", file=sys.stderr)
        return False

    text = page.read_text(encoding="utf-8")
    i = text.find(DATA_START)
    j = text.find(DATA_END)
    if i == -1 or j == -1 or j < i:
        fail(
            f"{page}: snapshot markers not found "
            f"({DATA_START!r} / {DATA_END!r}). "
            "The file:// fallback would go stale — restore the markers."
        )

    payload = json.dumps(doc, indent=2, ensure_ascii=False)
    # `</` inside a <script> block would close the element early; `<\/` is a
    # valid JSON escape for `/` and does not match the parser's terminator.
    payload = payload.replace("</", "<\\/")

    block = (
        f"{DATA_START}\n"
        "     GENERATED by ACowork/dev/common/sync-agent-hub.py — do not hand-edit.\n"
        "     This is the file:// fallback copy of data/agents.json. Over HTTP the page\n"
        "     fetches agents/data/agents.json and this snapshot is ignored; it exists so\n"
        "     double-clicking agents/index.html still renders the library offline. -->\n"
        f'<script id="ah-data" type="application/json">{payload}</script>\n'
        f"  {DATA_END}"
    )

    page.write_text(text[:i] + block + text[j + len(DATA_END):], encoding="utf-8")
    return True


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument(
        "--site",
        default=None,
        help="ACoworkWebSite repo root (default: ../ACoworkWebSite next to ACowork)",
    )
    ap.add_argument("--dry-run", action="store_true", help="print plan, write nothing")
    args = ap.parse_args()

    repo_root = Path(__file__).resolve().parents[2]  # .../ACowork
    examples = repo_root / "examples"
    packages_dir = examples / "agent-packages"
    if not examples.is_dir() or not packages_dir.is_dir():
        print(f"[sync-agent-hub] FATAL: expected {examples} and {packages_dir}", file=sys.stderr)
        sys.exit(2)

    site_root = Path(args.site) if args.site else repo_root.parent / "ACoworkWebSite"
    hub_dir = site_root / "agents"
    if not site_root.is_dir():
        print(f"[sync-agent-hub] FATAL: site repo not found at {site_root}", file=sys.stderr)
        sys.exit(2)

    # Directories that hold a manifest are agents. `agnes-document-skills`
    # (skills-only, no manifest.toml) is deliberately out of scope — PRD §2.
    agent_dirs = sorted(
        d for d in examples.iterdir()
        if d.is_dir() and (d / "manifest.toml").is_file()
    )
    if not agent_dirs:
        fail(f"no agent manifests under {examples}")

    entries = []
    dir_by_id: dict[str, Path] = {}
    for d in agent_dirs:
        e = build_entry(d, packages_dir)
        if e["agent_id"] in dir_by_id:
            fail(
                f"duplicate agent_id {e['agent_id']!r} in "
                f"{dir_by_id[e['agent_id']].name} and {d.name}"
            )
        dir_by_id[e["agent_id"]] = d
        entries.append(e)

    entries.sort(key=lambda e: (e["category"], e["display_name"].lower()))

    # Only advertise categories that actually have an agent on the shelf —
    # an empty filter chip is worse than no filter chip.
    used = {e["category"]: e["category_label"] for e in entries}
    categories = [{"key": k, "label": used[k]} for k in sorted(used)]

    doc = {
        "schema_version": SCHEMA_VERSION,
        "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "source": {
            "repo": "acowork/ACowork",
            "ref": git_ref(repo_root),
            "path": "examples/",
            "generator": "dev/common/sync-agent-hub.py",
        },
        "categories": categories,
        "agents": entries,
    }

    # Keep the manifest's own avatar path — the output file is renamed to
    # `<agent_id>.<ext>` for collision safety, so the source can't be derived
    # from it (`assets/avatar.jpg` != `com.acowork.foo.jpg`).
    avatar_src_by_id = {e["agent_id"]: e.get("_avatar_src") for e in entries}

    for e in entries:
        e.pop("_avatar_src", None)

    if args.dry_run:
        print(json.dumps(doc, indent=2, ensure_ascii=False))
        print(f"\n[sync-agent-hub] dry-run: {len(entries)} agents, nothing written")
        return

    (hub_dir / "data").mkdir(parents=True, exist_ok=True)
    (hub_dir / "downloads").mkdir(parents=True, exist_ok=True)
    (hub_dir / "avatars").mkdir(parents=True, exist_ok=True)

    # Prune stale artifacts so a deleted example cannot keep downloading.
    keep = {Path(e["package"]["file"]).name for e in entries}
    for f in (hub_dir / "downloads").glob("*.agent"):
        if f.name not in keep:
            f.unlink()
            print(f"[sync-agent-hub] pruned stale {f.name}")

    keep_av = {Path(e["avatar"]).name for e in entries if e["avatar"]}
    for f in (hub_dir / "avatars").glob("*"):
        if f.is_file() and f.name not in keep_av:
            f.unlink()
            print(f"[sync-agent-hub] pruned stale avatar {f.name}")

    for e in entries:
        src_pkg = packages_dir / f"{e['agent_id']}.agent"
        shutil.copy2(src_pkg, hub_dir / "downloads" / src_pkg.name)
        src_dir = dir_by_id[e["agent_id"]]
        if e["avatar"] and avatar_src_by_id.get(e["agent_id"]):
            av_file = src_dir / avatar_src_by_id[e["agent_id"]]
            if av_file.is_file():
                shutil.copy2(av_file, hub_dir / e["avatar"])
            else:
                fail(f"avatar vanished: {av_file}")

    out = hub_dir / "data" / "agents.json"
    out.write_text(
        json.dumps(doc, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )

    snapshot = write_embedded_snapshot(hub_dir, doc)

    total = sum(e["package"]["size_bytes"] for e in entries)
    print(
        f"[sync-agent-hub] wrote {out} | {len(entries)} agents | "
        f"{total/1024:.0f} KiB of packages -> {hub_dir}"
        + (" | snapshot embedded" if snapshot else "")
    )


if __name__ == "__main__":
    main()
