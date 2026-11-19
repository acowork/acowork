#!/usr/bin/env python3
"""Pack the doc-clone fixture into an unsigned .app for ADR-116 install smoke.

Usage (repo root):
  python dev/common/pack-doc-clone.py [--output PATH]
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import subprocess
import sys
import tempfile


ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
FIXTURE = os.path.join(ROOT, "dev", "fixtures", "doc-clone-app")


def is_windows() -> bool:
    return os.name == "nt"


def find_doc_bin() -> str:
    name = "acowork-doc.exe" if is_windows() else "acowork-doc"
    candidates = [
        os.path.join(ROOT, "core", "target", "debug", name),
        os.path.join(ROOT, "core", "target", "release", name),
        os.path.join(ROOT, "target", "debug", name),
        os.path.join(ROOT, "target", "release", name),
        os.path.join(ROOT, "target-e2e", "debug", name),
        os.path.join(ROOT, "target-e2e", "release", name),
    ]
    for c in candidates:
        if os.path.isfile(c):
            return c
    raise SystemExit(
        "acowork-doc binary not found; build it first:\n"
        "  cd core && cargo build -p acowork-doc"
    )


def find_pack_bin() -> str | None:
    names = (
        ["acowork-pack-app.exe", "acowork-pack-app"]
        if is_windows()
        else ["acowork-pack-app"]
    )
    roots = [
        os.path.join(ROOT, "core", "target", "debug"),
        os.path.join(ROOT, "core", "target", "release"),
        os.path.join(ROOT, "target", "debug"),
        os.path.join(ROOT, "target", "release"),
    ]
    for root in roots:
        for name in names:
            path = os.path.join(root, name)
            if os.path.isfile(path):
                return path
    return None


def rewrite_entry(app_toml: str, entry: str) -> None:
    text = open(app_toml, encoding="utf-8").read()
    text2, n = re.subn(r'(?m)^entry\s*=\s*".*"\s*$', f'entry = "{entry}"', text, count=1)
    if n != 1:
        raise SystemExit("failed to rewrite package.entry in staging app.toml")
    open(app_toml, "w", encoding="utf-8").write(text2)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--output", default="")
    args = ap.parse_args()

    out = args.output or os.path.join(ROOT, "build", "doc-clone-1.0.0.unsigned.app")
    os.makedirs(os.path.dirname(out) or ".", exist_ok=True)

    doc_bin = find_doc_bin()
    bin_name = os.path.basename(doc_bin)

    with tempfile.TemporaryDirectory(prefix="doc-clone-pack-") as staging:
        bin_dir = os.path.join(staging, "bin")
        os.makedirs(bin_dir, exist_ok=True)
        shutil.copy2(doc_bin, os.path.join(bin_dir, bin_name))
        shutil.copy2(os.path.join(FIXTURE, "app.toml"), os.path.join(staging, "app.toml"))
        rewrite_entry(os.path.join(staging, "app.toml"), f"bin/{bin_name}")

        pack = find_pack_bin()
        if pack:
            cmd = [pack, "--app-dir", staging, "--output", out]
        else:
            cmd = [
                "cargo",
                "run",
                "-p",
                "acowork-sign",
                "--bin",
                "acowork-pack-app",
                "--quiet",
                "--",
                "--app-dir",
                staging,
                "--output",
                out,
            ]
        print("packing with:", " ".join(cmd))
        subprocess.check_call(cmd, cwd=os.path.join(ROOT, "core") if pack is None else ROOT)

    print("wrote", out)
    print("source binary:", doc_bin)
    return 0


if __name__ == "__main__":
    sys.exit(main())
