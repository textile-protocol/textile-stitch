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
- Discover the actual latest-release installer asset from GitHub Release
  metadata instead of guessing or cloning.
- Do not start live operation, background services, launchd, systemd, Task
  Scheduler, Windows services, or any persistent process until after a
  successful dry run and my explicit confirmation.

Security rules:
- Do not ask me to paste STITCH_PRIVATE_KEY into chat or any question tool.
- Do not put STITCH_PRIVATE_KEY in stitch.toml, shell history, logs, screenshots,
  or command arguments.
- Collect STITCH_PRIVATE_KEY only through a local hidden terminal prompt.
- Write it only to the platform key file. On Unix, use chmod 600.
- Put only STITCH_PRIVATE_KEY_FILE=<key-file-path> in the env file.
- If both STITCH_PRIVATE_KEY_FILE and STITCH_PRIVATE_KEY are set,
  STITCH_PRIVATE_KEY_FILE takes precedence.
- Use a dedicated operator wallet.
- Before any live operation, confirm token balances and set up Permit2 approvals
  with `stitch approve` (see the Permit2 approvals step). A live start refuses to
  run until the input tokens are approved.

Question tool rules:
- Use AskUserQuestion for every non-secret operator question when that tool
  exists in your environment. If the exact name differs (AskUserQuestionTool,
  request_user_input, AskQuestion, etc.), use the equivalent question tool —
  never substitute plain chat for a question the tool can ask.
- If running in Codex and request_user_input is unavailable because the thread
  is in Default mode, stop and ask me to switch to Plan mode if I want structured
  tappable questions. Otherwise continue with one-at-a-time chat questions.
- Ask one question per tool call and wait for my answer before asking the next.
  Don't batch unrelated fields into one prompt. I want to answer one at a time.
- Before the first question, tell me in one line what you're setting up and
  roughly how many questions to expect. Prefix each question with progress like
  "(3 of ~9)" so I know where I am.
- Make every question multiple choice. Pass an options array of concrete,
  tappable choices so I can pick instead of type. Phrase the question in plain
  language and add one short sentence on why it matters.
- Offer at most 3 options per question. Some equivalents (Codex's
  request_user_input) only accept 2-3 choices and add the free-form answer
  themselves, so more than 3 can fail to render or fall back to chat. If there
  are more than 3 good choices, show the top 3 (recommended first) and put the
  rest behind the free-form answer or a follow-up question.
- Put the recommended value first and label it "(recommended)". For numeric
  settings, offer a couple of sensible presets around the default (see each step
  below). For values with no good presets (URLs, addresses), still offer the
  known or discovered value as an option when you have one.
- The tool adds a free-form answer automatically, so I can always type a custom
  value (custom chain ID, RPC URL, address, amount). Don't add your own "Other"
  option and don't drop to plain chat for anything the tool can ask.
- Keep questions single-select unless a step explicitly says I may pick several;
  don't enable multi-select otherwise.
- Ask only the strictly-needed questions the interview lists. Everything else is
  auto-filled from discovery, the example config, my wallet, or safe defaults —
  don't invent extra questions. After the dry run, tell me I can edit the config
  file directly for advanced settings.
- Never ask for STITCH_PRIVATE_KEY through a question tool or chat.
- If no question tool exists, ask the same sequence as concise chat questions,
  still one at a time, with the same progress and recommended-first style.

Operator interview (AskUserQuestion, one question at a time):
Keep it short — ask ONLY the questions below. Everything else is auto-filled from
discovery, the example config, my wallet, or the defaults; never ask about it.
Run after detecting OS/architecture and before writing config.

1. Chain — which network should Stitch run on?
   - Options (3 max): Base — 8453 (recommended); BNB Smart Chain — 56; Another
     network. If I pick "Another network", ask a follow-up with the rest as
     options (Ethereum — 1; Celo — 42220; Local Hardhat — 31337 for debugging),
     and only list a public chain if it's on
     https://app.textilecredit.com/s/deposit. For anything else I'll type a
     custom chain ID in the free-form answer.
   - After I answer, set chain_id and RPC URL: Base -> https://mainnet.base.org,
     31337 -> http://127.0.0.1:8545. For other chains, ask a follow-up RPC URL
     question if you don't know the right default. On 31337, skip public catalog
     discovery and use local pool/token addresses (see discovery section).

