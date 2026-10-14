#!/usr/bin/env python3
"""Launcher-icon regression check for the Android build.

Run:  python dev/android/check_android_icon.py

WHY THIS EXISTS
  `tauri android init` emits two Android-template drawables that share a name
  with our artwork:

      drawable-v24/ic_launcher_foreground.xml   the stock Android robot vector
      drawable/ic_launcher_background.xml      the green template background

  `@mipmap/ic_launcher_foreground` resolves to the drawable/ copy in preference
  to the mipmap-*dpi bitmap on API 24+, so the launcher kept drawing Tauri's
  robot no matter what was in mipmap-anydpi-v26/ or values/. build_mobile.ps1
  deletes both files; this asserts they stayed deleted, so a future change that
  reorders the build steps cannot silently ship the robot again.

  The second assertion is the one that bit hardest: an earlier revision tinted
  the adaptive BACKGROUND brand yellow on the assumption that the foreground
  held only a bare "A". It does not - `tauri icon` renders the whole master
  (yellow tile + knocked-out A) into the foreground bitmap. Stacking two
  identical yellows filled the "A" back in and the launcher showed a flat
  yellow square. The background must therefore stay transparent.

Skipped (exit 0) when gen/android does not exist yet - it is gitignored, so a
fresh clone legitimately has no generated project to check.
"""

from __future__ import annotations

import sys
import xml.etree.ElementTree as ET
from pathlib import Path

ANDROID_NS = "http://schemas.android.com/apk/res/android"
REPO = Path(__file__).resolve().parents[2]
RES = REPO / "apps" / "acowork-mobile" / "src-tauri" / "gen" / "android" / "app" / "src" / "main" / "res"
ICONS_DIR = REPO / "apps" / "acowork-mobile" / "src-tauri" / "icons" / "android"
MANIFEST = RES.parent / "AndroidManifest.xml"

TAURI_TEMPLATES = (
    RES / "drawable-v24" / "ic_launcher_foreground.xml",
    RES / "drawable" / "ic_launcher_background.xml",
)


def fail(msg: str) -> None:
    print(f"FAIL: {msg}")
    sys.exit(1)


def main() -> int:
    if not RES.exists():
        print("SKIP: gen/android not generated yet (gitignored). "
              "Run dev/android/build_mobile.ps1 first.")
        return 0

    # 1. The Android template drawables must not exist.
    for path in TAURI_TEMPLATES:
        if path.exists():
            fail(f"{path.relative_to(REPO)} exists — Tauri's stock Android icon "
                 f"template shadows our mipmap bitmaps on API 24+, so the launcher "
                 f"would show the default robot. build_mobile.ps1 removes these.")

    # 2. The adaptive wrapper must be present and use a TRANSPARENT background.
    adaptive = RES / "mipmap-anydpi-v26" / "ic_launcher.xml"
    if not adaptive.exists():
        fail(f"{adaptive.relative_to(REPO)} missing — without the adaptive-icon "
             f"wrapper, API 26+ launchers fall back to a placeholder icon.")

    root = ET.parse(adaptive).getroot()
    # <foreground>/<background> are in NO namespace (Android only namespaces
    # their ATTRIBUTES), so match them bare. Searching with the android NS would
    # silently find nothing.
    bg = root.find("background")
    if bg is None:
        fail("adaptive-icon has no <background> element")
    target = bg.get(f"{{{ANDROID_NS}}}drawable", "")
    if "transparent" not in target:
        fail(f"adaptive background is {target!r}, expected a transparent drawable. "
             f"The foreground already carries the full artwork (yellow tile + "
             f"knocked-out A); a coloured background stacks a second copy of the "
             f"same yellow and fills the mark back in.")

    # 3. The permissions the app actually needs must have survived.
    if MANIFEST.exists():
        # Same as above: the elements are un-namespaced; only `android:name` is.
        perms = {e.get(f"{{{ANDROID_NS}}}name")
                 for e in ET.parse(MANIFEST).getroot().findall("uses-permission")}
        missing = {"android.permission.CAMERA", "android.permission.INTERNET"} - perms
        if missing:
            fail(f"AndroidManifest.xml is missing {sorted(missing)}. The QR pairing "
                 f"scanner (ScanModal.tsx) needs CAMERA; build_mobile.ps1 re-applies "
                 f"dev/android/manifest-permissions.xml after every init.")

    # 4. gen/res must not have drifted from the committed icons/. The artwork
    #    lives in the mipmap-*dpi bitmaps; a partial sync silently keeps
    #    `tauri android init`'s stale copies and ships the wrong icon.
    if RES.exists():
        stale = []
        for mip in sorted(ICONS_DIR.glob("mipmap-*dpi")):
            for png in sorted(mip.glob("*.png")):
                twin = RES / mip.name / png.name
                if not twin.exists() or twin.read_bytes() != png.read_bytes():
                    stale.append(f"{mip.name}/{png.name}")
        if stale:
            fail("gen/res icon bitmaps differ from the committed "
                 f"icons/android/ set: {stale}. build_mobile.ps1 syncs them "
                 "right before this check runs; if you see this, the sync was "
                 "skipped or icons/android/ itself changed.")
    else:
        print("  note: gen/res not present yet (skipping bitmap drift check)")

    print("OK: adaptive icon uses a transparent background, no Tauri template "
          "drawables present, required permissions declared.")
    return 0


if __name__ == "__main__":
    sys.exit(main())