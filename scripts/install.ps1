#Requires -Version 5.1
<#
.SYNOPSIS
    Install the TokenGauge TUI on Windows 10/11.

.DESCRIPTION
    Downloads the latest (or a specified) TokenGauge Windows release, installs
    tokengauge-tui.exe into a user-writable directory, adds that directory to
    your user PATH, and seeds a default config at %APPDATA%\tokengauge\config.toml.

    Only the TUI is supported on Windows - the Waybar module, GTK4 popover, and
    KDE Plasma applet are Linux-only. Cost/token data comes from ccusage (needs
    Node.js or Bun on PATH). Usage limits need a `codexbar` binary, which upstream
    CodexBar does not ship for Windows; without it the TUI still runs and shows
    ccusage data (limit providers just report an error).

.PARAMETER Repo
    GitHub repo to install from. Default: Arzaroth/TokenGauge.

.PARAMETER Version
    Release tag to install (e.g. v0.8.0). Default: the latest release.

.PARAMETER InstallDir
    Where to place tokengauge-tui.exe. Default: %LOCALAPPDATA%\TokenGauge\bin.

.PARAMETER NoPath
    Do not modify the user PATH.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File scripts\install.ps1

.EXAMPLE
    irm https://raw.githubusercontent.com/Arzaroth/TokenGauge/master/scripts/install.ps1 | iex
#>
[CmdletBinding()]
param(
    [string]$Repo = $(if ($env:TOKENGAUGE_REPO) { $env:TOKENGAUGE_REPO } else { 'Arzaroth/TokenGauge' }),
    [string]$Version = '',
    [string]$InstallDir = $(if ($env:TOKENGAUGE_INSTALL_DIR) { $env:TOKENGAUGE_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'TokenGauge\bin' }),
    [switch]$NoPath
)

$ErrorActionPreference = 'Stop'
# The progress bar makes Invoke-WebRequest downloads very slow on Windows PowerShell 5.1.
$ProgressPreference = 'SilentlyContinue'
# GitHub requires a User-Agent; older PowerShell defaults to TLS 1.0.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
$Headers = @{ 'User-Agent' = 'TokenGauge-Installer' }

function Write-Info    { param([string]$Msg) Write-Host $Msg -ForegroundColor Cyan }
function Write-Good    { param([string]$Msg) Write-Host $Msg -ForegroundColor Green }
function Write-Warned  { param([string]$Msg) Write-Host $Msg -ForegroundColor Yellow }

# ---------------------------------------------------------------------------
# Resolve the release tag
# ---------------------------------------------------------------------------
if ([string]::IsNullOrWhiteSpace($Version)) {
    Write-Info "Fetching latest release for $Repo"
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $Headers
    $Version = $release.tag_name
}
if ([string]::IsNullOrWhiteSpace($Version)) {
    throw "Could not determine a release tag for $Repo"
}
Write-Info "Installing TokenGauge $Version"

# ---------------------------------------------------------------------------
# Download + extract the Windows zip
# ---------------------------------------------------------------------------
$asset = "tokengauge-$Version-windows-x86_64.zip"
$url   = "https://github.com/$Repo/releases/download/$Version/$asset"
$tmp   = Join-Path ([System.IO.Path]::GetTempPath()) ("tokengauge-install-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    $zipPath = Join-Path $tmp $asset
    Write-Info "Downloading $url"
    try {
        Invoke-WebRequest -Uri $url -OutFile $zipPath -Headers $Headers
    } catch {
        throw "Failed to download $asset. This release may predate Windows support - " +
              "install a newer release with -Version, or build from source: " +
              "cargo build --release -p tokengauge-tui"
    }

    Expand-Archive -Path $zipPath -DestinationPath $tmp -Force

    $exe = Get-ChildItem -Path $tmp -Recurse -Filter 'tokengauge-tui.exe' | Select-Object -First 1
    if (-not $exe) {
        throw "tokengauge-tui.exe not found inside $asset"
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Path $exe.FullName -Destination (Join-Path $InstallDir 'tokengauge-tui.exe') -Force
    Write-Good "Installed tokengauge-tui.exe to $InstallDir"
} finally {
    Remove-Item -Path $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

# ---------------------------------------------------------------------------
# Add the install dir to the user PATH
# ---------------------------------------------------------------------------
if (-not $NoPath) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries  = @()
    if ($userPath) { $entries = $userPath -split ';' | Where-Object { $_ -ne '' } }
    if ($entries -notcontains $InstallDir) {
        $newPath = (@($entries + $InstallDir) -join ';')
        [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
        # Reflect it in the current session too.
        $env:Path = "$env:Path;$InstallDir"
        Write-Good "Added $InstallDir to your user PATH (restart terminals to pick it up)"
    } else {
        Write-Info "$InstallDir already on your user PATH"
    }
}

# ---------------------------------------------------------------------------
# Seed a default config
# ---------------------------------------------------------------------------
$configDir  = Join-Path $env:APPDATA 'tokengauge'
$configFile = Join-Path $configDir 'config.toml'
if (-not (Test-Path $configFile)) {
    New-Item -ItemType Directory -Force -Path $configDir | Out-Null
    # Single-quoted here-string: no variable/backtick interpretation.
    # cache_file is intentionally omitted so it defaults to %TEMP%.
    $config = @'
# TokenGauge configuration (Windows)
# Limits need a Windows codexbar binary; without one, ccusage cost/token data
# still works. Point codexbar_bin at a full path if you have a .cmd/.bat shim.
codexbar_bin = "codexbar"
refresh_secs = 600

[providers]
codex = true
claude = true
'@
    # WriteAllText writes UTF-8 without a BOM (Set-Content -Encoding UTF8 on
    # Windows PowerShell 5.1 emits a BOM, which the TOML parser rejects).
    [System.IO.File]::WriteAllText($configFile, $config)
    Write-Good "Wrote default config to $configFile"
} else {
    Write-Info "Config already exists at $configFile (left unchanged)"
}

# ---------------------------------------------------------------------------
# Prerequisite hints
# ---------------------------------------------------------------------------
Write-Host ""
if (-not (Get-Command node -ErrorAction SilentlyContinue) -and
    -not (Get-Command bun  -ErrorAction SilentlyContinue) -and
    -not (Get-Command ccusage -ErrorAction SilentlyContinue)) {
    Write-Warned "No Node.js / Bun / ccusage found on PATH. Install Node.js (https://nodejs.org) so"
    Write-Warned "cost tracking via 'npx ccusage' works. TokenGauge will run without it, but empty."
}
Write-Good "Done. Run it with:  tokengauge-tui"
