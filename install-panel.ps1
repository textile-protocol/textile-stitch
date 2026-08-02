# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright (c) 2026 Textile, Inc.
#
# Windows PowerShell install for Stitch (web UI) on Docker Desktop.
# Local mode only by default: password login at http://127.0.0.1:8420.
#
# Quick path:
#   irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex
#
# Recommended (pinned release + checksum):
#   $TAG = 'vX.Y.Z'
#   Invoke-WebRequest "https://raw.githubusercontent.com/textile-protocol/textile-stitch/$TAG/install-panel.ps1" -OutFile install-panel.ps1
#   Invoke-WebRequest "https://github.com/textile-protocol/textile-stitch/releases/download/$TAG/install-panel.ps1.sha256" -OutFile install-panel.ps1.sha256
#   $expected = ((Get-Content install-panel.ps1.sha256).Split()[0]).ToLowerInvariant()
#   $actual = (Get-FileHash install-panel.ps1 -Algorithm SHA256).Hash.ToLowerInvariant()
#   if ($actual -ne $expected) { throw "checksum mismatch for install-panel.ps1 (got $actual, want $expected)" }
#   $env:STITCH_REF = $TAG
#   $env:PANEL_IMAGE = 'ghcr.io/textile-protocol/textile-stitch-panel:sha-<commit>'
#   $env:STITCH_REQUIRE_PINNED = '1'
#   $env:PANEL_MODE = 'local'
#   $env:PANEL_PASSWORD = '…'
#   .\install-panel.ps1
#
# Non-interactive: set PANEL_MODE=local and PANEL_PASSWORD. Optionally PANEL_DIR,
# PANEL_BOTS_DIR, PANEL_IMAGE, STITCH_REF, STITCH_COMPOSE_SHA256, STITCH_REQUIRE_PINNED.
#
# Server/Tailscale mode is for Linux hosts. On Windows this script installs local
# mode only — use a Linux server (or WSL2 Linux Docker) for Tailscale.

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRaw = if ($env:STITCH_REPO_RAW) { $env:STITCH_REPO_RAW } else { 'https://raw.githubusercontent.com/textile-protocol/textile-stitch' }
$GithubApi = if ($env:STITCH_GITHUB_API) { $env:STITCH_GITHUB_API } else { 'https://api.github.com/repos/textile-protocol/textile-stitch' }
$DefaultImageRepo = 'ghcr.io/textile-protocol/textile-stitch-panel'
$DefaultDir = Join-Path $env:USERPROFILE 'stitch-panel'
$DefaultBotsDir = Join-Path $env:USERPROFILE 'stitch-bots'

function Write-Say([string]$Message) { Write-Host $Message }
function Write-Step([string]$Message) { Write-Host ""; Write-Host "==> $Message" }
function Write-Warn([string]$Message) { Write-Warning $Message }
function Die([string]$Message) { Write-Error "error: $Message"; exit 1 }

# Native executables (docker.exe, icacls.exe) set $LASTEXITCODE but do not throw.
# $ErrorActionPreference only affects cmdlets, so try/catch never sees them fail.
function Assert-LastExitOk([string]$FailureMessage) {
    if ($null -eq $LASTEXITCODE -or $LASTEXITCODE -eq 0) { return }
    Die $FailureMessage
}

