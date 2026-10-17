#!/usr/bin/env bash
# build_mobile.sh - One-command Android APK build for ACowork Mobile (macOS / Linux).
#
# WHY THIS EXISTS
#   This is the bash twin of build_mobile.ps1, and it exists because the .ps1
#   is not a usable entry point on macOS or Linux: requiring a PowerShell
#   install (~200MB, a Microsoft runtime, a second language on a machine that
#   already has bash) just to produce an APK is a real cost, not a rounding
#   error. Both scripts implement the SAME contract — see the twin-note at the
#   bottom before changing one and not the other.
#
#   `tauri android init` regenerates src-tauri/gen/android/ from scratch and
#   that tree is gitignored. Three things the app needs do NOT survive a plain
#   init:
#
#     1. the adaptive launcher icon (mipmap-anydpi-v26/ic_launcher.xml + the
#        ic_launcher_background colour) - init only copies the flat mipmap
#        bitmaps, so on API 26+ some OEM launchers (Huawei EMUI) fall back to a
#        default placeholder instead of the real mark;
#     2. the CAMERA permission for the QR pairing scanner
#        (src/screens/boot/ScanModal.tsx uses WebView getUserMedia) - without it
#        the scanner silently degrades to manual address entry;
#     3. the hand-written signingConfigs block and the release
#        usesCleartextTraffic=true placeholder, both documented in the README.
#
#   So the rule this script enforces is: run the native commands, then re-apply
#   every committed overlay. The user never edits gen/ by hand.
#
# Usage:
#   ./dev/android/build_mobile.sh                 debug APK (default)
#   ./dev/android/build_mobile.sh --release-apk   release APK
#   ./dev/android/build_mobile.sh --target x86_64 build for another ABI
#   ./dev/android/build_mobile.sh --install       install onto the connected device
#   ./dev/android/build_mobile.sh --reinit        nuke gen/ and re-init from scratch
#   ./dev/android/build_mobile.sh --skip-build    apply overlays only (no Gradle run)
#
# Requires: bash, node + npx, python3, a JDK >= 17, and an Android SDK with
# platform-tools. On macOS: `brew install openjdk@17` and Android Studio (or
# sdkmanager) for the SDK. On Linux: openjdk-17-jdk + the commandline tools.

set -euo pipefail

# ── Args ──────────────────────────────────────────────────────────────────────
RELEASE_APK=0
INSTALL=0
REINIT=0
SKIP_BUILD=0
TARGET="aarch64"

while [ $# -gt 0 ]; do
    case "$1" in
        --release-apk|--release) RELEASE_APK=1 ;;
        --target)               TARGET="${2:?--target needs a value}"; shift ;;
        --install)              INSTALL=1 ;;
        --reinit)               REINIT=1 ;;
        --skip-build)           SKIP_BUILD=1 ;;
        -h|--help)              sed -n '2,35p' "$0"; exit 0 ;;
        *) echo "unknown option: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

# ── Paths ─────────────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/../.." && pwd)"
APP_DIR="$REPO_ROOT/apps/acowork-mobile"
GEN_DIR="$APP_DIR/src-tauri/gen/android"
RES_DIR="$GEN_DIR/app/src/main/res"
MANIFEST="$GEN_DIR/app/src/main/AndroidManifest.xml"
PERM_SRC="$SCRIPT_DIR/manifest-permissions.xml"
ICON_SRC="$APP_DIR/src-tauri/icons/android"

if [ -t 1 ]; then
    C_CYAN=$'\033[36m'; C_YELLOW=$'\033[33m'; C_GREEN=$'\033[32m'; C_RED=$'\033[31m'; C_OFF=$'\033[0m'
else
    C_CYAN=""; C_YELLOW=""; C_GREEN=""; C_RED=""; C_OFF=""
fi
info() { echo "${C_CYAN}$*${C_OFF}"; }
note() { echo "      $*"; }
warn() { echo "${C_YELLOW}$*${C_OFF}" >&2; }
die()  { echo "${C_RED}$*${C_OFF}" >&2; exit 1; }

echo "========================================"
echo "ACowork Mobile — Android build"
if [ "$RELEASE_APK" -eq 1 ]; then echo "Profile: release   Target: $TARGET"; else echo "Profile: debug     Target: $TARGET"; fi
echo "========================================"

