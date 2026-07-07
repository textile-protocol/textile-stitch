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
- For an MPC signer (Turnkey/MPCVault), the same secret-handling rules apply to its
  credentials: never paste the Turnkey API private key or the MPCVault API token into
  chat or a question tool, never put them in stitch.toml, logs, or command arguments,
  collect them via a local hidden terminal prompt, and write them to a file with
  chmod 600. The Turnkey API public key and the non-secret signer fields (org id,
  vault uuid, addresses) are not secret and can be handled normally.
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

0. Signer / wallet type — ASK THIS FIRST, before the numbered questions, because it
   changes which secrets to collect and whether to write a `[signer]` section.
   "How should Stitch hold its operator key?"
   - Options (recommended first): Hotwallet (local private key); Turnkey (MPC, no
     extra infrastructure); MPCVault (MPC, needs a sidecar container).
   - Hotwallet is the default and simplest. Turnkey and MPCVault keep the key in an
     MPC wallet so the bot never holds raw key material. If unsure, pick Hotwallet.
   - Remember the answer. It selects the credential-collection path below and whether
     to add a `[signer]` section to stitch.toml. MPCVault also requires standing up a
     client-signer sidecar (see "MPC signer credentials").

1. Chain — which network should Stitch run on?
   - Options (3 max): BNB Smart Chain — 56 (recommended); Ethereum — 1; Another
     network. If I pick "Another network", ask a follow-up with the rest as
     options (Celo — 42220; Local Hardhat — 31337 for debugging), and only list a
     public chain if it's on https://app.textilecredit.com/s/deposit. For anything
     else I'll type a custom chain ID in the free-form answer.
   - After I answer, set chain_id and RPC URL to a free public endpoint: 56 ->
     https://bsc-rpc.publicnode.com, 1 -> https://ethereum-rpc.publicnode.com,
     42220 -> https://celo-rpc.publicnode.com, 31337 -> http://127.0.0.1:8545. For
     other chains, ask a follow-up RPC URL question if you don't know the right
     default. Never use a paid or API-key RPC endpoint; a free, well-performing
     public RPC is always fine. On 31337, skip public catalog discovery and use
     local pool/token addresses (see discovery section).

2. Run mode — how should Stitch run? Offer the choices for my OS, recommended
   first:
   - macOS: Foreground / manual; launchd background service.
   - Linux: systemd service; Foreground / manual.
   - Windows: Foreground / manual; Task Scheduler; Windows service.

3. Pool — after discovery (below), present pools as options (display name or pair
   label, recommended = first listed) so I pick a row instead of typing an
   address. Show at most 3; if there are more, show the first 3 and let me name
   another via the free-form answer. If discovery returned nothing on a real
   chain, stop; on 31337 use the local-address flow.

4. Buy spread — how far below the mid should the bid sit?
   - Options: 0.1% (recommended); 0.25%; 0.5%. Or type an absolute like "5 cNGN"
     or "5 NGN" (soft units per stable). Mapping: a percentage X% ->
     buy_offset_bps = X * 100 (0.1% -> 10); an absolute N -> buy_offset_abs = N.

5. Sell spread — how far above the mid should the ask sit?
   - Same options and mapping as the buy side (sell_offset_bps / sell_offset_abs).

6. Liquidity — don't ask me for amounts; default both market-making sides to
   `"max"` so Stitch quotes the currently funded wallet inventory on each tick.
   Set buy_total_liquidity_debt = "max" and
   sell_total_liquidity_collateral = "max". After the config is written, use
   the dry run and approval preflight to surface missing balances or approvals
   for each enabled side.

Never ask about these — map the answers above to the primary parameters and
leave every advanced parameter at its default. If you can't tell whether a field
is primary or advanced, ask me rather than guess.

Auto-fill (never ask):
- Reactor: use the reactor value already in the downloaded stitch.example.toml
  (we prefill it per deployment). If it's still the zero address, ask me for the
  deployed reactor address for my chain (free-form) before writing config — never
  write a zero reactor.
