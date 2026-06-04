# AI Install Prompt For Stitch

Copy this prompt into Claude, GPT, Codex, or another coding agent that has
terminal access to the machine where Stitch should run.

```text
You are helping me install and configure Textile Stitch, the operator bot at
https://github.com/textile-protocol/textile-stitch.

Goal:
- Install the latest Stitch release on this machine.
- Configure Stitch with my operator settings.
- Store secrets safely.
- Run a dry run first.
- Start a persistent background service only after I confirm.

Supported platforms:
- Linux: install the release binary and configure a systemd service named stitch.
- macOS: install the release binary and offer either a launchd service or a
  foreground/manual run, depending on what I want.
- Windows: install the release binary with PowerShell and offer either a Windows
  service, Task Scheduler startup task, or foreground/manual run, depending on
  what I want.

Security rules:
- Do not ask me to paste my private key into chat.
- When a private key is needed, give me a local terminal command that prompts
  for it without echoing and writes it to the platform-appropriate secret file.
- Keep the private key out of stitch.toml, shell history, logs, command
  arguments, and screenshots.
- Use a dedicated operator wallet.
- Before running live, remind me to confirm token balances and Permit2 approvals.

Use these defaults unless I provide different values:
- Binary name: stitch
- GitHub repo: textile-protocol/textile-stitch
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

Before editing files, first detect the OS and package environment. Then ask me:
- whether I want a persistent background service or a manual foreground run;
- chain ID;
- RPC URL;
- Textile indexer URL;
- Permit2 address;
- reactor address;
- price feed URL;
- collateral token address and decimals;
- debt token address and decimals;
- buy/sell spread and order sizing;
- whether settlement closing should be enabled;
- if closing is enabled: subgraph URL, settlement pool address, floor_ray,
  buffer_ray, window_secs, min margin, and max positions per fill.

Install instructions:
- On Linux or macOS, install with:
  curl --proto '=https' --tlsv1.2 -LsSf \
    https://github.com/textile-protocol/textile-stitch/releases/latest/download/stitch-installer.sh | sh
- On Windows PowerShell, install with:
  powershell -ExecutionPolicy Bypass -c "irm https://github.com/textile-protocol/textile-stitch/releases/latest/download/stitch-installer.ps1 | iex"
- If the installer does not put stitch on PATH, locate the installed binary or
  download the correct binary from the latest GitHub Release and place it in a
  standard user-accessible binary directory.

Configuration instructions:
1. Download the example config from:
   https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/stitch.example.toml
2. Create stitch.toml at the platform-appropriate config path using my values.
3. Create the secret env file at the platform-appropriate env path using a local
   hidden prompt for STITCH_PRIVATE_KEY.
4. Restrict permissions on the config and secret files. On Unix, chmod the env
   file 600.
5. Run:
   stitch --config <config-path> --dry-run
6. Show me a short dry-run summary and ask before starting any persistent
   service or live run.

Linux service instructions:
- Download the systemd unit from:
  https://raw.githubusercontent.com/textile-protocol/textile-stitch/main/deploy/stitch.service
- Install it to /etc/systemd/system/stitch.service.
- Run:
  sudo systemctl daemon-reload
  sudo systemctl enable --now stitch
  journalctl -u stitch -f

macOS service instructions:
- If I choose launchd, create a LaunchAgent plist that runs:
  stitch --config ~/.config/stitch/stitch.toml
- Load it with launchctl.
- Make sure STITCH_PRIVATE_KEY is available from the env file without exposing it
  in command arguments.

Windows service instructions:
- If I choose Task Scheduler, create a startup task that runs:
  stitch.exe --config "%ProgramData%\Stitch\stitch.toml"
- If I choose a Windows service, use an installed service manager such as NSSM
  only after asking me to confirm that dependency.
- Make sure STITCH_PRIVATE_KEY is available from the env file without exposing it
  in command arguments.

If anything fails, stop and explain the failure before retrying. Do not start
live mode until I explicitly confirm after a successful dry run.
```
