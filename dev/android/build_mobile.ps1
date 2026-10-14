#!/usr/bin/env pwsh
# build_mobile.ps1 - One-command Android APK build for ACowork Mobile.
#
# WHY THIS EXISTS
#   `tauri android init` regenerates src-tauri/gen/android/ from scratch and that
#   tree is gitignored. Three things the app needs do NOT survive a plain init:
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
#   .\dev\android\build_mobile.ps1                 debug APK (default)
#   .\dev\android\build_mobile.ps1 -ReleaseApk      release APK
#   .\dev\android\build_mobile.ps1 -Target x86_64   build for another ABI
#   .\dev\android\build_mobile.ps1 -Install         install onto the connected device
#   .\dev\android\build_mobile.ps1 -Reinit         nuke gen/ and re-init from scratch
#   .\dev\android\build_mobile.ps1 -SkipBuild       apply overlays only (no Gradle run)

[CmdletBinding()]
param(
    [switch]$ReleaseApk,
    [string]$Target = "aarch64",
    [switch]$Install,
    [switch]$Reinit,
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Split-Path -Parent (Split-Path -Parent $ScriptDir)
$AppDir    = Join-Path $RepoRoot "apps\acowork-mobile"
$GenDir    = Join-Path $AppDir "src-tauri\gen\android"
$ResDir    = Join-Path $GenDir "app\src\main\res"
$Manifest  = Join-Path $GenDir "app\src\main\AndroidManifest.xml"
$PermSrc   = Join-Path $ScriptDir "manifest-permissions.xml"
$IconSrc   = Join-Path $AppDir "src-tauri\icons\android"

# NOTE: the profile switches are NOT named -Debug/-Release: PowerShell already
# binds -Debug as a common parameter, and a CmdletBinding param() would collide
# with it. -ReleaseApk is unambiguous.
$IsRelease = $ReleaseApk

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "ACowork Mobile — Android build" -ForegroundColor Cyan
Write-Host "Profile: $(if ($IsRelease) { 'release' } else { 'debug' })   Target: $Target" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan

# ── Toolchain discovery ──────────────────────────────────────────────────────
# Gradle 9 / AGP 8 refuse to run on a JDK < 17, and the JBR shipped with a
# current Android Studio is the JDK most likely to already be on the machine.
function Resolve-Jdk($sdk) {
    # Candidate roots in preference order. An explicit JAVA_HOME wins (the
    # operator chose it); otherwise Android Studio's JBR is preferred because it
    # is the JDK that is guaranteed compatible with the installed SDK, and a
    # stray IDE-bundled JBR (IntelliJ ships an old one) is only a fallback.
    $cands = @()
    if ($env:JAVA_HOME) { $cands += @{ Home = $env:JAVA_HOME; Explicit = $true } }
    # Android Studio is frequently NOT under Program Files (a common layout is a
    # dedicated D:\Android\Android Studio), so probe the drives the SDK lives on
    # plus the usual install roots before falling back to a bare Adoptium.
    $roots = @(
        "${env:ProgramFiles}\Android\Android Studio\jbr",
        "${env:LOCALAPPDATA}\Programs\Android Studio\jbr",
        "${env:ProgramFiles}\Eclipse Adoptium"
    )
    # $sdk is a PARAMETER, not an ambient global: a function body reads it from
    # its own scope, and depending on assignment order made the JDK search
    # silently find nothing under Windows PowerShell.
    if ($sdk) {
        foreach ($d in @($sdk, (Split-Path -Parent $sdk))) {
            if (-not $d) { continue }
            $roots += (Join-Path $d "Android Studio\jbr")
            $roots += (Join-Path $d "jbr")
        }
    }
    foreach ($r in $roots) { $cands += @{ Home = $r; Explicit = $false } }

    $found = @()
    foreach ($c in $cands) {
        if (-not $c.Home) { continue }
        $exe = Join-Path $c.Home "bin\java.exe"
        if (-not (Test-Path $exe)) { continue }
        # `java -version` prints to STDERR. With $ErrorActionPreference =
        # "Stop" a native command touching stderr raises a TERMINATING error,
        # so the version line is never parsed and every JDK looks like
        # "not found" - which is exactly what happens on Windows PowerShell
        # 5.1 even though the JDK is perfectly good. Demote to Continue for
        # just this probe, then restore.
        $prevEap = $ErrorActionPreference
        try {
            $ErrorActionPreference = "Continue"
            $v = (& $exe -version 2>&1 | Select-Object -First 1) -replace '.*"(\d+).*', '$1'
        } catch {
            $v = $null
        } finally {
            $ErrorActionPreference = $prevEap
        }
        if ($v -and [int]$v -ge 17) {
            if ($c.Explicit) { return @{ Home = $c.Home; Version = $v } }
            $found += @{ Home = $c.Home; Version = $v; Major = [int]$v }
        }
    }
    # No explicit JAVA_HOME: take the newest of the discovered JBRs.
    if ($found.Count -gt 0) {
        return ($found | Sort-Object Major -Descending | Select-Object -First 1)
    }
    return $null
}

$sdk = if ($env:ANDROID_HOME) { $env:ANDROID_HOME }
       elseif ($env:ANDROID_SDK_ROOT) { $env:ANDROID_SDK_ROOT }
       else { Join-Path $env:LOCALAPPDATA "Android\Sdk" }
if (-not (Test-Path $sdk)) { throw "Android SDK not found. Set `$env:ANDROID_HOME." }
$env:ANDROID_HOME = $sdk
$env:ANDROID_SDK_ROOT = $sdk
Write-Host "[env] Android SDK: $sdk" -ForegroundColor DarkGray

# Resolved AFTER the SDK, because the JDK search walks the SDK's own drive to
# find an Android Studio install (they usually sit side by side).
$jdk = Resolve-Jdk $sdk
if (-not $jdk) {
    throw @"
No JDK >= 17 found.
Gradle 9 (pulled by `tauri android init`) requires JDK 17+; an older JDK fails
with an unhelpful Gradle error. Install a current JDK, or point JAVA_HOME at an
Android Studio JBR:
    `$env:JAVA_HOME = "C:\Program Files\Android\Android Studio\jbr"
"@
}
$env:JAVA_HOME = $jdk.Home
Write-Host "[env] JDK $($jdk.Version): $($jdk.Home)" -ForegroundColor DarkGray

# ── Step 1: gen/android exists? ──────────────────────────────────────────────
if ($Reinit -and (Test-Path $GenDir)) {
    Write-Host "[1/5] -Reinit: removing $GenDir (all overlays are re-applied below)" -ForegroundColor Yellow
    Remove-Item $GenDir -Recurse -Force
}

if (-not (Test-Path $GenDir)) {
    Write-Host "[1/5] Running 'tauri android init' (gen/ is gitignored, so this is required once per clone)..." -ForegroundColor Cyan
    Push-Location $AppDir
    try { npx tauri android init } finally { Pop-Location }
} else {
    Write-Host "[1/5] gen/android already present." -ForegroundColor DarkGray
}

if (-not (Test-Path $Manifest)) { throw "init did not produce $Manifest" }

# ── Step 2: launcher icons ───────────────────────────────────────────────────
# `tauri icon` writes the adaptive layer into src-tauri/icons/android/; init
# copies only mipmap-*dpi bitmaps, so the wrapper has to be synced explicitly.
# dev/common/gen_icons.{ps1,sh} does the same thing when icons are regenerated;
# this is the build-time guarantee so a plain clone is always correct.
Write-Host "[2/5] Syncing adaptive launcher icon (mipmap-anydpi-v26 + ic_launcher_background)..." -ForegroundColor Cyan
if (-not (Test-Path (Join-Path $IconSrc "mipmap-anydpi-v26"))) {
    throw "missing $($IconSrc)\mipmap-anydpi-v26 - run dev\common\gen_icons.ps1 to regenerate icons"
}
# Sync ALL of icons/android, not just the adaptive wrapper: the mipmap-*dpi
# BITMAPS are the actual artwork the launcher draws, and `tauri android init`
# drops its own copies in gen/res. Syncing only mipmap-anydpi-v26 left gen/res
# serving stale bitmaps, which is how a from-scratch build could ship a
# different icon than a previous one from the same committed sources.
Copy-Item (Join-Path $IconSrc "mipmap-anydpi-v26") $ResDir -Recurse -Force
Copy-Item (Join-Path $IconSrc "values") $ResDir -Recurse -Force
foreach ($mip in Get-ChildItem $IconSrc -Directory -Filter "mipmap-*") {
    if ($mip.Name -eq "mipmap-anydpi-v26") { continue }
    Copy-Item $mip.FullName $ResDir -Recurse -Force
}

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
foreach ($junk in @("drawable-v24\ic_launcher_foreground.xml", "drawable\ic_launcher_background.xml")) {
    $p = Join-Path $ResDir $junk
    if (Test-Path $p) {
        Remove-Item $p -Force
        Write-Host "      removed Tauri template: $junk" -ForegroundColor DarkGray
    }
}
foreach ($dir in @("drawable-v24", "drawable")) {
    $d = Join-Path $ResDir $dir
    if ((Test-Path $d) -and -not (Get-ChildItem $d -Force)) {
        Remove-Item $d -Force
    }
}
Write-Host "      ok." -ForegroundColor DarkGray


# ── Step 3: permissions ──────────────────────────────────────────────────────
# Splices the committed <uses-permission> block in after the <manifest> tag.
# Idempotent: a marker comment guards against double-insertion.
Write-Host "[3/5] Applying committed permissions (CAMERA/VIBRATE) to AndroidManifest.xml..." -ForegroundColor Cyan
$ANDROID_NS = "http://schemas.android.com/apk/res/android"

# Splice our committed permission block into the generated manifest.
#
# Two things are easy to get wrong here and both produce a manifest that
# LOOKS fine in a diff but fails aapt2:
#   - attributes must land in the android: namespace (the <manifest> tag
#     declares xmlns:android but no default namespace, so an unprefixed
#     `name=` is a different, invalid attribute);
#   - SetAttribute(local, ns, value) rejects a prefixed name in the first
#     slot ("':' cannot be included in a name"), so the prefix is split into
#     CreateAttribute(prefix, local, ns) instead.
function Add-ManifestElement($doc, $tag, $srcElem) {
    # CreateElement($tag) with NO namespace: the parent <manifest> already
    # declares xmlns:android, so an element in the android namespace here
    # forces .NET to re-declare xmlns on every inserted line. Only the
    # ATTRIBUTES need the namespace prefix (CreateAttribute takes the prefix
    # separately, because SetAttribute rejects a "android:name" first arg).
    $node = $doc.CreateElement($tag)
    foreach ($a in $srcElem.Attributes) {
        $attr = $doc.CreateAttribute($a.Prefix, $a.LocalName, $ANDROID_NS)
        $attr.Value = $a.Value
        [void]$node.Attributes.Append($attr)
    }
    [void]$doc.manifest.AppendChild($node)
}

$xml = [xml](Get-Content $Manifest -Raw)
$permXml = [xml](Get-Content $PermSrc -Raw)
$existingPerm = @($xml.manifest.'uses-permission' | ForEach-Object { $_.name })
$existingFeat = @($xml.manifest.'uses-feature'    | ForEach-Object { $_.name })
$added = 0

foreach ($p in $permXml.manifest.'uses-permission') {
    if ($existingPerm -notcontains $p.name) {
        Add-ManifestElement $xml "uses-permission" $p
        $added++
    }
}
foreach ($f in $permXml.manifest.'uses-feature') {
    if ($existingFeat -notcontains $f.name) {
        Add-ManifestElement $xml "uses-feature" $f
        $added++
    }
}
if ($added -eq 0) {
    Write-Host "      already present, nothing to do." -ForegroundColor DarkGray
} else {
    # [void] on Save(): XmlDocument.Save returns void but PowerShell still
    # surfaces the method's output stream without it (visible as bare
    # "android.permission.CAMERA" lines).
    [void]$xml.Save($Manifest)
    Write-Host "      added $added entr(ies) to AndroidManifest.xml." -ForegroundColor DarkGray
}

# ── Step 4: release-only build settings ──────────────────────────────────────
# The generated build.gradle.kts defaults release to cleartext=false, which
# breaks LAN http:// Gateway URLs. Only meaningful for release.
if ($IsRelease) {
    $gradle = Join-Path $GenDir "app\build.gradle.kts"
    if (Test-Path $gradle) {
        $g = Get-Content $gradle -Raw
        if ($g -notmatch 'manifestPlaceholders\["usesCleartextTraffic"\]') {
            $block = @'
                getByName("release") {
                    manifestPlaceholders["usesCleartextTraffic"] = "true"
'@
            $g = [regex]::Replace($g, '(?m)^(\s*)getByName\("release"\)\s*\{\s*$', {
                param($m)
                $indent = $m.Groups[1].Value
                "$($m.Value)`n$indent    manifestPlaceholders[`"usesCleartextTraffic`"] = `"true`""
            })
            Set-Content $gradle $g -NoNewline
            Write-Host "      release cleartext enabled." -ForegroundColor DarkGray
        }
    }
}

# ── Step 4.5: assert the overlays actually took ─────────────────────────────
# Cheap, and it is the difference between "the icon is wrong on my phone" and a
# build-time FAIL. Covers the three ways this has actually gone wrong: Tauri's
# stock robot drawable shadowing our bitmap, an opaque adaptive background
# refilling the knocked-out "A", and permissions dropped by a re-init.
Write-Host "[4.5/5] Verifying icon + permissions landed in gen/..." -ForegroundColor Cyan
& python (Join-Path $ScriptDir "check_android_icon.py")
if ($LASTEXITCODE -ne 0) {
    throw "gen/android failed the launcher-icon check (see above). Build aborted rather than shipping a wrong icon."
}

# ── Step 5: build ────────────────────────────────────────────────────────────
if ($SkipBuild) {
    Write-Host "[5/5] -SkipBuild: overlays applied, no Gradle run." -ForegroundColor Cyan
    return
}

$profile = if ($IsRelease) { "--release" } else { "--debug" }
Write-Host "[5/5] Building $profile APK for $Target (this compiles Rust for Android; first run is slow)..." -ForegroundColor Cyan
Push-Location $AppDir
try {
    npx tauri android build --apk $profile --target $Target
} finally { Pop-Location }

$apk = Get-ChildItem (Join-Path $GenDir "app\build\outputs\apk") -Recurse -Filter *.apk -ErrorAction SilentlyContinue |
       Sort-Object LastWriteTime -Descending | Select-Object -First 1
if (-not $apk) { throw "build reported success but no APK was found under app\build\outputs\apk" }
Write-Host ""
Write-Host "APK: $($apk.FullName)  ($([math]::Round($apk.Length/1MB,1)) MB)" -ForegroundColor Green

# ── Optional: install ──────────────────────────────────────────────────────────
if ($Install) {
    $adb = Join-Path $sdk "platform-tools\adb.exe"
    if (-not (Test-Path $adb)) { throw "adb not found at $adb" }
    Write-Host "Installing on the connected device..." -ForegroundColor Cyan
    & $adb install -r $apk.FullName
    Write-Host "Launch: adb shell monkey -p com.acowork.mobile -c android.intent.category.LAUNCHER 1" -ForegroundColor DarkGray
}