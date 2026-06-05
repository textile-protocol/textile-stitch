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
- Before any live operation, confirm token balances and set up Permit2 approvals
  with `stitch approve` (see the Permit2 approvals step). A live start refuses to
  run until the input tokens are approved.

Question tool rules:
- Use AskUserQuestion for every non-secret operator question when that tool
  exists in your environment. If the exact name differs (AskUserQuestionTool,
  request_user_input, AskQuestion, etc.), use the equivalent question tool —
  never substitute plain chat for a question the tool can ask.
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
- Never ask for STITCH_PRIVATE_KEY through a question tool or chat.
- If no question tool exists, ask the same sequence as concise chat questions,
  still one at a time, with the same progress and recommended-first style.

Operator interview (AskUserQuestion, one question at a time):
Run this sequence after detecting OS/architecture and before writing config.
Skip a step only when discovery or defaults already fixed the value and you
only need a quick confirm question for that step.

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

3. Settlement closing — what should Stitch do?
   - Options: Market-making only (recommended); Enable blue-leg closing too.

4. If closing is enabled — ask for the subgraph URL (offer the known Goldsky
   subgraph for my chain as an option if you have it, otherwise free-form). It's
   independent of pool discovery. Defer the closer params (floor_ray, buffer_ray,
   window): pull them from pool discovery in step 6, and only ask for any value
   still missing from GraphQL, one question each.

5. Reactor address — no safe default. Offer the known filler reactor for my
   chain as an option if you have one; otherwise I'll paste it in the free-form
   answer.

6. Pool — after discovery, present pools as options (display name or pair label,
   recommended = first listed) so I pick a row instead of typing an address.
   Show at most 3; if there are more, show the first 3 and let me name another
   via the free-form answer. If discovery returned nothing on a real chain,
   stop; on 31337 use the local-address flow.

Market-making setup (steps 7-12 all have safe defaults). To avoid a long quiz,
ask one gate question first:
   "Use the recommended market-making defaults (150 bps each side, 50000 per
   side, 10 min slice, 150 max orders), or set each value yourself?"
   - Options: Use the defaults (recommended) — skip steps 7-12; Customize — ask
     steps 7-12 one at a time.

For steps 7-12, present the presets below (recommended first); I can also type a
custom value in the free-form answer.

7. Buy spread (bps below mid).
   - Options: 150 / 1.5% (recommended); 100 / 1.0%; 200 / 2.0%.

8. Sell spread (bps above mid).
   - Options: 150 / 1.5% (recommended); 100 / 1.0%; 200 / 2.0%.

9. Buy-side total liquidity (human amount, debt/stable token).
   - Options: 50000 (recommended); 10000; 100000.

10. Sell-side total liquidity (human amount, collateral/soft token).
    - Options: 50000 (recommended); 10000; 100000.

11. Minimum order slice (human amount, debt/stable units).
    - Options: 10 (recommended); 5; 25.

12. Maximum ladder orders per side.
    - Options: 150 (recommended); 50; 100.

Use defaults without asking only for values not listed above (indexer URL,
price feed URL, Permit2, tick interval, TTL, refresh threshold, platform paths)
unless I ask to change them.

Use these defaults unless I provide different values:
- Binary command name: stitch
- GitHub repo: textile-protocol/textile-stitch
- Network: Base
- Chain ID: 8453
- RPC URL: https://mainnet.base.org
- Textile indexer URL: https://api.textilecredit.com
- Price feed URL: https://api.textilecredit.com/price
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
AskUserQuestion. Then continue to the pool pick (step 6) as usual.

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
   - closer_pool (only if settlement closing is enabled) = pool address

6. Present a short table: displayName or pair label, chainId, supply/debt
   symbol hint if known, collateralAsset, debtAsset, pool address. Then ask
   which row to operate on with AskUserQuestion (interview step 6;
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

If settlement closing is enabled:
- Settlement pool address = chosen pool address from discovery.
- floor_ray and buffer_ray from the pool's floorFee and buffer in GraphQL when
  present; otherwise ask via AskUserQuestion.
- window_secs 432000, min_margin_collateral 0, max_positions_per_fill 10,
  discover_first 200, skip_past_window true unless I override in the interview.

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
   - buy_total_liquidity_debt from my buy-side total liquidity answer.
   - sell_total_liquidity_collateral from my sell-side total liquidity answer.
   - buy_min_slice_debt and sell_min_slice_debt from the minimum slice default.
   - Example: 50000 with 6 decimals becomes "50000000000".
   - Example: 10 with 6 decimals becomes "10000000".

4. If settlement closing is disabled:
   - Omit subgraph_url.
   - Omit closer fields such as closer_pool, floor_ray, buffer_ray, window_secs,
     min_margin_collateral, max_positions_per_fill, discover_first, and
     skip_past_window.

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
Terminal` and when pasted. Do not proceed until I confirm the env file exists.

Unix and macOS private-key script:

   #!/usr/bin/env sh
   set -eu

   CONFIG_DIR="${HOME}/.config/stitch"
   ENV_FILE="${CONFIG_DIR}/stitch.env"

   mkdir -p "$CONFIG_DIR"
   chmod 700 "$CONFIG_DIR"

   printf 'Enter STITCH_PRIVATE_KEY: ' > /dev/tty
   stty -echo < /dev/tty
   IFS= read -r key < /dev/tty
   stty echo < /dev/tty
   printf '\n' > /dev/tty

   if [ -z "$key" ]; then
     echo "No private key entered; not writing env file."
     exit 1
   fi

   umask 077
   printf 'STITCH_PRIVATE_KEY=%s\n' "$key" > "$ENV_FILE"
   unset key
   chmod 600 "$ENV_FILE"

   echo "Wrote $ENV_FILE with restricted permissions." > /dev/tty

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

Permit2 approvals (after the env file exists, before the dry run):
The operator wallet must approve Permit2 to pull each token Stitch quotes as
order input — debt on the buy side, collateral on the sell side. Without it,
orders post but silently fail to fill, and a live start refuses to run. The
binary handles this; do not ask me to paste token addresses or send raw approve
transactions.

1. Load STITCH_PRIVATE_KEY from the env file into the process environment (same
   as the dry run below — never on the command line).
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
- Chosen pool (display name or pair) and how it was discovered (GraphQL vs
  deposit page).
- Collateral and debt token addresses, decimals, and human-readable symbols if
  known.
- Buy-side and sell-side total liquidity (human amounts and atomic values in
  config).
- Settlement closing enabled or disabled.
- Permit2 approvals: which input tokens are approved, and whether I chose
  maximum or exact.
- Dry-run result.

Then use AskUserQuestion for explicit confirmation before any live or
persistent operation. Make it multiple choice, recommended first:
   - Not yet — review the summary first (recommended).
   - Start a foreground live run.
   - Install and start the background service I picked (launchd / systemd / Task
     Scheduler / Windows service).
Do not start until I pick a start option after a successful dry run.

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