- Advanced (defaults, never ask): minimum order slice, maximum orders per side,
  tick interval, order TTL, refresh threshold, indexer URL, price feed, Permit2.

Use these defaults unless I provide different values:
- Binary command name: stitch
- GitHub repo: textile-protocol/textile-stitch
- Network: BNB Smart Chain
- Chain ID: 56
- RPC URL: https://bsc-rpc.publicnode.com (a free public endpoint; never a paid or
  API-key RPC)
- Textile indexer URL: https://api.textilecredit.com
- Price feed URL: https://api.textilecredit.com/price
- Permit2 address: 0x000000000022D473030F116dDEE9F6B43aC78BA3
- Reactor address: from stitch.example.toml (prefilled per deployment); ask only
  if it's still the zero address
- Price feed staleness: 30 seconds
- Tick interval: 5 seconds
- Token decimals: 6, unless I provide different decimals
- Buy spread: 0.1% (10 bps)
- Sell spread: 0.1% (10 bps)
- Buy total liquidity: "max" (quote the currently funded debt-token balance)
- Sell total liquidity: "max" (quote the currently funded collateral-token balance)
- Minimum order slice (advanced, never ask): 10 units of the debt/stable token
- Maximum ladder orders per side (advanced, never ask): 40
- Order TTL: 30 seconds
- Refresh threshold: 10 bps
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
2. Use the chain ID I confirmed in the interview (default 56 / BNB Smart Chain).
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

   {"query":"query SettlementV3PoolsForStitch($chainId: Int!) { settlementV3Pools(chainId: $chainId, includeUnlisted: false) { address chainId collateralAsset debtAsset displayName buffer floorFee window } }","variables":{"chainId":56}}

5. Map each pool to stitch.toml fields:
   - collateral token address = collateralAsset (soft / collateral column on
     the deposit page)
   - debt token address = debtAsset (stable / Supply column on the deposit page)

6. Present a short table: displayName or pair label, chainId, supply/debt
   symbol hint if known, collateralAsset, debtAsset, pool address. Then ask
   which row to operate on with AskUserQuestion (interview step 3;
   recommended = first listed pool or the one I name).

Fallback — browse the deposit page:
- Open https://app.textilecredit.com/s/deposit (or use a browser/scrape tool).
- Match pools on my chosen chain ID.
- Columns: Supply ≈ debt/stable token; Collateral ≈ soft/collateral token.
- Pool detail URLs look like /s/deposit/{chainId}/{poolAddress}.
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

3. Token amounts are atomic (uint256 strings), except the market-making total
   liquidity fields default to the `"max"` sentinel.
   - buy_total_liquidity_debt / sell_total_liquidity_collateral = "max" unless
     I explicitly ask for fixed numeric caps.
   - buy_min_slice_debt / sell_min_slice_debt = the default (advanced), e.g.
     "10000000" for a 10-unit minimum on a 6-decimal token.

4. Signer section: if I chose Turnkey or MPCVault, add a `[signer]` section to
   stitch.toml with `provider` and the non-secret fields gathered in the
   credentials step (Turnkey: organization_id, sign_with, operator_address;
   MPCVault: vault_uuid, client_signer_pubkey, operator_address,
   callback_listen_addr). Omit `[signer]` entirely for the hotwallet. Never put any
   signer secret in the TOML.

5. Restrict config file permissions:
   - Unix: config directory 700, config file 600.

Private key collection (HOTWALLET ONLY; needs a REAL interactive terminal):
If I chose Turnkey or MPCVault, SKIP this whole private-key step and use "MPC signer
credentials" below instead. For the hotwallet:
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

