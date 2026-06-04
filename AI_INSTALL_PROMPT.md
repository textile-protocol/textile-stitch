# AI Install Prompt For Stitch

Copy this prompt into Claude, GPT, Codex, or another coding agent that has
terminal access to the machine where Stitch should run.

Coding agents (including Codex) often block `curl ... | sh` because the script is
not inspected before execution. The prompt below forbids pipe-to-shell installs,
downloads the release installer to disk first (so `stitch --update` keeps working),
and documents a checksum-verified archive fallback when an agent will not run any
installer script.

```text
You are helping me install and configure Textile Stitch, the operator bot at:
https://github.com/textile-protocol/textile-stitch

Goal:
- Install the latest Stitch release on this machine.
- Configure Stitch with my operator settings.
- Store secrets safely.
- Run a dry run first.
- Start live or persistent operation only after I explicitly confirm.

Hard rules:
- Do not clone https://github.com/textile-protocol/textile-stitch into my
  workspace or any operator workspace.
- Do not build from source unless I explicitly request a source install.
- Install only from the latest GitHub Release assets.
- Never pipe a remote script into an interpreter. Forbidden patterns include
  `curl ... | sh`, `curl ... | bash`, and `irm ... | iex`. Download release
  assets to a local file first, then run or extract them from disk.
- If release discovery, asset download, checksum verification, installer
  execution, config writing, secret writing, or dry run fails, stop and explain.
- If the release installer URL in older docs is stale, discover the actual
  latest-release installer asset from GitHub Release metadata instead of
  guessing or cloning.
- Do not start live operation, background services, launchd, systemd, Task
  Scheduler, Windows services, or any persistent process until after a
  successful dry run and my explicit confirmation.

Security rules:
- Do not ask me to paste STITCH_PRIVATE_KEY into chat or any question tool.
- Do not put STITCH_PRIVATE_KEY in stitch.toml, shell history, logs, screenshots,
  or command arguments.
- Collect STITCH_PRIVATE_KEY only through a local hidden terminal prompt.
- Write it only to the platform env file as STITCH_PRIVATE_KEY=...
- Restrict env file permissions. On Unix, use chmod 600.
- Use a dedicated operator wallet.
- Before any live operation, remind me to confirm token balances and Permit2
  approvals.

Question tool rules:
- If your environment provides a question tool such as AskUserQuestionTool,
  request_user_input, or equivalent, use it for all non-secret operator
  questions.
- Put the recommended default first and label it recommended.
- Ask focused questions. Batch only short, related fields when the tool is
  designed for that.
- Ask for values with no safe default.
- Never ask for STITCH_PRIVATE_KEY through a question tool or chat.
- If no question tool exists, ask concise chat questions instead.

Use these defaults unless I provide different values:
- Binary command name: stitch
- GitHub repo: textile-protocol/textile-stitch
- Network: Base
- Chain ID: 8453
- RPC URL: https://mainnet.base.org
- Textile indexer URL: https://api.textilecredit.com
- Permit2 address: 0x000000000022D473030F116dDEE9F6B43aC78BA3
- Price feed staleness: 30 seconds
- Tick interval: 5 seconds
- Token decimals: 6, unless I provide different decimals
- Buy spread: 150 bps
- Sell spread: 150 bps
- Buy total liquidity: 50000 units of the debt/stable token
- Sell total liquidity: 50000 units of the collateral/soft token
- Minimum order slice: 10 units of the debt/stable token
- Maximum ladder orders per side: 150
- Order TTL: 30 seconds
- Refresh threshold: 10 bps
- Settlement closing: disabled unless I explicitly enable it
- Settlement close window: 432000 seconds, only if closing is enabled
- Minimum close margin: 0, only if closing is enabled
- Max positions per fill: 10, only if closing is enabled
- Discover first: 200, only if closing is enabled
- Skip past-window positions: true, only if closing is enabled
- Linux config directory: /etc/stitch
- Linux config path: /etc/stitch/stitch.toml
- Linux env path: /etc/stitch/stitch.env
- Linux service name: stitch
- macOS config directory: ~/.config/stitch
- macOS config path: ~/.config/stitch/stitch.toml
- macOS env path: ~/.config/stitch/stitch.env
- Windows config directory: %ProgramData%\Stitch
- Windows config path: %ProgramData%\Stitch\stitch.toml
- Windows env path: %ProgramData%\Stitch\stitch.env

Before editing files, first detect the OS, architecture, shell, package
environment, and available terminal/question tools.

Gather these values:
- Persistent background service or manual foreground run. Default to systemd on
  Linux, foreground/manual on macOS, and foreground/manual on Windows.
- Chain ID, default 8453.
- RPC URL, default https://mainnet.base.org.
- Textile indexer URL, default https://api.textilecredit.com.
- Permit2 address, default 0x000000000022D473030F116dDEE9F6B43aC78BA3.
- Reactor address, no safe default.
- Price feed URL, no safe default.
- Collateral token address, no safe default.
- Collateral token decimals, default 6.
- Debt token address, no safe default.
- Debt token decimals, default 6.
- Buy and sell spread, default 150 bps each.
- Buy and sell order sizing, default 50000 total depth, 10 minimum slice, and
  150 max orders per side.
- Whether settlement closing should be enabled, default no.

If settlement closing is enabled, also gather:
- Subgraph URL, no safe default.
- Settlement pool address, no safe default.
- floor_ray, no safe default unless I explicitly allow example values.
- buffer_ray, no safe default unless I explicitly allow example values.
- window_secs, default 432000.
- min_margin_collateral, default 0.
- max_positions_per_fill, default 10.
- discover_first, default 200.
- skip_past_window, default true.

Release install procedure:
1. Query the latest GitHub Release metadata:

   curl -fsSL https://api.github.com/repos/textile-protocol/textile-stitch/releases/latest

2. Determine the latest tag and available assets from the release metadata.

3. Choose an install path:

   A. Recommended (supports `stitch --update` later): run the release installer
      from a downloaded local file. The cargo-dist installer writes an install
      receipt that `stitch --update` requires. Do not use `curl | sh`.

   B. Fallback (for strict agent safety policies that block any remote
      installer script): install the matching release binary archive with
      checksum verification. This path does not create an install receipt, so
      `stitch --update` will not work until the operator runs path A once or
      upgrades by repeating path B on each release.

4. Path A — installer from disk (Unix):

   INSTALLER_URL="<discovered-installer-url>"
   INSTALLER_PATH="$(mktemp -t stitch-installer.XXXXXX.sh)"
   curl --proto '=https' --tlsv1.2 -fsSL "$INSTALLER_URL" -o "$INSTALLER_PATH"
   chmod 700 "$INSTALLER_PATH"
   sh "$INSTALLER_PATH"
   rm -f "$INSTALLER_PATH"

   Pick the installer asset in this order:
   - stitch-bot-installer.sh, then stitch-installer.sh

   Path A — installer from disk (Windows PowerShell):

   $InstallerUrl = "<discovered-installer-url>"
   $InstallerPath = Join-Path $env:TEMP "stitch-installer.ps1"
   Invoke-WebRequest -Uri $InstallerUrl -OutFile $InstallerPath
   powershell -ExecutionPolicy Bypass -File $InstallerPath
   Remove-Item -Force $InstallerPath

   Pick the installer asset in this order:
   - stitch-bot-installer.ps1, then stitch-installer.ps1

   If your environment still blocks executing a downloaded installer script,
   stop and ask me to choose one of:
   - I run the installer command myself in a normal terminal (paste the exact
     `curl -o` + `sh` commands you prepared).
   - You continue with path B (archive install; no `stitch --update` until I
     run path A manually later).

5. Path B — binary archive (when path A is blocked or I explicitly request it):
   - macOS Apple Silicon: *-aarch64-apple-darwin.tar.xz
   - macOS Intel: *-x86_64-apple-darwin.tar.xz
   - Linux ARM64: *-aarch64-unknown-linux-gnu.tar.xz
   - Linux x64: *-x86_64-unknown-linux-gnu.tar.xz
   - Windows x64: *-x86_64-pc-windows-msvc.zip

   Download the archive and its matching .sha256 asset, verify the checksum,
   extract, copy `stitch` (or `stitch.exe`) into a directory on PATH (release
   builds use the cargo-dist install path, typically `~/.cargo/bin` on Unix),
   and ensure the binary is executable.

6. Verify:

   stitch --version

   Optional: confirm self-update will work:

   stitch --update

   If that errors with "no install receipt found", explain that path B was used
   and offer path A (installer from disk) or a manual re-run of the installer.

If stitch is not on PATH, locate the installed release binary and use its
absolute path.

Configuration procedure:
1. Download the example config:

   curl -fsSL https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/stitch.example.toml

2. Create stitch.toml at the platform config path using gathered values.

3. Convert human token amounts to base units using token decimals.
   - Example: 50000 with 6 decimals becomes "50000000000".
   - Example: 10 with 6 decimals becomes "10000000".

4. If settlement closing is disabled:
   - Omit subgraph_url.
   - Omit closer fields such as closer_pool, floor_ray, buffer_ray, window_secs,
     min_margin_collateral, max_positions_per_fill, discover_first, and
     skip_past_window.

5. Restrict config file permissions:
   - Unix: config directory 700, config file 600.

Private key collection:
- Use a terminal-opening tool if available.
- If an OpenTerminalTool, integrated terminal tool, or equivalent exists, open a
  local terminal running the script below.
- The script must prompt with hidden input.
- Wait for me to complete it before continuing.

Unix and macOS private-key script:

   #!/usr/bin/env sh
   set -eu

   CONFIG_DIR="${HOME}/.config/stitch"
   ENV_FILE="${CONFIG_DIR}/stitch.env"

   mkdir -p "$CONFIG_DIR"
   chmod 700 "$CONFIG_DIR"

   printf 'Enter STITCH_PRIVATE_KEY: '
   stty -echo
   IFS= read -r key
   stty echo
   printf '\n'

   if [ -z "$key" ]; then
     echo "No private key entered; not writing env file."
     exit 1
   fi

   umask 077
   printf 'STITCH_PRIVATE_KEY=%s\n' "$key" > "$ENV_FILE"
   unset key
   chmod 600 "$ENV_FILE"

   echo "Wrote $ENV_FILE with restricted permissions."

If no terminal-opening tool is available:
- Try running an interactive TTY command.
- If that is not possible, print the script for me to run manually.
- Do not proceed until I confirm the env file has been created.

Windows PowerShell private-key script:

   $ConfigDir = "$env:ProgramData\Stitch"
   $EnvFile = "$ConfigDir\stitch.env"

   New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null

   $key = Read-Host "Enter STITCH_PRIVATE_KEY" -AsSecureString
   $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($key)
   $plain = [Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)

   if ([string]::IsNullOrWhiteSpace($plain)) {
     Write-Error "No private key entered; not writing env file."
     exit 1
   }

   "STITCH_PRIVATE_KEY=$plain" | Set-Content -NoNewline -Path $EnvFile
   [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
   $plain = $null

   Write-Host "Wrote $EnvFile."

Dry run:
- Run dry run only after config and env file exist.
- Load STITCH_PRIVATE_KEY from the env file into the process environment.
- Do not print the private key.
- Do not pass the private key in command arguments.

Unix and macOS:

   set -a
   . "<env-path>"
   set +a
   stitch --config "<config-path>" --dry-run

Windows PowerShell:

   Get-Content "<env-path>" | ForEach-Object {
     if ($_ -match '^([^=]+)=(.*)$') {
       [Environment]::SetEnvironmentVariable($matches[1], $matches[2], "Process")
     }
   }
   stitch.exe --config "<config-path>" --dry-run

Do not use command substitution such as:

   STITCH_PRIVATE_KEY="$(...)" stitch ...

because that can expose secrets through command construction or logs.

After dry run:
Show me a short summary:
- Installed Stitch version.
- Config path.
- Env path exists and permissions are restricted, without showing secret
  contents.
- Network, chain ID, and RPC URL.
- Reactor address.
- Pool token addresses and decimals.
- Settlement closing enabled or disabled.
- Dry-run result.

Then ask for explicit confirmation before:
- Foreground live run.
- launchd.
- systemd.
- Task Scheduler.
- Windows service.
- Any other persistent or live operation.

Service setup after confirmation only:
- macOS: If I choose launchd, create a LaunchAgent plist that runs
  stitch --config ~/.config/stitch/stitch.toml, and make sure the env file is
  loaded without putting STITCH_PRIVATE_KEY in command arguments.
- Linux: If I confirm systemd, install a stitch.service that loads the env file
  via EnvironmentFile, then run sudo systemctl daemon-reload and
  sudo systemctl enable --now stitch.
- Windows: If I choose Task Scheduler, create a startup task. If I choose a
  Windows service, ask before installing or using NSSM.

Never start any live operation until I explicitly confirm after a successful dry
run.
```
