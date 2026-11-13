#!/usr/bin/env python3
"""One-off data fixer for pre-ADR-114 `local` installs. NOT part of any build.

ADR-114 removed `AUTH_MODE`: the account system is now always on. A legacy
`local` install may have:

  * `user_profiles.json` (presentation-only single user) but no `accounts.json`;
  * chat `session.json` rows whose owner / human participant id is the old
    `"human"` sentinel (or absent), which no longer resolves to a real account;
  * `accounts.json` holding only the passwordless `admin` seed — that is NOT
    dirty data: it is the `needs_setup` state and `POST /api/auth/setup`
    activates it in place. This script deliberately leaves it alone.

What this script does:

  1. If `accounts.json` is missing/empty and `user_profiles.json` exists, create
     exactly one `admin` account from the first profile, with the
     `$disabled$` sentinel hash. The Gateway will report `needs_setup` and the
     Desktop will offer the first-boot form to set its password.
  2. Rewrite the legacy `"human"` / empty owner & human-participant ids in
     `{data_dir}/chat-sessions/*/session.json` to that admin's `user_id`.
     Agent participants (`identity != "human"`) are never touched.
  3. Rewrite `user_profiles.json` is NOT needed — `sync_profiles()` rebuilds it
     from `accounts.json` on the first account mutation.

Safety:
  * dry-run by default; pass `--apply` to write.
  * `--apply` takes a timestamped backup of every file it is about to modify
    (and of the newly written `accounts.json`'s parent) before touching it.
  * idempotent: a second run finds no sentinels and no missing store.

Usage:
  python scripts/migrate_legacy_local_data.py --data-dir /path/to/acowork-user
  python scripts/migrate_legacy_local_data.py --data-dir /path --apply

This file lives in `scripts/` and is referenced by no Cargo/Vite/npm target.
It is an operator tool for existing dev data, not production code.
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
import uuid
from datetime import datetime, timezone
from pathlib import Path

DISABLED_PASSWORD_HASH = "$disabled$"
LEGACY_SENTINELS = {"", "human"}
BACKUP_TAG = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")


def read_json(path: Path):
    with path.open("r", encoding="utf-8") as fh:
        return json.load(fh)


def backup(path: Path, root: Path) -> Path:
    """Copy `path` beside itself with a .bak-<tag> suffix; return the copy."""
    dst = path.with_name(path.name + f".bak-{BACKUP_TAG}")
    if not dst.exists():
        shutil.copy2(path, dst)
    return dst


def write_json(path: Path, value, root: Path, apply: bool) -> str:
    if not apply:
        return f"would write {path.relative_to(root)}"
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        made = backup(path, root)
        print(f"  backup: {made.name}")
    tmp = path.with_name(path.name + ".tmp")
    with tmp.open("w", encoding="utf-8") as fh:
        json.dump(value, fh, ensure_ascii=False, indent=2)
        fh.write("\n")
    tmp.replace(path)
    return f"wrote {path.relative_to(root)}"


def iso_now() -> str:
    return datetime.now(timezone.utc).isoformat().replace("+00:00", "Z")


def account_from_profile(profile: dict) -> dict:
    """Project a legacy UserProfile onto a UserAccount (ADR-076 § Decision 1)."""
    stamp = iso_now()
    return {
        "user_id": profile.get("user_id") or str(uuid.uuid4()),
        "username": "admin",
        "display_name": profile.get("display_name") or "Admin",
        "role": "admin",
        "password_hash": DISABLED_PASSWORD_HASH,
        "password_changed_at": stamp,
        "password_expires_at": None,
        "language": profile.get("language") or "zh-CN",
        "timezone": profile.get("timezone") or "Asia/Shanghai",
        "city": profile.get("city"),
        "country": profile.get("country"),
        "occupation": profile.get("occupation"),
        "avatar": profile.get("avatar"),
        "builtin_avatar": profile.get("builtin_avatar"),
        "communication_style": profile.get("communication_style"),
        "custom": profile.get("custom") or {},
        "created_at": stamp,
        "updated_at": stamp,
        "last_login_at": None,
        "disabled_at": None,
        "invite_token_hash": None,
        "invite_expires_at": None,
    }


def resolve_admin(data_dir: Path, apply: bool):
    """Return (user_id, list_of_action_strings). Never overwrites a real admin."""
    actions: list[str] = []
    accounts_path = data_dir / "accounts.json"
    profiles_path = data_dir / "user_profiles.json"

    accounts = None
    if accounts_path.exists():
        try:
            accounts = read_json(accounts_path)
        except (OSError, ValueError) as exc:
            sys.exit(f"cannot parse {accounts_path}: {exc}")

    existing = (accounts or {}).get("accounts") or []
    admins = [a for a in existing if a.get("role") == "admin"]
    login_capable = [
        a for a in admins
        if a.get("password_hash") not in (None, "", DISABLED_PASSWORD_HASH)
        and a.get("disabled_at") is None
    ]
    if existing:
        if not admins:
            # The store is `needs_setup`: the Desktop setup flow will CREATE
            # a new admin with a brand-new user_id. Attributing legacy
            # sessions to the first ordinary account would hand them to the
            # wrong person, so refuse and let the operator run setup first.
            actions.append(
                f"accounts.json has {len(existing)} account(s) but NO admin; "
                "state=needs_setup. Setup will create a new admin with a new "
                "user_id, so legacy sessions cannot be attributed yet. "
                "Finish first-boot setup (Desktop wizard or admin-setup CLI), "
                "then re-run this script."
            )
            return None, actions
        chosen = (login_capable or admins)[0]
        state = "ready" if login_capable else "needs_setup"
        actions.append(
            f"accounts.json already has {len(existing)} account(s); state={state}; "
            "leaving the store untouched"
        )
        return chosen["user_id"], actions

    # Empty / missing store: seed one admin from the legacy presentation view.
    profile = None
    if profiles_path.exists():
        try:
            profiles = read_json(profiles_path)
            users = (profiles or {}).get("users") or []
            if users:
                profile = users[0]
        except (OSError, ValueError) as exc:
            actions.append(f"warning: cannot parse {profiles_path.name}: {exc}")
    if profile is None:
        profile = {"display_name": "Admin"}

    account = account_from_profile(profile)
    new_store = {"version": 1, "accounts": [account]}
    actions.append(
        f"create accounts.json with admin user_id={account['user_id']} "
        "(passwordless → needs_setup; set the password via the Desktop setup form)"
    )
    actions.append("  " + write_json(accounts_path, new_store, data_dir, apply))
    return account["user_id"], actions


def fix_sessions(data_dir: Path, admin_id: str, apply: bool) -> list[str]:
    actions: list[str] = []
    root = data_dir / "chat-sessions"
    if not root.is_dir():
        return ["no chat-sessions/ directory — nothing to backfill"]

    changed = 0
    for session_file in sorted(root.glob("*/session.json")):
        try:
            session = read_json(session_file)
        except (OSError, ValueError) as exc:
            actions.append(f"warning: skipping {session_file.name}: {exc}")
            continue
        dirty = False

        # `owner` may be missing entirely, JSON null, or one of the legacy
        # strings. All three mean "predates accounts" and must be backfilled.
        # The isinstance guard keeps an unexpected structured value from
        # raising TypeError on the set membership test.
        owner = session.get("owner")
        if owner is None or (isinstance(owner, str) and owner in LEGACY_SENTINELS):
            session["owner"] = admin_id
            dirty = True

        for participant in session.get("participants") or []:
            if not isinstance(participant, dict):
                continue
            identity = participant.get("identity")
            pid = participant.get("id")
            # Only human participants with a legacy sentinel id are rewritten.
            # Agent instance ids are real and must survive intact.
            if identity == "human" and (
                pid is None
                or (isinstance(pid, str) and pid in LEGACY_SENTINELS)
            ):
                participant["id"] = admin_id
                dirty = True

        if dirty:
            changed += 1
            rel = session_file.relative_to(data_dir)
            actions.append(f"  " + write_json(session_file, session, data_dir, apply))
            if not apply:
                actions[-1] = f"  would backfill {rel}"

    actions.insert(0, f"session backfill: {changed} file(s) need owner/participant rewriting")
    return actions


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--data-dir", required=True, type=Path,
                        help="the acowork-user data directory (holds accounts.json / chat-sessions/)")
    parser.add_argument("--apply", action="store_true",
                        help="actually write (default: dry-run)")
    args = parser.parse_args()

    data_dir = args.data_dir.resolve()
    if not data_dir.is_dir():
        sys.exit(f"not a directory: {data_dir}")

    print(f"data dir : {data_dir}")
    print(f"mode     : {'APPLY (writes)' if args.apply else 'DRY-RUN (no writes)'}")
    print("")

    admin_id, account_actions = resolve_admin(data_dir, args.apply)
    print("== accounts ==")
    for line in account_actions:
        print(line)
    print("")
    print("== chat sessions ==")
    if admin_id is None:
        print("skipped: no administrator to attribute legacy sessions to")
    else:
        for line in fix_sessions(data_dir, admin_id, args.apply):
            print(line)

    if not args.apply:
        print("\nNothing was written. Re-run with --apply after reviewing the plan.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