2. Run mode — how should Stitch run? Offer the choices for my OS, recommended
   first:
   - macOS: Foreground / manual; launchd background service.
   - Linux: systemd service; Foreground / manual.
   - Windows: Foreground / manual; Task Scheduler; Windows service.

3. Pool — after discovery (below), present pools as options (display name or pair
   label, recommended = first listed) so I pick a row instead of typing an
   address. Show at most 3; if there are more, show the first 3 and let me name
   another via the free-form answer. If discovery returned nothing on a real
   chain, stop; on 31337 use the local-address flow. Both legs run by default, so
   the pool I pick is also the settlement-closing target — don't ask me whether
   to enable closing.

4. Buy spread — how far below the mid should the bid sit?
   - Options: 0.1% (recommended); 0.25%; 0.5%. Or type an absolute like "5 cNGN"
     or "5 NGN" (soft units per stable). Mapping: a percentage X% ->
     buy_offset_bps = X * 100 (0.1% -> 10); an absolute N -> buy_offset_abs = N.

5. Sell spread — how far above the mid should the ask sit?
   - Same options and mapping as the buy side (sell_offset_bps / sell_offset_abs).

6. Liquidity — don't ask me for amounts; read them from my wallet. Get my
   operator wallet address (ask me for the public 0x... address — never the
   private key), then read each side's input-token balance via the RPC:
   debt-token balanceOf for the buy side, collateral-token balanceOf for the
   sell side. Tell me what you found (human-readable) and ask me to confirm
   quoting my full balance per side, or to enter a smaller amount. Map the
   confirmed values to buy_total_liquidity_debt and
   sell_total_liquidity_collateral (balanceOf is already atomic — no decimal
   conversion). If a side's balance is 0, tell me to fund it and skip that side.

Never ask about these — map the answers above to the primary parameters and
leave every advanced parameter at its default. If you can't tell whether a field
is primary or advanced, ask me rather than guess.

Auto-fill (never ask):
- Reactor: use the reactor value already in the downloaded stitch.example.toml
  (we prefill it per deployment). If it's still the zero address, ask me for the
  deployed reactor address for my chain (free-form) before writing config — never
  write a zero reactor.
- subgraph_url: set to https://api.textilecredit.com/subgraph?chainId=<my chain
  id> (the Textile proxy). Both legs run by default, so always set it.
- Settlement closing (blue leg) runs by default on the chosen pool: closer_pool =
  the chosen pool address; floor_ray / buffer_ray / window_secs from the pool's
  floorFee / buffer / window in GraphQL (defaults below if discovery misses);
  min_margin_collateral 0, max_positions_per_fill 10, discover_first 200,
  skip_past_window true.
- Advanced (defaults, never ask): minimum order slice, maximum orders per side,
  tick interval, order TTL, refresh threshold, indexer URL, price feed, Permit2.

Use these defaults unless I provide different values:
- Binary command name: stitch
- GitHub repo: textile-protocol/textile-stitch
- Network: Base
- Chain ID: 8453
- RPC URL: https://mainnet.base.org
- Textile indexer URL: https://api.textilecredit.com
- Price feed URL: https://api.textilecredit.com/price
- Permit2 address: 0x000000000022D473030F116dDEE9F6B43aC78BA3
- Reactor address: from stitch.example.toml (prefilled per deployment); ask only
  if it's still the zero address
- Subgraph URL: https://api.textilecredit.com/subgraph?chainId=<chain id> (the
  Textile proxy); never ask
- Price feed staleness: 30 seconds
- Tick interval: 5 seconds
- Token decimals: 6, unless I provide different decimals
- Buy spread: 0.1% (10 bps)
- Sell spread: 0.1% (10 bps)
- Buy total liquidity: from my wallet's debt-token balance (don't ask)
- Sell total liquidity: from my wallet's collateral-token balance (don't ask)
- Minimum order slice (advanced, never ask): 10 units of the debt/stable token
- Maximum ladder orders per side (advanced, never ask): 150
- Order TTL: 30 seconds
- Refresh threshold: 10 bps
- Settlement closing: enabled by default (both legs)
- Floor rate (floor_ray): 500000000000000000000000 (0.05%), if discovery didn't
  supply it
- Buffer rate (buffer_ray): 20000000000000000000000000 (2%), if discovery didn't
  supply it
