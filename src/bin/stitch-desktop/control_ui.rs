// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Inline HTML for the desktop control window (mirrors the tray menu).

/// Build the control-panel HTML. `hide_dock_row` is macOS-only in the menu;
/// pass false on other platforms to omit the Dock checkbox.
pub fn html(autostart: bool, hide_dock: bool, panel_running: bool, hide_dock_row: bool) -> String {
    let autostart_checked = if autostart { " checked" } else { "" };
    let hide_dock_checked = if hide_dock { " checked" } else { "" };
    let status = if panel_running {
        ("running", "Panel running")
    } else {
        ("stopped", "Panel stopped")
    };
    let dock_row = if hide_dock_row {
        format!(
            r#"<label class="check"><input type="checkbox" id="hide-dock"{hide_dock_checked}> Hide Dock icon</label>"#
        )
    } else {
        String::new()
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Stitch</title>
<style>
  :root {{
    color-scheme: light;
    --bg: #f4f7f6;
    --fg: #102a27;
    --muted: #5b6f6c;
    --teal: #14b8a6;
    --teal-dark: #0f766e;
    --border: #d5e2df;
    --card: #ffffff;
  }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0;
    font: 14px/1.4 -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    background: linear-gradient(160deg, #e8f7f4 0%, var(--bg) 45%, #eef2f1 100%);
    color: var(--fg);
    min-height: 100vh;
  }}
  main {{
    max-width: 360px;
    margin: 0 auto;
    padding: 28px 22px 24px;
  }}
  h1 {{
    margin: 0 0 4px;
    font-size: 22px;
    letter-spacing: -0.02em;
  }}
  .brand {{
    color: var(--teal-dark);
    font-weight: 700;
  }}
  .sub {{
    margin: 0 0 18px;
    color: var(--muted);
    font-size: 13px;
  }}
  .status {{
    display: inline-flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 18px;
    padding: 6px 10px;
    border-radius: 999px;
    background: var(--card);
    border: 1px solid var(--border);
    font-size: 12px;
    color: var(--muted);
  }}
  .dot {{
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: #94a3b8;
  }}
  .status.running .dot {{ background: var(--teal); }}
  .stack {{
    display: flex;
    flex-direction: column;
    gap: 8px;
  }}
  button, .check {{
    width: 100%;
    border: 1px solid var(--border);
    background: var(--card);
    color: var(--fg);
    border-radius: 10px;
    padding: 10px 12px;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }}
  button.primary {{
    background: var(--teal);
    border-color: var(--teal);
    color: #042f2e;
    font-weight: 600;
  }}
  button.danger {{
    color: #9f1239;
  }}
  button:hover, .check:hover {{
    border-color: #9fb8b3;
  }}
  button:active {{
    transform: translateY(1px);
  }}
  .check {{
    display: flex;
    align-items: center;
    gap: 10px;
    user-select: none;
  }}
  .check input {{
    width: 15px;
    height: 15px;
    accent-color: var(--teal-dark);
  }}
  .sep {{
    height: 1px;
    background: var(--border);
    margin: 6px 0;
  }}
</style>
</head>
<body>
<main>
  <h1 class="brand">Stitch</h1>
  <p class="sub">Local panel controller — same actions as the menu bar.</p>
  <div id="status" class="status {status_class}"><span class="dot"></span><span id="status-text">{status_text}</span></div>
  <div class="stack">
    <button class="primary" data-action="open">Open Stitch</button>
    <button data-action="start">Start panel</button>
    <button data-action="stop">Stop panel</button>
    <label class="check"><input type="checkbox" id="autostart"{autostart_checked}> Start at login</label>
    {dock_row}
    <div class="sep"></div>
    <button data-action="copy_password">Copy panel password</button>
    <button data-action="update">Check for updates…</button>
    <div class="sep"></div>
    <button class="danger" data-action="quit">Quit Stitch</button>
  </div>
</main>
<script>
  function post(action) {{
    if (window.ipc && window.ipc.postMessage) {{
      window.ipc.postMessage(action);
    }}
  }}
  document.querySelectorAll("[data-action]").forEach((el) => {{
    el.addEventListener("click", () => post(el.getAttribute("data-action")));
  }});
  const autostart = document.getElementById("autostart");
  if (autostart) {{
    autostart.addEventListener("change", () => post("toggle_autostart:" + (autostart.checked ? "1" : "0")));
  }}
  const hideDock = document.getElementById("hide-dock");
  if (hideDock) {{
    hideDock.addEventListener("change", () => post("toggle_hide_dock:" + (hideDock.checked ? "1" : "0")));
  }}
  window.__stitchSetState = function (state) {{
    const status = document.getElementById("status");
    const statusText = document.getElementById("status-text");
    if (status && statusText) {{
      status.className = "status " + (state.panelRunning ? "running" : "stopped");
      statusText.textContent = state.panelRunning ? "Panel running" : "Panel stopped";
    }}
    if (autostart) autostart.checked = !!state.autostart;
    if (hideDock) hideDock.checked = !!state.hideDock;
  }};
</script>
</body>
</html>"#,
        status_class = status.0,
        status_text = status.1,
        autostart_checked = autostart_checked,
        dock_row = dock_row,
    )
}

/// JS snippet to push tray/window state into the control UI.
pub fn set_state_script(autostart: bool, hide_dock: bool, panel_running: bool) -> String {
    format!(
        "window.__stitchSetState && window.__stitchSetState({{ autostart: {}, hideDock: {}, panelRunning: {} }});",
        if autostart { "true" } else { "false" },
        if hide_dock { "true" } else { "false" },
        if panel_running { "true" } else { "false" },
    )
}
