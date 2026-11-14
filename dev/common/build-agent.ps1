#!/usr/bin/env pwsh
<#
.SYNOPSIS
    ACowork Agent Package Builder and Signer (PowerShell)
.DESCRIPTION
    Builds and signs .agent packages for one or all example agents.
    Equivalent to dev/common/build-agent.sh for Windows.
.PARAMETER AgentDir
    Path to a single agent directory. If omitted with -All, builds all agents.
.PARAMETER OutputDir
    Output directory for .agent packages (default: agents/agent-packages)
.PARAMETER All
    Build all agents in the agents/ directory.
.EXAMPLE
    .\dev\common\build-agent.ps1 agents\senior-engineer-agent
.EXAMPLE
    .\dev\common\build-agent.ps1 -All
#>

param(
    [Parameter(Position = 0)]
    [string]$AgentDir,

    [Parameter()]
    [string]$OutputDir,

    [Parameter()]
    [switch]$All
)

$ErrorActionPreference = "Stop"
$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Split-Path -Parent (Split-Path -Parent $ScriptDir)

# ── Defaults ────────────────────────────────────────────────────────

if (-not $OutputDir) {
    $OutputDir = Join-Path $ProjectRoot "agents\agent-packages"
}
$KeyDir = Join-Path $ProjectRoot "agents\.signing-keys"

# ── Colors ──────────────────────────────────────────────────────────

function Write-Success { Write-Host $args -ForegroundColor Green }
function Write-Error_ { Write-Host $args -ForegroundColor Red }
function Write-Warn { Write-Host $args -ForegroundColor Yellow }
function Write-Info { Write-Host $args -ForegroundColor Cyan }
function Write-Subtle { Write-Host $args -ForegroundColor Gray }

# ── Resolve agent directories ───────────────────────────────────────

function Get-AgentDirs {
    $agentsDir = Join-Path $ProjectRoot "agents"
    Get-ChildItem -Path $agentsDir -Directory |
        Where-Object { Test-Path (Join-Path $_.FullName "manifest.toml") } |
        ForEach-Object { $_.FullName }
}

$agentDirs = @()
if ($All) {
    $agentDirs = @(Get-AgentDirs)
    if ($agentDirs.Count -eq 0) {
        Write-Error_ "No agent directories found with manifest.toml in agents/"
        exit 1
    }
} elseif ($AgentDir) {
    $agentDirs = @($AgentDir)
} else {
    Write-Error_ "Error: Specify -AgentDir or -All"
    Write-Host "Usage: .\dev\common\build-agent.ps1 <agent-dir>"
    Write-Host "       .\dev\common\build-agent.ps1 -All"
    exit 1
}

# ── Ensure output directory ─────────────────────────────────────────

New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null

# ── Helper: read TOML value ─────────────────────────────────────────

