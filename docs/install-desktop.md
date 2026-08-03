# Install — Desktop app (menu bar / tray)

Recommended when you're installing on the Mac or Windows machine in front of
you. No Docker and no terminal. For an always-on server, see
[install-panel.md](install-panel.md).

The desktop app (`stitch-desktop`) is a menu-bar (macOS) or system-tray
(Windows / Linux) controller with a small Settings window. On macOS it also
shows a Dock icon by default (you can hide it from Settings). On first launch
it:

1. Creates a data directory (`~/Library/Application Support/Stitch` on macOS,
   `%APPDATA%\Stitch` on Windows, `~/.local/share/stitch` on Linux).
2. If you previously used `stitch-setup`, imports that config into
   `<data_root>/bots/<id>` (from `config-location`, `~/Stitch`, or the
   release folder) so the fleet isn't empty after the upgrade.
3. Asks you to create a panel password (entered twice). Only an Argon2 hash is
   written to `panel.env` — no plaintext password file.
4. Starts bundled `stitch-panel` with `STITCH_PANEL_RUNTIME=process` (local
   `stitch` child processes — not containers).
5. Opens the control window and `http://127.0.0.1:8420` in your browser so you
   can sign in with that password.

The tray menu leads with a running/stopped status, then **Open Stitch panel**,
**Pause** / **Resume**, **Keep Mac awake** / **Keep computer awake**, updates,
**Settings**, and Quit. Login and Dock prefs live in the Settings window only;
keep-awake is also in Settings and persists across launches. When keep-awake is
on, the menu-bar / tray icon shows a small lightning badge. When a newer
desktop release is published, the control window shows an update banner and the
tray item becomes **Download update**. On macOS, **Hide Dock icon** (off by
default, in Settings) keeps the menu-bar icon and drops the Dock entry; the
preference is saved in `<data_root>/desktop-prefs.json`.

If you still have a legacy `panel.password` file from an older build, the app
asks you to choose a new password and deletes the cleartext file.

Closing the Settings window hides it — it does not quit. Use **Settings** in
the tray, or click the Dock icon when it has no visible windows.

**Start at login** (in Settings) registers the OS login item (LaunchAgent on
macOS, Run key on Windows, XDG autostart on Linux). On boot the tray app starts
quietly (`--autostart` — no browser tab, no control window), brings the panel
up, and the process runtime restarts bots that were left on when you last
stopped or quit the panel.

**Keep Mac / computer awake** (tray + Settings) holds an OS power assertion so
idle sleep does not stop the local panel and bots. Display sleep is still
allowed. The preference is stored in `desktop-prefs.json` and restored on
launch.

## Download

- **macOS**: [Stitch.dmg](https://github.com/textile-protocol/textile-stitch/releases/latest/download/Stitch.dmg) — drag to Applications, launch from there.
- **Windows**: unzip the release archive and run `stitch-desktop.exe`.
- **Linux**: extract and run `stitch-desktop`, or use the bundled `.desktop` entry.

Product page: [textilecredit.com/stitch](https://textilecredit.com/stitch).

## Server / Docker

For always-on hosts, use [install-panel.md](install-panel.md) (`install-panel.sh`
/ `.ps1`). That path still uses Docker Compose.
