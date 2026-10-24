#!/usr/bin/env pwsh
# package_desktop_windows.ps1 - Build ACowork Desktop installer for Windows

param(
    [switch] $ReinstallOrt,
    [switch] $NoMirror
)

$ErrorActionPreference = "Stop"
$WorkspaceRoot = Split-Path -Parent (Split-Path -Parent (Split-Path -Parent $PSScriptRoot))
$DesktopDir = Join-Path $WorkspaceRoot "apps\acowork-desktop"
$OrtVersion = "1.22.0"
$OrtDir = Join-Path $WorkspaceRoot ".ort\onnxruntime-win-x64-$OrtVersion"
$OrtLibDir = Join-Path $OrtDir "lib"
$OrtDll = Join-Path $OrtLibDir "onnxruntime.dll"
$BinDir = Join-Path $DesktopDir "src-tauri\bin"

Write-Host "========================================" -ForegroundColor Cyan
Write-Host "ACowork Desktop Package (Windows)" -ForegroundColor Cyan
Write-Host "========================================" -ForegroundColor Cyan
Write-Host ""

if (-not (Test-Path $OrtDll) -or $ReinstallOrt) {
    $setupArgs = @("-Version", $OrtVersion)
    if ($ReinstallOrt) { $setupArgs += "-Reinstall" }
    if ($NoMirror) { $setupArgs += "-NoMirror" }
    & (Join-Path $PSScriptRoot "..\..\common\setup_ort.ps1") @setupArgs
}

if (-not (Test-Path $OrtDll)) {
    Write-Host "ONNX Runtime DLL not found: $OrtDll" -ForegroundColor Red
    exit 1
}

$env:ORT_LIB_LOCATION = $OrtLibDir
$env:ORT_DYLIB_PATH = $OrtDll
$env:ORT_PREFER_DYNAMIC_LINK = "1"
$env:PATH = "$OrtLibDir;$env:PATH"

if (-not (Test-Path $BinDir)) {
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
}
Copy-Item -Path $OrtDll -Destination (Join-Path $BinDir "onnxruntime.dll") -Force
Write-Host "Bundled ORT DLL: $OrtDll" -ForegroundColor Green

# Bundle LSP Relay binary (sibling of acowork-gateway.exe, ADR-019).
# The Gateway locates it via `current_exe().parent().join("acowork-lsp-relay.exe")`,
# so without this copy the Tauri app's Gateway supervisor will fail to spawn LSP
# and Monaco / runtime codebase tool will silently lose all LSP features.
$LspRelayBin = Join-Path $WorkspaceRoot "target\release\acowork-lsp-relay.exe"
if (Test-Path $LspRelayBin) {
    Copy-Item -Path $LspRelayBin -Destination (Join-Path $BinDir "acowork-lsp-relay.exe") -Force
    Write-Host "Bundled LSP Relay binary: $LspRelayBin" -ForegroundColor Green
} else {
    Write-Host "WARN: acowork-lsp-relay.exe not found at $LspRelayBin." -ForegroundColor Yellow
    Write-Host "      Run .\dev\common\build_core.ps1 (release) first." -ForegroundColor Yellow
    Write-Host "      Without it, Gateway startup will fail with:" -ForegroundColor Yellow
    Write-Host "        acowork-lsp-relay binary not found" -ForegroundColor Yellow
}

# Bundle Node Agent binary (sibling of acowork-gateway.exe, ADR-055 §6.11).
# The Gateway locates it via `current_exe().parent().join("acowork-node.exe")`;
# without this copy node 'local' never enrolls and agent installs fail with
# 503 "Node 'local' has never enrolled (offline)".
$NodeBin = Join-Path $WorkspaceRoot "target\release\acowork-node.exe"
if (Test-Path $NodeBin) {
    Copy-Item -Path $NodeBin -Destination (Join-Path $BinDir "acowork-node.exe") -Force
    Write-Host "Bundled Node Agent binary: $NodeBin" -ForegroundColor Green
} else {
    Write-Host "WARN: acowork-node.exe not found at $NodeBin." -ForegroundColor Yellow
    Write-Host "      Run .\dev\common\build_core.ps1 (release) first." -ForegroundColor Yellow
    Write-Host "      Without it, Gateway startup will fail with:" -ForegroundColor Yellow
    Write-Host "        acowork-node binary not found — node topology disabled" -ForegroundColor Yellow
}