function Get-TomlValue {
    param([string]$Path, [string]$Key)
    $content = Get-Content -Raw -Path $Path
    if ($content -match "$Key\s*=\s*`"([^`"]+)`"") {
        return $matches[1]
    }
    return $null
}

# ── Build a single agent ────────────────────────────────────────────

function Build-Agent {
    param([string]$AgentDir)

    $manifestPath = Join-Path $AgentDir "manifest.toml"
    if (-not (Test-Path $manifestPath)) {
        Write-Error_ "Error: manifest.toml not found in $AgentDir"
        return $false
    }

    # Read agent metadata
    $agentId = Get-TomlValue -Path $manifestPath -Key "agent_id"
    $agentVersion = Get-TomlValue -Path $manifestPath -Key "version"

    if (-not $agentId) {
        Write-Error_ "Error: Could not read agent_id from manifest.toml"
        return $false
    }
    if (-not $agentVersion) {
        Write-Warn "Warning: No version found, using '1.0.0'"
        $agentVersion = "1.0.0"
    }

    Write-Info "========================================"
    Write-Info "Building: $agentId v$agentVersion"
    Write-Info "========================================"
    Write-Subtle "  Agent Dir : $AgentDir"
    Write-Subtle "  Output Dir: $OutputDir"
    Write-Subtle "  Key Dir   : $KeyDir"
    Write-Host ""

    # ── [1/4] Create unsigned .agent ZIP ──

    # Same pin as build-agent.sh: timestamp the archive at the last commit that
    # touched this agent, so the two builders on the two platforms produce one
    # byte string for one commit.
    if (-not $env:SOURCE_DATE_EPOCH) {
        $pin = & git -C $ProjectRoot log -1 --format=%ct -- $AgentDir 2>$null
        if ($LASTEXITCODE -eq 0 -and $pin) { $env:SOURCE_DATE_EPOCH = "$pin".Trim() }
    }

    $unsignedPkg = Join-Path $OutputDir "$agentId-$agentVersion.unsigned.agent"
    $signedPkg   = Join-Path $OutputDir "$agentId.agent"

    Write-Subtle "[1/4] Creating unsigned package..."

    # Same packer as build-agent.sh. This used to be Compress-Archive, which
    # wrote its own entry order, its own compression and backslash path
    # separators — so a package built here and one built from Git Bash were
    # different bytes for one source tree, and the signature made that
    # visible. `acowork-pack` is the only writer of the archive now.
    Remove-Item -Force -ErrorAction SilentlyContinue $unsignedPkg
    Push-Location (Join-Path $ProjectRoot "core")
    try {
        & cargo run --release -q -p acowork-sign --bin acowork-pack -- `
            --agent-dir $(if ([IO.Path]::IsPathRooted($AgentDir)) { $AgentDir } else { Join-Path $ProjectRoot $AgentDir }) `
            --output $unsignedPkg
        if ($LASTEXITCODE -ne 0) {
            Write-Error "acowork-pack failed (exit $LASTEXITCODE)"
            exit 1
        }
    } finally {
        Pop-Location
    }
    Write-Subtle "  Created: $unsignedPkg"
    Write-Host ""

    # ── [2/4] Generate signing keys if needed ──

    $privateKeyPath = Join-Path $KeyDir "developer.key"
    $pubKeyPath     = Join-Path $KeyDir "developer.pub"
    $certPath       = Join-Path $KeyDir "developer.cert"

    if (-not (Test-Path $privateKeyPath)) {
        Write-Warn "[2/4] Generating signing keys..."
        New-Item -ItemType Directory -Force -Path $KeyDir | Out-Null

        Push-Location (Join-Path $ProjectRoot "core")
        try {
            cargo run --release --bin acowork-keygen -- --type developer --output-dir $KeyDir 2>&1 |
                Where-Object { $_ -notmatch "Compiling|Finished|Running|warning" } |
                ForEach-Object { Write-Subtle "  $_" }
        } finally {
            Pop-Location
        }
        Write-Subtle "  Keys generated in: $KeyDir"
    } else {
        Write-Subtle "[2/4] Signing keys already exist"
    }
    Write-Host ""

    # ── [3/4] Sign the package ──

    Write-Warn "[3/4] Signing package..."

    Push-Location (Join-Path $ProjectRoot "core")
    try {
        cargo run --release --bin acowork-sign -- `
            --input $unsignedPkg `
            --key $KeyDir `
            --output $signedPkg `
            --key-type developer 2>&1 |
            Where-Object { $_ -notmatch "Compiling|Finished|Running|warning" } |
            ForEach-Object { Write-Subtle "  $_" }
    } finally {
        Pop-Location
    }
    Write-Subtle "  Signed: $signedPkg"
    Write-Host ""

    # ── [4/4] Verify the signature ──

    Write-Warn "[4/4] Verifying signature..."

    Push-Location (Join-Path $ProjectRoot "core")
    try {
        $verifyResult = cargo run --release --bin acowork-verify -- $signedPkg 2>&1 |
            Where-Object { $_ -notmatch "Compiling|Finished|Running|warning" }
        $verifyResult | ForEach-Object { Write-Subtle "  $_" }
    } finally {
        Pop-Location
    }
    Write-Host ""

    # ── Cleanup unsigned package ──

    Remove-Item -Force -ErrorAction SilentlyContinue $unsignedPkg

    # ── Summary ──

    $pkgSize = (Get-Item $signedPkg).Length
    $sizeDisplay = if ($pkgSize -gt 1MB) {
        "{0:N1} MB" -f ($pkgSize / 1MB)
    } else {
        "{0:N1} KB" -f ($pkgSize / 1KB)
    }

    Write-Info "========================================"
    Write-Info "Build Complete: $agentId"
    Write-Info "========================================"
    Write-Subtle "  Package: $signedPkg"
    Write-Subtle "  Size   : $sizeDisplay"
    Write-Host ""

    # ── Show package contents ──

    Write-Subtle "Package Contents:"
    try {
        Add-Type -AssemblyName System.IO.Compression.FileSystem
        $zip = [System.IO.Compression.ZipFile]::OpenRead($signedPkg)
        foreach ($entry in $zip.Entries) {
            $entrySize = if ($entry.Length -gt 1KB) { "{0,7:N1} KB" -f ($entry.Length / 1KB) } else { "{0,7} B" -f $entry.Length }
            Write-Subtle "  $entrySize  $($entry.FullName)"
        }
        $zip.Dispose()
    } catch {
        Write-Warn "  (Could not list package contents)"
    }
    Write-Host ""

    return $true
}

# ── Main ────────────────────────────────────────────────────────────

Write-Info "ACowork Agent Package Builder (PowerShell)"
Write-Info ""

$successCount = 0
$failCount = 0

foreach ($dir in $agentDirs) {
    if (Build-Agent -AgentDir $dir) {
        $successCount++
    } else {
        $failCount++
    }
}

Write-Info "========================================"
Write-Info "All builds complete!"
Write-Info "========================================"
Write-Success "  Successful: $successCount"
if ($failCount -gt 0) {
    Write-Error_ "  Failed    : $failCount"
}
