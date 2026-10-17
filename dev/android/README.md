# dev/android/

Mobile (Android / iOS) build steps. Grouped here so every reproducible
build/package step lives under `dev/`. The authoritative Android flow is
documented in [`apps/acowork-mobile/README.md`](../../apps/acowork-mobile/README.md);
this folder adds the automations on top of it:

| script | what it does |
|---|---|
| [`build_mobile.sh`](build_mobile.sh) | **macOS / Linux — start here.** Same 5 steps and same overlays as the `.ps1`, in bash + npx + python3. No PowerShell needed. |
| [`build_mobile.ps1`](build_mobile.ps1) | **Windows — start here.** Toolchain discovery → `tauri android init` (if needed) → icon / permission / release overlays → verification → `tauri android build`. Debug or release, optional device install. |
| [`manifest-permissions.xml`](manifest-permissions.xml) | The committed `<uses-permission>` block (CAMERA for the QR scanner). Source of truth; the build script splices it into the generated manifest. |
| [`check_android_icon.py`](check_android_icon.py) | Asserts the generated `gen/` tree has a correct adaptive icon and the required permissions. Runs as step 4.5 of the build; also runnable standalone. Platform-agnostic, so it gates both build scripts identically. |
| [`sign_release.ps1`](sign_release.ps1) | Interactive release signing (keystore + `gradle.properties`) for a signed APK. Windows only. |

### Why the overlays exist

`src-tauri/gen/android/` is regenerated from scratch by `tauri android init` and
is gitignored, so nothing hand-written there survives. Three things do not
survive it, and each has already shipped a broken APK:

1. **The adaptive icon wrapper** (`mipmap-anydpi-v26/ic_launcher.xml`) — init
   copies only the `mipmap-*dpi` bitmaps. Without the wrapper, API 26+ launchers
   show a placeholder.
2. **Two Android-template drawables init leaves behind** —
   `drawable-v24/ic_launcher_foreground.xml` (stock robot) and
   `drawable/ic_launcher_background.xml` (green template). They collide by name
   with our artwork, and `@mipmap/ic_launcher_foreground` resolves to the
   drawable copy on API 24+, so the launcher drew the robot regardless. The
   build script deletes them.
3. **The CAMERA permission** — without it `ScanModal.tsx` (WebView
   `getUserMedia`) silently degrades to manual address entry.

`check_android_icon.py` is what keeps (1)–(3) from regressing silently; it
fails the build rather than shipping.

## build_mobile.sh (macOS / Linux)

The bash twin of `build_mobile.ps1`, for the same reason `dev/common/` ships
`.sh` twins: requiring a PowerShell install (~200MB, a second language on a
machine that already has bash) just to produce an APK is a real cost. Same
five steps, same overlays, same flags — only the shell differs.

```bash
# debug APK (default) — one command from a fresh clone
./dev/android/build_mobile.sh

# release APK, installed on the connected device
./dev/android/build_mobile.sh --release-apk --install

# nuke gen/ and prove the from-scratch path still works
./dev/android/build_mobile.sh --reinit
```

| flag | `.ps1` equivalent |
|---|---|
| `--release-apk` | `-ReleaseApk` |
| `--target <abi>` | `-Target <abi>` |
| `--install` | `-Install` |
| `--reinit` | `-Reinit` |
| `--skip-build` | `-SkipBuild` |

Finds a JDK ≥ 17 itself: explicit `$JAVA_HOME` wins, otherwise it probes the
macOS Android Studio JBR and the usual Homebrew / Debian / Ubuntu openjdk-17
locations, and takes the newest it finds. The Android SDK comes from
`$ANDROID_HOME`, then `$ANDROID_SDK_ROOT`, then
`~/Library/Android/sdk`, then `~/Android/Sdk`.

The two scripts are twins by contract, not by shared code — the XML edits use
each language's native DOM API (`.ps1` uses .NET `XmlDocument`, the `.sh` uses
a `python3` heredoc). When you change a step in one, change the other;
`check_android_icon.py` is the shared regression gate and will surface a
divergence as a failure on one host only.

## build_mobile.ps1 (Windows)

```powershell
# debug APK (default) — one command from a fresh clone
.\devndroiduild_mobile.ps1

# release APK, installed on the connected device
.\devndroiduild_mobile.ps1 -ReleaseApk -Install

# nuke gen/ and prove the from-scratch path still works
.\devndroiduild_mobile.ps1 -Reinit
```

Finds a JDK ≥ 17 itself (explicit `$env:JAVA_HOME` wins; otherwise it looks for
an Android Studio JBR, including on the drive the SDK lives on — a layout like
`D:\Android\Android Studio` is common and not under `Program Files`).

## sign_release.ps1 (Windows)

One-stop flow: locate `keytool` → create/reuse the keystore → write the
plaintext `gradle.properties` → build the signed release APK. Every location is
prompted with the manual-flow defaults (Enter = default):

- keystore: `$HOME/acowork-mobile.keystore`
- gradle.properties: `$HOME/.gradle/gradle.properties`

```powershell
# interactive (recommended first run)
.\dev\android\sign_release.ps1

# non-interactive overrides + auto-build
.\dev\android\sign_release.ps1 -Build
```

The script writes `acowork.release.{storeFile,storePassword,keyAlias,keyPassword}`
into `gradle.properties` (preserving unrelated lines) and the empty-safe signing
block in `src-tauri/gen/android/app/build.gradle.kts` reads them via
`findProperty`. Passwords are typed by the operator and stored in plaintext in
that local file only — never printed, never committed. If you point
`gradle.properties` inside the repo, the script warns and asks to confirm.

Flags: `-StorePath`, `-GradlePropsPath`, `-KeytoolPath`, `-Alias`, `-KeySize`,
`-Validity`, `-Build` / `-NoBuild`.

## Prerequisites (per machine)

- Android Studio (SDK + NDK), JDK 17+
- `rustup target add aarch64-linux-android`
- `ANDROID_HOME` / `NDK_HOME` exported
- `@tauri-apps/api` and the Rust `tauri` crate aligned on the same minor
- On macOS / Linux: `python3` on `PATH` for `build_mobile.sh` (it uses a
  `python3` heredoc for the manifest / gradle XML edits, and calls
  `check_android_icon.py`). Both build scripts also need `node` + `npx`.
- On Windows: **Developer Mode** must be on. The Tauri CLI symlinks the `.so`
  into `jniLibs`, and non-admin processes can't create symlinks without it
  (`Creation symbolic link is not allowed for this system`). `sign_release.ps1`
  probes this before building and offers to enable it via a UAC-elevated
  `reg add`. To do it manually:
  ```powershell
  Start-Process pwsh -Verb RunAs -Wait -ArgumentList '-NoProfile','-Command',
    'reg add "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\AppModelUnlock" /t REG_DWORD /f /v AllowDevelopmentWithoutDevLicense /d 1'
  ```
  The authoritative Android flow + the full pitfall list live in
  [`apps/acowork-mobile/README.md`](../../apps/acowork-mobile/README.md).
