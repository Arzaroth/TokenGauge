#Requires -Version 5.1
<#
.SYNOPSIS
    Install TokenGauge on Windows 10/11.

.DESCRIPTION
    Downloads the latest (or a specified) TokenGauge Windows release, installs
    tokengauge-tui.exe and the system-tray GUI tokengauge-tray.exe into a
    user-writable directory, adds that directory to your user PATH, puts
    TokenGauge in the Start Menu, and seeds a default config at
    %APPDATA%\tokengauge\config.toml.

    The Waybar module, KDE Plasma applet, GNOME extension and Quickshell widget
    are Linux-only. Usage limits are fetched natively over HTTP; sign in to the
    `codex` and/or `claude` CLIs so TokenGauge can read their OAuth
    credentials. ccusage (needs Node.js/Bun on PATH) then adds cost/token
    detail.

.PARAMETER Repo
    GitHub repo to install from. Default: Arzaroth/TokenGauge.

.PARAMETER Version
    Release tag to install (e.g. v0.8.0). Default: the latest release.

.PARAMETER InstallDir
    Where to place the binaries. Default: %LOCALAPPDATA%\TokenGauge\bin.

.PARAMETER NoPath
    Do not modify the user PATH.

.PARAMETER RunAtLogin
    Also start the tray GUI at login (a shortcut in the Startup folder).

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
    [switch]$NoPath,
    [switch]$RunAtLogin
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
# The release job sanitizes the tag when naming the archive, so mirror that here
# (the download path still uses the raw tag, which is the actual release/tag name).
$assetVersion = $Version -replace '[^A-Za-z0-9._-]', '_'
$asset = "tokengauge-$assetVersion-windows-x86_64.zip"
$url   = "https://github.com/$Repo/releases/download/$Version/$asset"
$tmp   = Join-Path ([System.IO.Path]::GetTempPath()) ("tokengauge-install-" + [System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

try {
    $zipPath = Join-Path $tmp $asset
    Write-Info "Downloading $url"
    try {
        Invoke-WebRequest -Uri $url -OutFile $zipPath -Headers $Headers
    } catch {
        throw "Failed to download $asset ($($_.Exception.Message)). This release may " +
              "predate Windows support - install a newer release with -Version, or " +
              "build from source: cargo build --release -p tokengauge-tui"
    }

    Expand-Archive -Path $zipPath -DestinationPath $tmp -Force

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

    # Both binaries: the TUI is the command line, the tray is the GUI most
    # Windows users actually run. Releases before the tray existed carry only
    # the TUI, so a missing tray binary is a warning rather than a failure.
    $installed = @{}
    foreach ($name in @('tokengauge-tui.exe', 'tokengauge-tray.exe')) {
        $exe = Get-ChildItem -Path $tmp -Recurse -Filter $name | Select-Object -First 1
        if (-not $exe) {
            if ($name -eq 'tokengauge-tui.exe') { throw "$name not found inside $asset" }
            Write-Warned "$name is not in $asset - this release predates the tray GUI"
            continue
        }
        $dest = Join-Path $InstallDir $name
        try {
            Copy-Item -Path $exe.FullName -Destination $dest -Force
        } catch {
            throw "Couldn't write $name to $InstallDir - it may be running. " +
                  "Close it (and the tray app) and re-run. ($($_.Exception.Message))"
        }
        $installed[$name] = $dest
        Write-Good "Installed $name to $InstallDir"
    }
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
# Start Menu entry, and optionally start the tray at login
# ---------------------------------------------------------------------------
# The tray GUI has no taskbar button and no console, so without a shortcut the
# only way to launch it is to type its path.
$trayExe = $installed['tokengauge-tray.exe']
if ($trayExe) {
    try {
        $shell = New-Object -ComObject WScript.Shell

        $startMenu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
        New-Item -ItemType Directory -Force -Path $startMenu | Out-Null
        $link = $shell.CreateShortcut((Join-Path $startMenu 'TokenGauge.lnk'))
        $link.TargetPath = $trayExe
        $link.WorkingDirectory = $InstallDir
        $link.Description = 'TokenGauge usage meter (system tray)'
        $link.Save()
        Write-Good "Added TokenGauge to the Start Menu"

        $startup = [Environment]::GetFolderPath('Startup')
        $startupLink = Join-Path $startup 'TokenGauge.lnk'
        if ($RunAtLogin) {
            $auto = $shell.CreateShortcut($startupLink)
            $auto.TargetPath = $trayExe
            # Start in the tray with nothing on screen; a panel that opens
            # itself on every login is not what run-at-login is for.
            $auto.Arguments = '--hidden'
            $auto.WorkingDirectory = $InstallDir
            $auto.Description = 'TokenGauge usage meter (system tray)'
            $auto.Save()
            Write-Good "TokenGauge will start at login"
        } else {
            Write-Info "Re-run with -RunAtLogin to start the tray automatically"
        }
    } catch {
        Write-Warned "Couldn't create shortcuts ($($_.Exception.Message)). Launch the tray from $trayExe"
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
    # cache_file is intentionally omitted so it defaults to %LOCALAPPDATA%\TokenGauge.
    $config = @'
# TokenGauge configuration (Windows)
# Usage limits are fetched natively over HTTP; sign in to the codex and/or
# claude CLIs so TokenGauge can read their OAuth credentials. ccusage then adds
# cost/token detail.
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
    Write-Warned "'npx ccusage' can add cost/token detail. (Limits still show natively; costs won't.)"
}
if ($NoPath) {
    Write-Good "Done. Run it with:  & `"$(Join-Path $InstallDir 'tokengauge-tui.exe')`""
} else {
    Write-Good "Done. Run it with:  tokengauge-tui"
}
if ($trayExe) {
    Write-Good "Or open the tray GUI from the Start Menu (TokenGauge)."
}