# ── Toolchain discovery ──────────────────────────────────────────────────────
# Gradle 9 / AGP 8 refuse to run on a JDK < 17. An explicit JAVA_HOME wins (the
# operator chose it); otherwise Android Studio's bundled JBR is preferred,
# because that is the JDK guaranteed compatible with the installed SDK.
jdk_major() {
    # `java -version` writes to STDERR, hence the 2>&1.
    "$1/bin/java" -version 2>&1 | head -1 \
        | sed -E 's/.*"([0-9]+).*/\1/'
}

JDK_HOME=""
found_major=0

if [ -n "${JAVA_HOME:-}" ] && [ -x "$JAVA_HOME/bin/java" ]; then
    v="$(jdk_major "$JAVA_HOME" || true)"
    if [ -n "$v" ] && [ "$v" -ge 17 ]; then
        JDK_HOME="$JAVA_HOME"; found_major="$v"
    else
        die "JAVA_HOME is set to $JAVA_HOME but its JDK is ${v:-unknown}; Gradle 9 needs >= 17."
    fi
fi

if [ -z "$JDK_HOME" ]; then
    for r in \
        "/Applications/Android Studio.app/Contents/jbr" \
        "/usr/local/opt/openjdk@17/libexec/openjdk.jdk/Contents/Home" \
        "/usr/lib/jvm/java-17-openjdk-amd64" \
        "/usr/lib/jvm/java-17-openjdk" \
        "/usr/lib/jvm/default-java"
    do
        [ -x "$r/bin/java" ] || continue
        v="$(jdk_major "$r" || true)"
        [ -n "$v" ] || continue
        if [ "$v" -ge 17 ] && [ "$v" -gt "$found_major" ]; then
            JDK_HOME="$r"; found_major="$v"
        fi
    done
fi

if [ -z "$JDK_HOME" ]; then
    die "$(cat <<'MSG'
No JDK >= 17 found.
Gradle 9 (pulled by `tauri android init`) requires JDK 17+; an older JDK fails
with an unhelpful Gradle error. On macOS: `brew install openjdk@17`. On Debian /
Ubuntu: `sudo apt install openjdk-17-jdk`. Or export JAVA_HOME at an Android
Studio JBR / a current JDK.
MSG
)"
fi
export JAVA_HOME="$JDK_HOME"
export PATH="$JAVA_HOME/bin:$PATH"
note "[env] JDK $found_major: $JDK_HOME"

SDK=""
for c in "${ANDROID_HOME:-}" "${ANDROID_SDK_ROOT:-}" \
         "$HOME/Library/Android/sdk" "$HOME/Android/Sdk"
do
    [ -n "$c" ] && [ -d "$c" ] && { SDK="$c"; break; }
done
[ -n "$SDK" ] || die "Android SDK not found. Set \$ANDROID_HOME (or \$ANDROID_SDK_ROOT)."
export ANDROID_HOME="$SDK"
export ANDROID_SDK_ROOT="$SDK"
note "[env] Android SDK: $SDK"

# ── Step 1: gen/android exists? ──────────────────────────────────────────────
if [ "$REINIT" -eq 1 ] && [ -d "$GEN_DIR" ]; then
    warn "[1/5] --reinit: removing $GEN_DIR (all overlays are re-applied below)"
    rm -rf "$GEN_DIR"
fi

if [ ! -d "$GEN_DIR" ]; then
    info "[1/5] Running 'tauri android init' (gen/ is gitignored, so this is required once per clone)..."
    ( cd "$APP_DIR" && npx tauri android init )
else
    note "[1/5] gen/android already present."
fi

[ -f "$MANIFEST" ] || die "init did not produce $MANIFEST"

# ── Step 2: launcher icons ───────────────────────────────────────────────────
# `tauri icon` writes the adaptive layer into src-tauri/icons/android/; init
# copies only mipmap-*dpi bitmaps, so the wrapper has to be synced explicitly.
# dev/common/gen_icons.sh does the same thing when icons are regenerated; this
# is the build-time guarantee so a plain clone is always correct.
info "[2/5] Syncing adaptive launcher icon (mipmap-anydpi-v26 + ic_launcher_background)..."
[ -d "$ICON_SRC/mipmap-anydpi-v26" ] \
    || die "missing $ICON_SRC/mipmap-anydpi-v26 - run dev/common/gen_icons.sh to regenerate icons"