function ConvertTo-DockerHostPath([string]$Path) {
    # Forward slashes keep Path::join correct inside the Linux panel container
    # and are accepted by Docker Desktop on Windows. Drive letters still need
    # the structured Mount API (see bollard create), not short bind strings.
    return ([System.IO.Path]::GetFullPath($Path)).Replace('\', '/')
}

function Protect-EnvFile([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    $user = if ($env:USERNAME) { $env:USERNAME } else { [Environment]::UserName }
    # icacls is a native exe — nonzero exit does not throw. Fail closed: the
    # .env holds the password hash, so a failed ACL restriction must stop install.
    & icacls $Path /inheritance:r /grant:r "${user}:F" | Out-Null
    Assert-LastExitOk "couldn't restrict ACL on $Path to user-only (icacls failed). Move PANEL_DIR onto an NTFS volume and re-run."
}

# Windows PowerShell 5.1's `Set-Content -Encoding utf8` writes a BOM. Docker
# Compose then fails to parse the project `.env` (BOM before the first key).
# Pair writes with explicit UTF-8 reads: bare `Get-Content` on PS 5.1 uses the
# system ANSI code page for BOM-free UTF-8, which mojibakes non-ASCII paths
# (e.g. PANEL_BOTS_DIR under a localized %USERPROFILE%) on rewrite.
function Get-Utf8NoBomEncoding {
    return New-Object System.Text.UTF8Encoding $false
}

function Write-Utf8NoBomFile([string]$Path, [string[]]$Lines) {
    [System.IO.File]::WriteAllLines($Path, $Lines, (Get-Utf8NoBomEncoding))
}

function Read-Utf8NoBomLines([string]$Path) {
    return [System.IO.File]::ReadAllLines($Path, (Get-Utf8NoBomEncoding))
}

function Get-EnvFileValue([string]$File, [string]$Key) {
    if (-not (Test-Path -LiteralPath $File)) { return '' }
    $line = Read-Utf8NoBomLines $File | Where-Object { $_ -match "^$([regex]::Escape($Key))=" } | Select-Object -First 1
    if (-not $line) { return '' }
    $val = $line.Substring($Key.Length + 1)
    if (($val.StartsWith("'") -and $val.EndsWith("'")) -or ($val.StartsWith('"') -and $val.EndsWith('"'))) {
        $val = $val.Substring(1, $val.Length - 2)
    }
    return $val
}

function Test-FloatingLatestImage([string]$Image) {
    if ($Image -match '@') { return $false }
    $name = Split-Path -Leaf $Image
    if ($name -match ':latest$') { return $true }
    if ($name -match ':') { return $false }
    return $true
}

function Test-PinnedImage([string]$Image) {
    return ($Image -match '@sha256:[0-9a-fA-F]{64}$') -or ($Image -match ':sha-[0-9a-fA-F]{7,64}$')
}

function Test-PinnedRef([string]$Ref) {
    return ($Ref -match '^v[0-9]+\.[0-9]+\.[0-9]+([+.-][A-Za-z0-9.+-]+)?$') -or ($Ref -match '^[0-9a-fA-F]{40}$')
}

function Test-RequirePinned {
    switch -Regex ($env:STITCH_REQUIRE_PINNED) {
        '^(1|true|TRUE|yes|YES)$' { return $true }
        default { return $false }
    }
}

function Resolve-StitchRef {
    if ($env:STITCH_REF) { return $env:STITCH_REF }
    try {
        $json = Invoke-RestMethod -Uri "$GithubApi/releases/latest" -Headers @{ 'User-Agent' = 'stitch-install-panel' }
        if ($json.tag_name) { return [string]$json.tag_name }
    } catch {
        # fall through
    }
    Write-Warn "couldn't resolve the latest release tag from GitHub; falling back to main."
    Write-Warn 'Pin STITCH_REF=vX.Y.Z for a reproducible install.'
    return 'main'
}

function Read-Secret([string]$Prompt) {
    $secure = Read-Host -AsSecureString -Prompt $Prompt
    $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($secure)
    try {
        return [Runtime.InteropServices.Marshal]::PtrToStringBSTR($bstr)
    } finally {
        [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr) | Out-Null
    }
}

function Need-Value([string]$Current, [string]$Prompt, [string]$VarName, [switch]$Secret) {
    if ($Current) { return $Current }
    if (-not [Environment]::UserInteractive) {
        Die "$VarName is not set and there is no interactive session to ask on. Set it and re-run."
    }
    if ($Secret) { return (Read-Secret $Prompt) }
    return (Read-Host -Prompt $Prompt)
}

# Unicode scalar count — matches stitch-panel's chars().count() (not UTF-16 .Length).
function Get-PasswordCharCount([string]$Password) {
    $n = 0
    for ($i = 0; $i -lt $Password.Length; $i++) {
        $n++
        if (
            [char]::IsHighSurrogate($Password[$i]) -and
            ($i + 1) -lt $Password.Length -and
            [char]::IsLowSurrogate($Password[$i + 1])
        ) {
            $i++
        }
    }
    return $n
}

function Need-Password([string]$Current, [string]$Prompt, [string]$VarName) {
    if ($Current) {
        $len = Get-PasswordCharCount $Current
        if ($len -lt 12) {
            Die "$VarName must be at least 12 characters (got $len)"
        }
        return $Current
    }
    if (-not [Environment]::UserInteractive) {
        Die "$VarName is not set and there is no interactive session to ask on. Set it and re-run."
    }
    while ($true) {
        $pw = Read-Secret $Prompt
        if (-not $pw) { Die 'a panel password is required' }
        $len = Get-PasswordCharCount $pw
        if ($len -lt 12) {
            Write-Warn "need at least 12 characters (got $len). Try again."
            continue
        }
        $again = Read-Secret 'Again'
        if ($pw -cne $again) {
            Write-Warn "those didn't match. Try again."
            continue
        }
        return $pw
    }
}

function Fetch-File([string]$Url, [string]$Dest) {
    try {
        Invoke-WebRequest -Uri $Url -OutFile $Dest -UseBasicParsing
    } catch {
        Die "couldn't download $Url"
    }
}

# ---------------------------------------------------------------- preflight ---
# Plain-language Docker help — many Windows operators have never used containers.
# Use Write-Host (not Write-Error): $ErrorActionPreference=Stop would otherwise
# abort before the "what to do" steps print.
function Explain-Docker([ValidateSet('missing', 'compose', 'not_running')][string]$Reason) {
    Write-Host ''
    switch ($Reason) {
        'missing' {
            Write-Host 'error: Docker is not installed on this computer.'
            Write-Host ''
            Write-Host 'Stitch runs inside Docker — a free app that hosts the web panel.'
            Write-Host 'Install it once, then re-run the same install command.'
            Write-Host ''
            Write-Host 'What to do:'
            Write-Host '  1. Download and install Docker Desktop for Windows:'
            Write-Host '     https://docs.docker.com/desktop/setup/install/windows-install/'
            Write-Host '  2. Open Docker Desktop from the Start menu and wait until it says'
            Write-Host '     Docker is running (whale icon in the system tray is steady).'
            Write-Host '  3. Open a new PowerShell window and re-run:'
            Write-Host '     irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex'
        }
        'compose' {
            Write-Host 'error: Docker Compose v2 is missing.'
            Write-Host ''
            Write-Host 'The docker command is present, but docker compose does not work.'
            Write-Host 'Stitch needs Compose v2. Update or reinstall Docker Desktop, then re-run.'
            Write-Host '  https://docs.docker.com/desktop/setup/install/windows-install/'
        }
        'not_running' {
            Write-Host 'error: Docker is installed but not running.'
            Write-Host ''
            Write-Host 'The panel cannot start until Docker Desktop is up.'
            Write-Host ''
            Write-Host 'What to do:'
            Write-Host '  1. Open Docker Desktop from the Start menu.'
            Write-Host '  2. Wait until it finishes starting (system-tray whale icon steady /'
            Write-Host '     Docker Desktop is running).'
            Write-Host '  3. Re-run the install command in PowerShell.'
        }
    }
    Write-Host ''
    exit 1
}

Write-Step 'Checking Docker'
if (-not (Get-Command docker -ErrorAction SilentlyContinue)) {
    Explain-Docker missing
}
docker compose version | Out-Null
if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {
    Explain-Docker compose
}
docker info | Out-Null
if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {
    Explain-Docker not_running
}
Write-Say 'Docker and Compose v2 are ready.'

# Windows Docker Desktop runs Linux containers. Tailscale server compose needs
# /dev/net/tun and is aimed at Linux hosts — keep this installer on local mode.
# Group the alternatives so anchors apply to every alias (otherwise
# PANEL_MODE=server-laptop would match `laptop` in the middle and slip through).
if ($env:PANEL_MODE -and $env:PANEL_MODE -notmatch '^(?i)(local|laptop|desktop|computer)$') {
    Die @"
PANEL_MODE=$($env:PANEL_MODE) is not supported by install-panel.ps1.
On Windows, use local mode (password on http://127.0.0.1:8420):

  `$env:PANEL_MODE = 'local'
  `$env:PANEL_PASSWORD = '…'
  irm https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.ps1 | iex

For Tailscale server mode, install on a Linux Docker host with install-panel.sh.
"@
}

Write-Step 'Where to install'
$PanelDir = if ($env:PANEL_DIR) { $env:PANEL_DIR } else {
    if ([Environment]::UserInteractive) {
        $answer = Read-Host -Prompt "Directory for the compose file and .env [$DefaultDir]"
        if ($answer) { $answer } else { $DefaultDir }
    } else { $DefaultDir }
}
$PanelDir = [System.IO.Path]::GetFullPath($PanelDir)
$EnvFile = Join-Path $PanelDir '.env'

$ReuseEnv = $false
if (Test-Path -LiteralPath $EnvFile) {
    Write-Step 'Existing install found'
    Write-Say "$EnvFile already exists, so its settings are kept as they are."
    Write-Say 'Delete it first if you want to be asked again.'
    $ReuseEnv = $true
    if (-not $env:PANEL_IMAGE) {
        $saved = Get-EnvFileValue $EnvFile 'PANEL_IMAGE'
        if ($saved) { $env:PANEL_IMAGE = $saved; Write-Say "Reusing PANEL_IMAGE from $EnvFile" }
    }
    if (-not $env:STITCH_REF) {
        $saved = Get-EnvFileValue $EnvFile 'STITCH_REF'
        if ($saved) { $env:STITCH_REF = $saved; Write-Say "Reusing STITCH_REF from $EnvFile" }
    }
}

$PanelBotsDir = if ($env:PANEL_BOTS_DIR) { $env:PANEL_BOTS_DIR } else {
    if ([Environment]::UserInteractive) {
        $answer = Read-Host -Prompt "Directory on this host for bot configs [$DefaultBotsDir]"
        if ($answer) { $answer } else { $DefaultBotsDir }
    } else { $DefaultBotsDir }
}
$PanelBotsDir = [System.IO.Path]::GetFullPath($PanelBotsDir)
$PanelBotsDirDocker = ConvertTo-DockerHostPath $PanelBotsDir

Write-Step 'Resolving install ref'
$StitchRefExplicit = [bool]$env:STITCH_REF
$Ref = Resolve-StitchRef
$PanelImage = if ($env:PANEL_IMAGE) { $env:PANEL_IMAGE } else { "${DefaultImageRepo}:latest" }

if ((Test-FloatingLatestImage $PanelImage) -and -not $env:STITCH_REF) {
    if ($Ref -ne 'main') {
        Write-Warn "PANEL_IMAGE resolves to :latest; using STITCH_REF=main so compose tracks the image (resolved release was $Ref)."
        Write-Warn 'Pin both STITCH_REF=vX.Y.Z and PANEL_IMAGE=…:sha-… together for a release install.'
        $Ref = 'main'
    }
}

if (Test-RequirePinned) {
    if (-not $StitchRefExplicit) {
        Die "STITCH_REQUIRE_PINNED=1 requires STITCH_REF to be set explicitly (resolved $Ref from GitHub)."
    }
    if (-not (Test-PinnedImage $PanelImage)) {
        Die "STITCH_REQUIRE_PINNED=1 requires PANEL_IMAGE to be a sha-* tag or @sha256:… digest (got $PanelImage)."
    }
    if (-not (Test-PinnedRef $Ref)) {
        Die "STITCH_REQUIRE_PINNED=1 requires STITCH_REF to be a release tag (vX.Y.Z) or 40-char commit SHA (got $Ref)."
    }
} elseif (Test-FloatingLatestImage $PanelImage) {
    Write-Warn "PANEL_IMAGE=$PanelImage uses a floating tag. Pin a sha-* tag or @sha256:… digest in production."
}

$ComposeFile = 'docker-compose.panel.local.yml'
Write-Say "Using ref: $Ref"

Write-Step "Installing into $PanelDir (local)"
New-Item -ItemType Directory -Force -Path $PanelDir | Out-Null

$composeTmp = Join-Path $env:TEMP ("stitch-compose-" + [guid]::NewGuid().ToString('n') + '.yml')
try {
    Fetch-File "$RepoRaw/$Ref/$ComposeFile" $composeTmp
    # STITCH_COMPOSE_SHA256 is the release digest for docker-compose.panel.yml
    # (server). This installer always fetches docker-compose.panel.local.yml, so
    # applying that digest would fail a valid Windows pin — same skip as install-panel.sh.
    if ($env:STITCH_COMPOSE_SHA256) {
        Write-Warn "STITCH_COMPOSE_SHA256 is set but this install uses $ComposeFile; checksum skipped."
        Write-Warn 'For server compose integrity checks use install-panel.sh. Release also publishes docker-compose.panel.local.yml.sha256 for manual verification.'
    }
    Copy-Item -LiteralPath $composeTmp -Destination (Join-Path $PanelDir $ComposeFile) -Force
} finally {
    Remove-Item -LiteralPath $composeTmp -Force -ErrorAction SilentlyContinue
}
Write-Say 'Local compose file in place.'

Write-Step "Creating the bots root at $PanelBotsDir"
New-Item -ItemType Directory -Force -Path $PanelBotsDir | Out-Null
Write-Say 'Created.'

if (-not $ReuseEnv) {
    Write-Step 'Writing .env'
    $tmpEnv = Join-Path $PanelDir ('.env.tmp.' + [guid]::NewGuid().ToString('n'))
    Write-Utf8NoBomFile $tmpEnv @(
        '# Written by install-panel.ps1. Restricted to your user via icacls.'
        'PANEL_MODE=local'
        "PANEL_BOTS_DIR=$PanelBotsDirDocker"
        "PANEL_IMAGE=$PanelImage"
        "STITCH_REF=$Ref"
    )
    Move-Item -LiteralPath $tmpEnv -Destination $EnvFile -Force
    Protect-EnvFile $EnvFile
    Write-Say "Wrote $EnvFile (user-only ACL)."
}

function Add-Password([string]$Password, [string]$Label = 'password') {
    $len = Get-PasswordCharCount $Password
    if ($len -lt 12) {
        Die "password must be at least 12 characters (got $len)"
    }

    # Keep stderr: the binary's real reason used to disappear into $null.
    $errFile = [System.IO.Path]::GetTempFileName()
    try {
        $hash = $Password | docker run --rm -i $PanelImage hash-password 2>$errFile
        if ($null -ne $LASTEXITCODE -and $LASTEXITCODE -ne 0) {
            $msg = @(Get-Content -LiteralPath $errFile -ErrorAction SilentlyContinue |
                Where-Object { $_.Trim() } |
                ForEach-Object { $_ -replace '^Error:\s*', '' } |
                Select-Object -Last 1) -join ''
            if ($msg) { Die "hashing the password failed: $msg" }
            Die 'hashing the password failed'
        }
        $hashLine = @($hash) | Where-Object { $_ -match '^\$argon2' } | Select-Object -First 1
        if (-not $hashLine) { Die 'the panel returned no argon2 password hash' }
        $hash = $hashLine.ToString().Trim()
    } finally {
        Remove-Item -LiteralPath $errFile -Force -ErrorAction SilentlyContinue
    }

    $lines = @()
    if (Test-Path -LiteralPath $EnvFile) {
        $lines = @(Read-Utf8NoBomLines $EnvFile | Where-Object { $_ -notmatch '^PANEL_PASSWORD_HASH=' })
    }
    $lines += "PANEL_PASSWORD_HASH='$hash'"
    Write-Utf8NoBomFile $EnvFile $lines
    Protect-EnvFile $EnvFile
    Write-Say "Added $Label."
}

Write-Step "Pulling $PanelImage"
docker pull $PanelImage | Out-Null
Assert-LastExitOk "couldn't pull $PanelImage"
Write-Say 'Pulled.'

if (-not (Get-EnvFileValue $EnvFile 'PANEL_PASSWORD_HASH')) {
    Write-Step 'Panel password'
    if (-not $env:PANEL_PASSWORD) {
        Write-Say 'This install is loopback-only. You log in with a password you choose now.'
    }
    $password = Need-Password $env:PANEL_PASSWORD 'Panel password (12+ characters, not shown)' 'PANEL_PASSWORD'
    Add-Password $password 'panel password'
} elseif ($env:PANEL_PASSWORD) {
    Write-Step 'Updating the panel password'
    Add-Password $env:PANEL_PASSWORD 'panel password'
}

Write-Step 'Starting the panel'
Push-Location $PanelDir
try {
    docker compose -f $ComposeFile up -d --no-build
    Assert-LastExitOk "couldn't start the panel (`docker compose up` failed)"
} finally {
    Pop-Location
}

Write-Step 'Done'
Write-Say 'Open http://127.0.0.1:8420 and log in with the password you set.'
Write-Say ''
Write-Say 'In the web UI: Add a bot, pick a corridor, paste your operator wallet key,'
Write-Say 'approve tokens, then start. The installer does not configure bots for you.'
Write-Say ''
Write-Say "Logs:    Set-Location '$PanelDir'; docker compose -f $ComposeFile logs -f panel"
Write-Say "Stop:    Set-Location '$PanelDir'; docker compose -f $ComposeFile down"
Write-Say 'Advanced setups (custom reverse proxy, building from source):'
Write-Say 'https://github.com/textile-protocol/textile-stitch/blob/main/docs/install-panel.md'