- Settlement close window: 432000 seconds, if discovery didn't supply it
- Minimum close margin: 0 (advanced)
- Max positions per fill: 10 (advanced)
- Discover first: 200 (advanced)
- Skip past-window positions: true (advanced)
- Foreground config directory on macOS/Linux: ~/Stitch
- Foreground config path on macOS/Linux: ~/Stitch/stitch.toml
- Foreground env path on macOS/Linux: ~/Stitch/stitch.env
- Foreground key path on macOS/Linux: ~/Stitch/stitch.key
- Linux systemd config directory: /etc/stitch-bot
- Linux systemd config path: /etc/stitch-bot/stitch.toml
- Linux systemd env path: /etc/stitch-bot/stitch.env
- Linux systemd key path: /etc/stitch-bot/stitch.key
- Linux service name: stitch
- macOS config directory: ~/Stitch
- macOS config path: ~/Stitch/stitch.toml
- macOS env path: ~/Stitch/stitch.env
- macOS key path: ~/Stitch/stitch.key
- Windows config directory: %USERPROFILE%\Stitch
- Windows config path: %USERPROFILE%\Stitch\stitch.toml
- Windows env path: %USERPROFILE%\Stitch\stitch.env
- Windows key path: %USERPROFILE%\Stitch\stitch.key

Before editing files, first detect the OS, architecture, shell, package
environment, and available terminal/question tools.

Gather operator settings via the interview above. For any field not covered by a
question, use the values from "Use these defaults unless I provide different
values" above, unless I override. Do not ask me to paste pool or token
addresses: take them from deposit catalog discovery (below), and only ask if
discovery fails.

Discover pool and token addresses (after chain is confirmed, before pool pick):
The public deposit picker at https://app.textilecredit.com/s/deposit lists the
same settlement pools Stitch can quote. Propose to query that catalog for me
instead of guessing addresses.

Local Hardhat chain (31337) — skip catalog discovery entirely. The public
deposit catalog only lists production chains, so chain 31337 will always come
back empty; do not query it and do not treat the empty result as a failure.
Instead get pool and token addresses from local deploy output (for example a
deployments/addresses file, broadcast/run-latest.json from the deploy script, or
the addresses the deploy printed), or ask me for them one at a time via
AskUserQuestion. Then continue to the pool pick (step 3) as usual.

Preferred — GraphQL (same data as the deposit page):
1. POST to https://app.textilecredit.com/api/graphql with Content-Type:
   application/json.
2. Use the chain ID I confirmed in the interview (default 8453 / Base).
3. Example query:

   query SettlementV3PoolsForStitch($chainId: Int!) {
     settlementV3Pools(chainId: $chainId, includeUnlisted: false) {
       address
       chainId
       collateralAsset
       debtAsset
       displayName
       buffer
       floorFee
       window
     }
   }

4. Example request body (replace chainId if I chose another network):

   {"query":"query SettlementV3PoolsForStitch($chainId: Int!) { settlementV3Pools(chainId: $chainId, includeUnlisted: false) { address chainId collateralAsset debtAsset displayName buffer floorFee window } }","variables":{"chainId":8453}}

5. Map each pool to stitch.toml fields:
   - collateral token address = collateralAsset (soft / collateral column on
     the deposit page)
   - debt token address = debtAsset (stable / Supply column on the deposit page)
   - closer_pool = pool address (closing runs by default)

6. Present a short table: displayName or pair label, chainId, supply/debt
   symbol hint if known, collateralAsset, debtAsset, pool address. Then ask
   which row to operate on with AskUserQuestion (interview step 3;
   recommended = first listed pool or the one I name).

Fallback — browse the deposit page:
- Open https://app.textilecredit.com/s/deposit (or use a browser/scrape tool).
- Match pools on my chosen chain ID.
- Columns: Supply ≈ debt/stable token; Collateral ≈ soft/collateral token.
- Pool detail URLs look like /s/deposit/{chainId}/{poolAddress}; use the pool
  address from the link or GraphQL when enabling settlement closing.
- If the page shows symbols but not addresses, still use the GraphQL path above
  or ask me to confirm after you show the table from a successful query.

If discovery returns no pools for my chain, stop and explain before writing
config — unless I'm on chain 31337, where an empty catalog is expected and you
should use the local-address flow above instead of stopping. If discovery
succeeds, confirm the pool via AskUserQuestion rather than asking me to re-type
addresses.

