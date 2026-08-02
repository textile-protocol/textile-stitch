# Install — Desktop app (menu bar / tray)

Recommended when you're installing on the Mac or Windows machine in front of
you. No Docker and no terminal. For an always-on server, see
[install-panel.md](install-panel.md).

The desktop app (`stitch-desktop`) is a light menu-bar (macOS) or system-tray
(Windows / Linux) controller. On first launch it:

1. Creates a data directory (`~/Library/Application Support/Stitch` on macOS,
   `%APPDATA%\Stitch` on Windows, `~/.local/share/stitch` on Linux).
2. If you previously used `stitch-setup`, imports that config into
   `<data_root>/bots/<id>` (from `config-location`, `~/Stitch`, or the
   release folder) so the fleet isn't empty after the upgrade.
3. Generates a panel password and writes `panel.env` + `panel.password`.
4. Starts bundled `stitch-panel` with `STITCH_PANEL_RUNTIME=process` (local
   `stitch` child processes — not containers).
5. Opens `http://127.0.0.1:8420` in your browser.

From the menu / tray you can Open Stitch, Start / Stop the panel, toggle
**Start at login**, Copy panel password, Check for updates (releases page),
and Quit.

**Start at login** registers the OS login item (LaunchAgent on macOS, Run key
on Windows, XDG autostart on Linux). On boot the tray app starts quietly
(`--autostart` — no browser tab), brings the panel up, and the process runtime
restarts bots that were left on when you last stopped or quit the panel.

## Download

- **macOS**: [Stitch.dmg](https://github.com/textile-protocol/textile-stitch/releases/latest/download/Stitch.dmg) — drag to Applications, launch from there.
- **Windows**: unzip the release archive and run `stitch-desktop.exe`.
- **Linux**: extract and run `stitch-desktop`, or use the bundled `.desktop` entry.

Product page: [textilecredit.com/stitch](https://textilecredit.com/stitch).

## Server / Docker

For always-on hosts, use [install-panel.md](install-panel.md) (`install-panel.sh`
/ `.ps1`). That path still uses Docker Compose.