# Sync ALL of icons/android, not just the adaptive wrapper: the mipmap-*dpi
# BITMAPS are the actual artwork the launcher draws, and `tauri android init`
# drops its own copies in gen/res. Syncing only mipmap-anydpi-v26 left gen/res
# serving stale bitmaps, which is how a from-scratch build could ship a
# different icon than a previous one from the same committed sources.
cp -R "$ICON_SRC/mipmap-anydpi-v26" "$RES_DIR/"
cp -R "$ICON_SRC/values"            "$RES_DIR/"
for mip in "$ICON_SRC"/mipmap-*; do
    [ -d "$mip" ] || continue
    [ "$(basename "$mip")" = "mipmap-anydpi-v26" ] && continue
    cp -R "$mip" "$RES_DIR/"
done

# `tauri android init` ALSO drops two Android-template drawables that collide by
# name with our artwork:
#     drawable-v24/ic_launcher_foreground.xml  (the stock Android robot vector)
#     drawable/ic_launcher_background.xml     (the green template background)
#
# These are NOT inert leftovers. @mipmap/ic_launcher_foreground resolves to the
# drawable/ variant in preference to the mipmap-*dpi bitmap on API 24+, so the
# launcher kept rendering Tauri's robot; only the background colour (which the
# adaptive xml does read from our values/) came from us - producing a "yellow
# robot" instead of the ACowork mark. Delete both so the mipmap bitmaps are the
# only definition left. This is the whole reason the on-device icon ignored
# everything in gen/res before.
for junk in drawable-v24/ic_launcher_foreground.xml drawable/ic_launcher_background.xml; do
    if [ -f "$RES_DIR/$junk" ]; then
        rm -f "$RES_DIR/$junk"
        note "removed Tauri template: $junk"
    fi
done
for d in drawable-v24 drawable; do
    if [ -d "$RES_DIR/$d" ] && [ -z "$(ls -A "$RES_DIR/$d" 2>/dev/null)" ]; then
        rmdir "$RES_DIR/$d" 2>/dev/null || true
    fi
done
note "ok."

# ── Step 3: permissions ──────────────────────────────────────────────────────
# Splices the committed <uses-permission> / <uses-feature> block into the
# generated manifest. Idempotent: anything already present is skipped.
#
# A heredoc-built Python heredoc does the XML edit rather than sed, because
# both of these are easy to get wrong with sed and produce a manifest that
# LOOKS fine in a diff but fails aapt2:
#   - attributes must land in the android: namespace (the <manifest> tag
#     declares xmlns:android but no default namespace, so an unprefixed
#     `name=` is a different, invalid attribute);
#   - naive line-inserting breaks once <manifest> grows attributes of its own.
info "[3/5] Applying committed permissions (CAMERA/VIBRATE) to AndroidManifest.xml..."
python3 - "$MANIFEST" "$PERM_SRC" <<'PYEOF'
import sys
import xml.etree.ElementTree as ET

manifest_path, perm_src = sys.argv[1], sys.argv[2]
ANDROID_NS = "http://schemas.android.com/apk/res/android"

# Register the android: prefix so ET writes `android:name` instead of ns0:name.
ET.register_namespace("android", ANDROID_NS)


def q(attr: str) -> str:
    """Qualify an ATTRIBUTE name into the android: namespace.

    Element tags are unqualified — <manifest> only declares xmlns:android and
    no default xmlns, so ET parses `uses-permission` as a bare tag while
    `android:name` becomes `{uri}name`. Only attributes take the namespace,
    which is exactly why this helper is for attributes and the findall() calls
    below pass bare tag names.
    """
    return f"{{{ANDROID_NS}}}{attr}"


def android_name(el: ET.Element) -> str | None:
    return el.get(q("name"))


tree = ET.parse(manifest_path)
root = tree.getroot()
perm_root = ET.parse(perm_src).getroot()

added = 0
for tag in ("uses-permission", "uses-feature"):
    existing = {
        name for name in (android_name(el) for el in root.findall(tag)) if name
    }
    for src_elem in perm_root.findall(tag):
        name = android_name(src_elem)
        if not name or name in existing:
            continue
        node = ET.Element(tag)
        for k, v in src_elem.attrib.items():
            node.set(k, v)
        root.append(node)
        existing.add(name)
        added += 1

if added == 0:
    print("      already present, nothing to do.")
else:
    ET.indent(tree, space="    ")
    tree.write(manifest_path, encoding="utf-8", xml_declaration=True)
    print(f"      added {added} entr(ies) to AndroidManifest.xml.")
PYEOF