Token decimals after discovery:
- Default 6 for both tokens when symbols look like USDT/USDC-style stables.
- If unsure, read decimals() from each token contract via the gathered RPC URL.

Settlement closing (runs by default — don't ask whether to enable it):
- subgraph_url = https://api.textilecredit.com/subgraph?chainId=<my chain id>
  (the Textile proxy). Never ask me for a subgraph URL.
- closer_pool = chosen pool address from discovery.
- floor_ray and buffer_ray from the pool's floorFee and buffer in GraphQL when
  present; otherwise default to floor_ray = 500000000000000000000000 (0.05%
  opening rate) and buffer_ray = 20000000000000000000000000 (2% buffer), the
  current production values.
- window_secs 432000, min_margin_collateral 0, max_positions_per_fill 10,
  discover_first 200, skip_past_window true. These are advanced — leave them at
  the defaults; I can edit the config file if I want to change them.

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

3. Token amounts are atomic (uint256 strings).
   - buy_total_liquidity_debt / sell_total_liquidity_collateral = the wallet
     balances I confirmed in the interview (balanceOf is already atomic — no
     conversion).
   - buy_min_slice_debt / sell_min_slice_debt = the default (advanced), e.g.
     "10000000" for a 10-unit minimum on a 6-decimal token.

4. Settlement closing runs by default:
   - Set subgraph_url to the Textile proxy
     (https://api.textilecredit.com/subgraph?chainId=<chain id>).
   - Include the closer fields: closer_pool, floor_ray, buffer_ray, window_secs,
     min_margin_collateral, max_positions_per_fill, discover_first,
     skip_past_window.
   - Only omit these if I explicitly ask to run the green leg only.

5. Restrict config file permissions:
   - Unix: config directory 700, config file 600.

Private key collection (needs a REAL interactive terminal):
This step hides input, which needs a real TTY. Most coding agents do NOT have an
interactive terminal in their own tool sandbox — running the script there just
hangs or errors with "stty: stdin isn't a terminal" and I never see a prompt.
Do not announce that you launched a TTY unless keystrokes actually reach the
script. Pick the first option that genuinely gives a TTY:

1. If you have a tool that opens a real interactive terminal, run the script
   there and wait for me to finish.
2. macOS without such a tool — open my Terminal.app on a saved script:

     KEY_SCRIPT="$(mktemp -t stitch-key.XXXXXX.sh)"
     cat > "$KEY_SCRIPT" <<'EOF'
     <paste the Unix script below>
     EOF
     chmod 700 "$KEY_SCRIPT"
     open -a Terminal "$KEY_SCRIPT"

   Tell me to type the key in the Terminal window that opens, then wait. Remove
   the script after I confirm the env file exists.
3. If you cannot open any terminal, print the commands for me to paste into my
   own terminal. Make the line that RUNS the script the last line you give me,
   so the hidden prompt reads my keystrokes and not the rest of the paste.

The script reads from /dev/tty (not piped stdin) so it works under `open -a
Terminal` and when pasted. Do not proceed until I confirm the key and env files
exist.

Unix and macOS private-key script:

   #!/usr/bin/env sh
   set -eu

   CONFIG_DIR="${HOME}/Stitch"
   ENV_FILE="${CONFIG_DIR}/stitch.env"
   KEY_FILE="${CONFIG_DIR}/stitch.key"

   mkdir -p "$CONFIG_DIR"
   chmod 700 "$CONFIG_DIR"

   printf 'Enter STITCH_PRIVATE_KEY: ' > /dev/tty
   stty -echo < /dev/tty
   IFS= read -r key < /dev/tty
   stty echo < /dev/tty
   printf '\n' > /dev/tty

   if [ -z "$key" ]; then
     echo "No private key entered; not writing key file."
     exit 1
   fi

   umask 077
   printf '%s\n' "$key" > "$KEY_FILE"
   unset key
   chmod 600 "$KEY_FILE"
   printf 'STITCH_PRIVATE_KEY_FILE=%s\n' "$KEY_FILE" > "$ENV_FILE"
   chmod 600 "$ENV_FILE"

   echo "Wrote $KEY_FILE and $ENV_FILE with restricted permissions." > /dev/tty

Windows PowerShell private-key script:

   $ConfigDir = "$env:USERPROFILE\Stitch"
   $EnvFile = "$ConfigDir\stitch.env"
   $KeyFile = "$ConfigDir\stitch.key"

   New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null

   $key = Read-Host "Enter STITCH_PRIVATE_KEY" -AsSecureString
   $bstr = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($key)
   $plain = [Runtime.InteropServices.Marshal]::PtrToStringAuto($bstr)

   if ([string]::IsNullOrWhiteSpace($plain)) {
     Write-Error "No private key entered; not writing key file."
     exit 1
   }

   $plain | Set-Content -NoNewline -Path $KeyFile
   "STITCH_PRIVATE_KEY_FILE=$KeyFile" | Set-Content -NoNewline -Path $EnvFile
   [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($bstr)
   $plain = $null

   Write-Host "Wrote $KeyFile and $EnvFile."

Permit2 approvals (after the env file exists, before the dry run):
The operator wallet must approve Permit2 to pull each token Stitch quotes as
order input — debt on the buy side, collateral on the sell side. Without it,
orders post but silently fail to fill, and a live start refuses to run. The
binary handles this; do not ask me to paste token addresses or send raw approve
transactions.

1. Load STITCH_PRIVATE_KEY_FILE from the env file into the process environment
   (same as the dry run below — never put the private key itself on the command
   line).
2. Check what's needed without sending anything:

   stitch approve --config "<config-path>" --dry-run

3. If it reports tokens that are already approved, skip the rest. Only if at
   least one token still needs approval, ask me how much to approve with
   AskUserQuestion (recommended first):
   - Maximum (recommended) — approve once, never re-approve. Standard for a
     market maker; you're approving the canonical audited Permit2 contract, and
     the reactor can only pull against orders I actually signed.
   - Exact amount — approve only the liquidity in my config.

   If I choose Exact, warn me clearly before running it: an exact allowance is
   consumed as orders fill, so once it's used up Stitch keeps posting orders that
   silently fail to fill until I re-approve. I also have to re-run `stitch
   approve` every time I raise my configured liquidity. Max avoids both.

4. Run the approval (omit `--exact` for Maximum):

   stitch approve --config "<config-path>"            # maximum
   stitch approve --config "<config-path>" --exact    # exact amount

   Each approval is one on-chain transaction per token and costs gas, so the
   operator wallet needs a small native balance. The command is idempotent —
   it skips tokens already approved.

Dry run:
- Run dry run only after config and env file exist.
- Load STITCH_PRIVATE_KEY_FILE from the env file into the process environment.
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
- Reactor address (from stitch.example.toml).
- Chosen pool (display name or pair) and how it was discovered (GraphQL vs
  deposit page).
- Collateral and debt token addresses, decimals, and human-readable symbols if
  known.
- Buy and sell spread (the percentage or absolute I picked).
- Buy-side and sell-side liquidity, read from my wallet balances (human +
  atomic).
- Settlement closing: on by default, with the closer pool and the subgraph proxy.
- Permit2 approvals: which input tokens are approved, and whether I chose
  maximum or exact.
- Dry-run result.

Then remind me that advanced parameters (minimum order slice, maximum orders,
tick interval, TTL, refresh threshold, and the closer's margin / positions /
discover / skip settings) are at safe defaults, and that I can edit the config
file directly and restart to change any of them.

Then use AskUserQuestion for explicit confirmation before any live or
persistent operation. Make it multiple choice, recommended first:
   - Not yet — review the summary first (recommended).
   - Start a foreground live run.
   - Install and start the background service I picked (launchd / systemd / Task
     Scheduler / Windows service).
Do not start until I pick a start option after a successful dry run.

Service setup after confirmation only:
- macOS: If I choose launchd, create a LaunchAgent plist that runs
  stitch --config ~/Stitch/stitch.toml, and make sure the env file is loaded
  without putting the private key itself in command arguments.
- Linux: If I confirm systemd, install a stitch.service that loads non-secret
  env via EnvironmentFile and the private key via LoadCredential, then run sudo
  systemctl daemon-reload and sudo systemctl enable --now stitch.
- Windows: If I choose Task Scheduler, create a startup task. If I choose a
  Windows service, ask before installing or using NSSM.

Never start any live operation until I explicitly confirm after a successful dry
run.
```
