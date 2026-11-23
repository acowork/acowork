"""Put build/hosted-bridge.js into each Craft adapter's injected bridge.

The six adapters (`apps/<app>-acowork/src/ui.rs`) serve the same script with
three per-app differences: the app id, its theme command, and (VectorCraft
only) the download-capture poll. This rewrites the `<script data-acowork-bridge>`
block from one template so the six cannot drift.

    python build/apply-bridge.py [app ...]
"""

import io
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
TEMPLATE = (pathlib.Path(__file__).resolve().parent / "hosted-bridge.js").read_text(encoding="utf-8")
APPS = ["wordcraft", "gridcraft", "deckcraft", "pdfcraft", "vectorcraft", "lightcraft"]
BLOCK = re.compile(r'const BRIDGE_SCRIPT: &str = r##"<script data-acowork-bridge=".*?</script>"##;', re.S)


def main() -> int:
    apps = sys.argv[1:] or APPS
    for app in apps:
        path = ROOT / f"{app}-app/apps/{app}-acowork/src/ui.rs"
        source = io.open(path, encoding="utf-8").read()
        match = BLOCK.search(source)
        if not match:
            print(f"{app}: NO BRIDGE BLOCK in {path}")
            continue
        current = match.group(0)
        theme = re.search(r"  function themeCommand\(dark\)\{(.*?)\}\n", current, re.S)
        capture = re.search(r"(  function captureDownloads\(\)\{.*?\n  \})\n", current, re.S)
        body = (
            TEMPLATE.replace("__APP__", app)
            .replace("__THEME__", theme.group(1) if theme else 'return null;')
            .replace("__CAPTURE__", capture.group(1) if capture else "  function captureDownloads(){}")
        )
        source = source[: match.start()] + 'const BRIDGE_SCRIPT: &str = r##"' + body + '"##;' + source[match.end() :]
        io.open(path, "w", encoding="utf-8", newline="").write(source)
        print(f"{app}: bridge {'+capture' if capture else ''} written")
    return 0


if __name__ == "__main__":
    sys.exit(main())