# Bundle PM service binary (sibling of acowork-gateway.exe, ADR-064).
# The Gateway supervisor locates it via `current_exe().parent().join("acowork-pm.exe")`;
# without this copy the PM supervisor logs "acowork-pm binary not found" and
# `/api/pm/*` returns 503 (project management unavailable).
$PmBin = Join-Path $WorkspaceRoot "target\release\acowork-pm.exe"
if (Test-Path $PmBin) {
    Copy-Item -Path $PmBin -Destination (Join-Path $BinDir "acowork-pm.exe") -Force
    Write-Host "Bundled PM service binary: $PmBin" -ForegroundColor Green
} else {
    Write-Host "WARN: acowork-pm.exe not found at $PmBin." -ForegroundColor Yellow
    Write-Host "      Run .\dev\common\build_core.ps1 (release) first." -ForegroundColor Yellow
    Write-Host "      Without it, /api/pm/* returns 503 (PM unavailable)." -ForegroundColor Yellow
}

# Bundle Doc service binary (sibling of acowork-gateway.exe, ADR-064).
# Mirrors the PM service above: the Gateway supervisor locates it via
# `current_exe().parent().join("acowork-doc.exe")`; without this copy the Doc
# supervisor logs "acowork-doc binary not found" and `/api/doc/*` returns 503
# (document library unavailable).
$DocBin = Join-Path $WorkspaceRoot "target\release\acowork-doc.exe"
if (Test-Path $DocBin) {
    Copy-Item -Path $DocBin -Destination (Join-Path $BinDir "acowork-doc.exe") -Force
    Write-Host "Bundled Doc service binary: $DocBin" -ForegroundColor Green
} else {
    Write-Host "WARN: acowork-doc.exe not found at $DocBin." -ForegroundColor Yellow
    Write-Host "      Run .\dev\common\build_core.ps1 (release) first." -ForegroundColor Yellow
    Write-Host "      Without it, /api/doc/* returns 503 (Doc unavailable)." -ForegroundColor Yellow
}

# Bundle User service binary (sibling of acowork-gateway.exe, ADR-084).
# Mirrors the Doc service above: the Gateway supervisor locates it via
# `current_exe().parent().join("acowork-user.exe")`; without this copy the User
# supervisor logs "acowork-user binary not found" and every user-domain route
# (`/api/auth/*`, `/api/users/*`, `/api/user/*`) returns 503 (accounts,
# profiles and user chat unavailable).
$UserBin = Join-Path $WorkspaceRoot "target\release\acowork-user.exe"
if (Test-Path $UserBin) {
    Copy-Item -Path $UserBin -Destination (Join-Path $BinDir "acowork-user.exe") -Force
    Write-Host "Bundled User service binary: $UserBin" -ForegroundColor Green
} else {
    Write-Host "WARN: acowork-user.exe not found at $UserBin." -ForegroundColor Yellow
    Write-Host "      Run .\dev\common\build_core.ps1 (release) first." -ForegroundColor Yellow
    Write-Host "      Without it, /api/auth/*, /api/users/* and /api/user/* return 503." -ForegroundColor Yellow
}

# Bundle Memory service binary (sibling of acowork-gateway.exe, ADR-100).
# Mirrors the User service above: the Gateway supervisor locates it via
# `current_exe().parent().join("acowork-memory.exe")`; without this copy the
# memory supervisor logs "acowork-memory binary not found" and the memory
# panel plus the agent's `memory_store` / `memory_recall` tools are gone
# (`/api/memory/*` and `/api/agents/{id}/memory/*` return 503).
$MemoryBin = Join-Path $WorkspaceRoot "target\release\acowork-memory.exe"
if (Test-Path $MemoryBin) {
    Copy-Item -Path $MemoryBin -Destination (Join-Path $BinDir "acowork-memory.exe") -Force
    Write-Host "Bundled Memory service binary: $MemoryBin" -ForegroundColor Green
} else {
    Write-Host "WARN: acowork-memory.exe not found at $MemoryBin." -ForegroundColor Yellow
    Write-Host "      Run .\dev\common\build_core.ps1 (release) first." -ForegroundColor Yellow
    Write-Host "      Without it, /api/memory/* returns 503 (memory unavailable)." -ForegroundColor Yellow
}

Push-Location $DesktopDir
try {
    npm run tauri build
} finally {
    Pop-Location
}