# ── Step 4: release-only build settings ──────────────────────────────────────
# The generated build.gradle.kts defaults release to cleartext=false, which
# breaks LAN http:// Gateway URLs. Only meaningful for release.
if [ "$RELEASE_APK" -eq 1 ]; then
    GRADLE="$GEN_DIR/app/build.gradle.kts"
    if [ -f "$GRADLE" ]; then
        python3 - "$GRADLE" <<'PYEOF'
import re
import sys

path = sys.argv[1]
with open(path, encoding="utf-8") as fh:
    src = fh.read()

MARKER = 'manifestPlaceholders["usesCleartextTraffic"]'
if MARKER in src:
    # Idempotency guard, same contract as build_mobile.ps1: `tauri android init`
    # can regenerate this file, and a re-run over an already-patched file must
    # not stack a second copy inside the release block.
    print("      release cleartext already enabled.")
    sys.exit(0)

pattern = re.compile(r'(?m)^([ \t]*)getByName\("release"\)[ \t]*\{[ \t]*$')


def repl(m: "re.Match[str]") -> str:
    indent = m.group(1)
    return f'{m.group(0)}\n{indent}    {MARKER} = "true"'


out = pattern.sub(repl, src)
if out == src:
    # Same contract as build_mobile.ps1: an unexpected gradle shape is a
    # warning, not a hard stop. The cleartext placeholder only matters for a
    # release build against a LAN http:// Gateway, and Tauri regenerates this
    # file anyway, so refusing to build here would be worse than building with
    # the default (cleartext=false) and letting check_android_icon.py + the
    # on-device smoke test be the gate.
    print('      no getByName("release") block found; left build.gradle.kts untouched.')
    sys.exit(0)
with open(path, "w", encoding="utf-8", newline="") as fh:
    fh.write(out)
print("      release cleartext enabled.")
PYEOF
    fi
fi

# ── Step 4.5: assert the overlays actually took ─────────────────────────────
# Cheap, and it is the difference between "the icon is wrong on my phone" and a
# build-time FAIL. Covers the three ways this has actually gone wrong: Tauri's
# stock robot drawable shadowing our bitmap, an opaque adaptive background
# refilling the knocked-out "A", and permissions dropped by a re-init.
info "[4.5/5] Verifying icon + permissions landed in gen/..."
if ! ( cd "$REPO_ROOT" && python3 dev/android/check_android_icon.py ); then
    die "gen/android failed the launcher-icon check (see above). Build aborted rather than shipping a wrong icon."
fi

# ── Step 5: build ────────────────────────────────────────────────────────────
if [ "$SKIP_BUILD" -eq 1 ]; then
    info "[5/5] --skip-build: overlays applied, no Gradle run."
    exit 0
fi

if [ "$RELEASE_APK" -eq 1 ]; then PROFILE="--release"; else PROFILE="--debug"; fi
info "[5/5] Building $PROFILE APK for $TARGET (this compiles Rust for Android; first run is slow)..."
( cd "$APP_DIR" && npx tauri android build --apk "$PROFILE" --target "$TARGET" )

APK="$(find "$GEN_DIR/app/build/outputs/apk" -name '*.apk' -type f 2>/dev/null \
       | xargs -r ls -t 2>/dev/null | head -1 || true)"
[ -n "$APK" ] || die "build reported success but no APK was found under app/build/outputs/apk"

APK_MB="$(awk -v b="$(wc -c < "$APK")" 'BEGIN{printf "%.1f", b/1048576}')"
echo
echo "${C_GREEN}APK: $APK  ($APK_MB MB)${C_OFF}"

# ── Optional: install ──────────────────────────────────────────────────────────
if [ "$INSTALL" -eq 1 ]; then
    ADB="$SDK/platform-tools/adb"
    [ -x "$ADB" ] || die "adb not found at $ADB (install the Android SDK platform-tools)"
    info "Installing on the connected device..."
    "$ADB" install -r "$APK"
    note "Launch: adb shell monkey -p com.acowork.mobile -c android.intent.category.LAUNCHER 1"
fi

# ── Twin-note ────────────────────────────────────────────────────────────────
# build_mobile.sh (this file) and build_mobile.ps1 implement the same contract:
# same 5 steps, same overlays, same flags. When you change a step here, change
# the twin too — check_android_icon.py is the shared regression gate, and it is
# platform-agnostic, so a divergence will show up as that check failing on one
# host and passing on the other.