MPC signer credentials (Turnkey or MPCVault; use instead of the private key step):
Collect the non-secret signer fields with AskUserQuestion and put them in the
`[signer]` section of stitch.toml. Collect the secret(s) the same hidden-terminal /
saved-script way as the private key above (real TTY, never in chat or arguments),
write each secret to its own file with chmod 600, and put only the `*_FILE` path (and
non-secret env) in the env file. Then continue to Permit2 approvals and the dry run
as usual; `set -a; . env; set +a` loads whatever the env file contains.

Turnkey:
- Ask for: organization_id, sign_with (wallet account address or private key id),
  operator_address (the EVM address it resolves to). api_base_url defaults to
  https://api.turnkey.com.
- Secret: the API private key. Write it to e.g. ~/Stitch/turnkey-api.key (chmod 600).
- Env file: `TURNKEY_API_PRIVATE_KEY_FILE=<path>` and `TURNKEY_API_PUBLIC_KEY=<pubkey>`
  (the public key is not secret). No STITCH_PRIVATE_KEY.
- stitch.toml: `[signer]` with `provider = "turnkey"` plus organization_id, sign_with,
  operator_address.

MPCVault (also needs a client-signer sidecar container — this is required infra,
not optional; the bot cannot sign without it):
- Ask for: vault_uuid, operator_address (the vault wallet's EVM address),
  callback_listen_addr (default 0.0.0.0:8088). Do NOT ask for client_signer_pubkey
  yet — it's generated in step 1 below, then written into stitch.toml.
- Secret: the MPCVault API token. Write it to e.g. ~/Stitch/mpcvault-api.token
  (chmod 600). Env file: `MPCVAULT_API_TOKEN_FILE=<path>`. No STITCH_PRIVATE_KEY.
- Keep the host part of callback_listen_addr as `0.0.0.0` (not 127.0.0.1) so the
  sidecar container can reach the bot on the host.
- stitch.toml: `[signer]` with `provider = "mpcvault"` plus vault_uuid,
  client_signer_pubkey (from step 1), operator_address, callback_listen_addr.

Stand up the client-signer sidecar BEFORE the dry run. It must be running and its
key-grant approved or every signature fails closed and the dry run can't sign.

  0. Prerequisite — Docker. Confirm `docker version` works and the daemon is
     running (on Linux, that I can run docker, or use sudo consistently). If Docker
     is missing, STOP and ask me to install Docker Desktop (macOS/Windows) or the
     Docker engine (Linux), then resume. Do not attempt MPCVault signing without it.
  1. Generate the sidecar key (no passphrase):
     `ssh-keygen -t ed25519 -C "mpcvault-client-signer" -f ~/Stitch/client-signer-key -N ""`
     then `chmod 600 ~/Stitch/client-signer-key`. Put the contents of
     `~/Stitch/client-signer-key.pub` into `client_signer_pubkey` in stitch.toml.
  2. Register that public key in the MPCVault console (vault → Team & policies →
     New Client Signer), then approve BOTH the vault-setting update and the
     "Key grant access" request in the MPCVault mobile app. Wait for me to confirm
     I approved them — the sidecar can't join MPC signing until I do.
  3. Pick the callback URL the container uses to reach the bot on the host:
       - macOS / Windows (Docker Desktop): http://host.docker.internal:<port>
       - Linux: same URL, but add `--add-host=host.docker.internal:host-gateway`
         to the docker run below (Linux Docker doesn't resolve that name by default).
     `<port>` is the port from callback_listen_addr (default 8088). The callback
     flows OUT of the container to the host, so no inbound `-p` mapping is needed
     for it (the `-p 8080:8080` below is only the sidecar's own health endpoint).
  4. Write `~/Stitch/mpcvault-signer/config.yml`, then `chmod 600` it (it holds the
     sidecar private key):

         http-health:
           listening-addr: 0.0.0.0:8080
         vault-uuid: "<vault_uuid>"
         ssh:
           private-key: |
             <full contents of ~/Stitch/client-signer-key, indented under this key>
           password: ""
         callback-url: "http://host.docker.internal:8088/callback"

  5. Run the sidecar next to the bot (include the `--add-host` line on Linux only):
     `docker run -d --name mpcvault-signer --restart unless-stopped \`
     `  --add-host=host.docker.internal:host-gateway \`
     `  -p 8080:8080 -v ~/Stitch/mpcvault-signer/config.yml:/config.yml:ro \`
     `  ghcr.io/mpcvault/client-signer:latest --config-path=/config.yml`
  6. Verify before continuing: `docker ps` shows mpcvault-signer Up, and
     `docker logs mpcvault-signer` shows it connected with no auth/key-grant errors.
     A key-grant/permission error means the step-2 approval isn't done — wait for it.
  7. Boot persistence: `--restart unless-stopped` only helps if Docker starts on
     boot. For a background/service bot install, enable Docker on boot (Linux:
     `sudo systemctl enable docker`).
  See ADVANCED.md ("MPCVault sidecar") for the same steps in reference form.

Permit2 approvals (after the env file exists, before the dry run):
The operator wallet must approve Permit2 to pull each token Stitch quotes as
order input — debt on the buy side, collateral on the sell side. Without it,
orders post but silently fail to fill, and a live start refuses to run. The
binary handles this; do not ask me to paste token addresses or send raw approve
transactions.

1. Load the signer credentials from the env file into the process environment
   (STITCH_PRIVATE_KEY_FILE for the hotwallet, or the TURNKEY_*/MPCVAULT_* vars for
   an MPC signer; same as the dry run below). Never put a secret on the command line.
   For MPCVault, the client-signer sidecar must be running so the bot can sign.
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
- Load the signer credentials from the env file into the process environment
  (STITCH_PRIVATE_KEY_FILE for the hotwallet, or TURNKEY_*/MPCVAULT_* for an MPC
  signer). For MPCVault, make sure the client-signer sidecar is running first.
- Do not print any secret.
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
- Signer backend: hotwallet, Turnkey, or MPCVault (for MPCVault, confirm the
  client-signer sidecar is running and reachable at callback_listen_addr).
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
- Buy-side and sell-side liquidity: "max", plus any dry-run balance or approval
  warnings for those sides.
- Permit2 approvals: which input tokens are approved, and whether I chose
  maximum or exact.
- Dry-run result.

Then remind me that advanced parameters (minimum order slice, maximum orders,
tick interval, TTL, and refresh threshold) are at safe defaults, and that I can edit the config
file directly and restart to change any of them.

Then use AskUserQuestion for explicit confirmation before any live or
persistent operation. Make it multiple choice, recommended first:
   - Not yet — review the summary first (recommended).
   - Start a foreground live run.
   - Install and start the background service I picked (launchd / systemd / Task
     Scheduler / Windows service).
Do not start until I pick a start option after a successful dry run.
After the install flow, tell me how to operate Stitch later:
- Claude Code skill: run `/stitch`.
- Codex skill: ask `Use the stitch skill to run Stitch`.
- No skill: run `stitch --config <config-path>` after loading the env file.

Service setup after confirmation only:
- macOS: If I choose launchd, create a LaunchAgent plist that runs
  stitch --config ~/Stitch/stitch.toml, and make sure the env file is loaded
  without putting the private key itself in command arguments.
- Linux: If I confirm systemd, install a stitch.service that loads non-secret
  env via EnvironmentFile and the private key via LoadCredential, then run sudo
  systemctl daemon-reload and sudo systemctl enable --now stitch.
- Windows: If I choose Task Scheduler, create a startup task. If I choose a
  Windows service, ask before installing or using NSSM.
- MPCVault only: a background/service bot also needs the client-signer sidecar
  running and surviving reboots. The `docker run --restart unless-stopped` from the
  sidecar step covers restarts as long as Docker starts on boot (Linux: `sudo
  systemctl enable docker`). Confirm the sidecar is Up before enabling the bot
  service, and remind me the bot can't sign while the sidecar is down.

Never start any live operation until I explicitly confirm after a successful dry
run.
```
