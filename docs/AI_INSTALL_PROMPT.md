# AI Install Prompt For Stitch

Copy this prompt into Claude, GPT, Codex, or another coding agent that has
terminal access to the machine where Stitch should run.

This installs the **Stitch web UI** (`stitch-panel`) only. Bot corridors, wallet
keys, spreads, Permit2 approvals, dry runs, and live starts happen in the browser
afterward — the agent does not configure bots.

Coding agents often block `curl ... | sh`. The prompt below downloads
`install-panel.sh` to disk first, then runs it from that file.

```text
You are helping me install Textile Stitch, the operator web UI at:
https://github.com/textile-protocol/textile-stitch

Goal:
- Install the Stitch panel (Docker web UI) on this machine.
- Open the web app in a browser.
- Stop there. I add bots, paste wallet keys, set spreads, approve tokens, dry-run,
  and go live myself inside the web UI.

Hard rules:
- Do not clone https://github.com/textile-protocol/textile-stitch unless I ask.
- Do not build from source unless I explicitly request a source install.
- Do not install the standalone `stitch` binary as the install path. The panel
  pulls bot images itself.
- Do not write stitch.toml, collect STITCH_PRIVATE_KEY / signer secrets, run
  `stitch approve`, run dry runs, start bots, or install launchd/systemd/Task
  Scheduler services for a bot.
- Never ask me to paste a bot wallet private key into chat or a question tool.
- Never pipe a remote script into an interpreter. Forbidden: `curl ... | sh`,
  `curl ... | bash`, `irm ... | iex`. Download install-panel.sh to a local file,
  then run it from disk.
- If Docker is missing or the daemon is unreachable, stop and tell me how to
  install/start Docker, then wait.
- If the install fails, stop and explain. Do not invent a hand-rolled compose
  stack unless install-panel.sh is unavailable and I ask you to continue from
  docs/install-panel.md.

Question tool rules:
- Use AskUserQuestion for every non-secret question when that tool exists. If the
  exact name differs (AskUserQuestionTool, request_user_input, AskQuestion), use
  the equivalent — never substitute plain chat for a question the tool can ask.
- Ask one question per tool call and wait for my answer before the next.
- Make every question multiple choice with at most 3 concrete options. Put the
  recommended value first and label it "(recommended)".
- The tool adds a free-form answer automatically. Don't add your own "Other".
- Never ask for STITCH_PRIVATE_KEY or other bot secrets through a question tool.

Install interview (AskUserQuestion, one question at a time):
Ask ONLY this before installing:

1. Where are you installing Stitch?
   - Options: Local computer (recommended) — password login at
     http://127.0.0.1:8420 on this machine only; Server — Tailscale, so you can
     open Stitch from your other devices on the tailnet.
   - Local computer → password auth, loopback bind. No Tailscale. Use this on
     macOS (Apple Silicon or Intel) and on Linux laptops.
   - Server → Tailscale sidecar on a Linux Docker host, no host port published.
     Prefer local on a Mac; server mode expects Linux (/dev/net/tun).
   - Remember the answer. It selects PANEL_MODE=local or PANEL_MODE=server for
     the installer.

Do not ask about corridors, pools, spreads, liquidity, chain IDs, RPC URLs,
signers, or bot wallet keys. Those are configured in the web app after install.

Defaults:
- GitHub repo: textile-protocol/textile-stitch
- Installer: install-panel.sh from the latest main (or the release docs point at)
- Install dir: ~/stitch-panel
- Local bots dir default: ~/stitch-bots
- Server bots dir default: /srv/stitch/bots
- Panel image: ghcr.io/textile-protocol/textile-stitch-panel:latest

Install procedure:
1. Confirm Docker works: `docker compose version` and `docker info`. If either
   fails, stop and help me fix Docker first.

2. Download the installer to a local file (do not pipe to sh):

   INSTALLER_URL="https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/install-panel.sh"
   INSTALLER_PATH="$(mktemp -t stitch-install-panel.XXXXXX.sh)"
   curl --proto '=https' --tlsv1.2 -fsSL "$INSTALLER_URL" -o "$INSTALLER_PATH"
   chmod 700 "$INSTALLER_PATH"

   Prefer a local copy if this machine already has the textile-stitch repo or the
   Textile monorepo's packages/stitch-bot/install-panel.sh — use that path instead
   of downloading.

3. Run the installer with the mode from question 1.

   Local computer:

     PANEL_MODE=local sh "$INSTALLER_PATH"

   The installer prompts for a panel login password (hidden). Do not put that
   password in chat. If you cannot give it a real TTY, set PANEL_PASSWORD in the
   environment for that one command only after collecting it via a local hidden
   prompt / saved Terminal script — never via AskUserQuestion.

   Server:

     PANEL_MODE=server sh "$INSTALLER_PATH"

   The installer prompts for a Tailscale auth key (reusable, not ephemeral) and
   the tailnet login(s) allowed in. Mint keys at
   https://login.tailscale.com/admin/settings/keys. Use the login from the
   Tailscale Users page, not a nickname.

4. Remove the temp installer file when finished: `rm -f "$INSTALLER_PATH"`.

5. Open the web app:
   - Local: http://127.0.0.1:8420 (log in with the panel password).
   - Server: the HTTPS URL the installer printed
     (https://stitch-panel.<tailnet>.ts.net), or discover it from the installer
     output / `tailscale status`.
   Prefer opening my browser for me when you have a tool for that
   (`open`, `xdg-open`, or equivalent).

After install — tell me this and stop:
- Stitch panel is up at <URL>.
- Next steps are in the web UI, not in this chat:
  1. Add a bot
  2. Pick a corridor
  3. Paste the operator wallet key (or configure Turnkey / MPCVault) in the UI
  4. Approve tokens
  5. Dry run, then start
- Do not offer to configure a bot from the terminal unless I explicitly ask later.
- Point me at docs/install-panel.md only if I need advanced deploy options
  (custom reverse proxy, building from source).
```
