// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (c) 2026 Textile, Inc.
//! Inline HTML for the desktop control window (mirrors the tray menu).
//!
//! Colors match `packages/stitch-bot/web/src/theme.css` / the Textile web app
//! `--tx-*` tokens so the desktop shell reads as the same product.

/// Textile mark (favicon-scale paths from the panel's `TextileIcon`).
const TEXTILE_ICON_SVG: &str = r##"<svg viewBox="0 0 32 32" fill="none" aria-hidden="true" focusable="false" class="mark">
  <path d="M17.1296 24.2935C16.9735 24.2935 16.829 24.23 16.7192 24.1262L14.9219 22.3314L13.1245 24.1262C12.8991 24.3512 12.535 24.3512 12.3096 24.1262L10.5123 22.3314L8.71493 24.1262C8.49532 24.3455 8.11389 24.3455 7.90005 24.1262L6.1027 22.3314L4.30534 24.1262C4.07995 24.3512 3.71586 24.3512 3.49047 24.1262L1.28278 21.9216C1.17298 21.812 1.11518 21.6677 1.11518 21.5119C1.11518 21.3561 1.17876 21.2118 1.28278 21.1021L3.08014 19.3074L1.28278 17.5126C1.05739 17.2875 1.05739 16.9239 1.28278 16.6988L3.08014 14.9041L1.28278 13.1093C1.17298 12.9996 1.11518 12.8553 1.11518 12.6995C1.11518 12.5437 1.17876 12.3994 1.28278 12.2898L3.08014 10.495L1.28278 8.70021C1.05739 8.47514 1.05739 8.11156 1.28278 7.88649L3.08014 6.0917L1.28278 4.30268C1.17298 4.19303 1.11518 4.04876 1.11518 3.89294C1.11518 3.73712 1.17876 3.59285 1.28278 3.4832L3.49047 1.27867C3.71586 1.0536 4.07995 1.0536 4.30534 1.27867L6.1027 3.07346L7.90005 1.27867C8.12545 1.0536 8.48954 1.0536 8.71493 1.27867L10.5123 3.07346L12.3096 1.27867C12.535 1.0536 12.8991 1.0536 13.1245 1.27867L14.9219 3.07346L16.7192 1.27867C16.9446 1.0536 17.3087 1.0536 17.5341 1.27867L19.3315 3.07346L21.1288 1.27867C21.3542 1.0536 21.7183 1.0536 21.9437 1.27867L24.1514 3.4832C24.2612 3.59285 24.319 3.73712 24.319 3.89294C24.319 4.04876 24.2554 4.19303 24.1514 4.30268L22.354 6.09747L24.1514 7.89226C24.3768 8.11733 24.3768 8.48091 24.1514 8.70598L22.354 10.5008L24.1514 12.2956C24.2612 12.4052 24.319 12.5495 24.319 12.7053C24.319 12.8611 24.2554 13.0054 24.1514 13.115L22.354 14.9098L24.1514 16.7046C24.3768 16.9297 24.3768 17.2933 24.1514 17.5183L22.354 19.3131L24.1514 21.1079C24.2612 21.2176 24.319 21.3618 24.319 21.5177C24.319 21.6735 24.2554 21.8178 24.1514 21.9274L21.9437 24.1319C21.7183 24.357 21.3542 24.357 21.1288 24.1319L19.3315 22.3371L17.5341 24.1319C17.4243 24.2416 17.2798 24.2993 17.1238 24.2993L17.1296 24.2935ZM19.3372 20.9406C19.4933 20.9406 19.6378 21.004 19.7476 21.1079L21.5449 22.9027L22.9319 21.5177L21.1346 19.7229C21.0248 19.6132 20.967 19.4689 20.967 19.3131C20.967 19.1573 21.0306 19.013 21.1346 18.9034L22.9319 17.1086L21.1346 15.3138C21.0248 15.2042 20.967 15.0599 20.967 14.9041C20.967 14.7482 21.0306 14.604 21.1346 14.4943L22.9319 12.6995L21.1346 10.9047C20.9092 10.6797 20.9092 10.3161 21.1346 10.091L22.9319 8.29624L21.1346 6.50145C20.9092 6.27638 20.9092 5.9128 21.1346 5.68773L22.9319 3.89294L21.5449 2.50789L19.7476 4.30268C19.5222 4.52775 19.1581 4.52775 18.9327 4.30268L17.1353 2.50789L15.338 4.30268C15.1126 4.52775 14.7485 4.52775 14.5231 4.30268L12.7257 2.50789L10.9284 4.30268C10.703 4.52775 10.3389 4.52775 10.1135 4.30268L8.31616 2.50789L6.51881 4.30268C6.29342 4.52775 5.92932 4.52775 5.70393 4.30268L3.90657 2.50789L2.51955 3.89294L4.3169 5.68773C4.54229 5.9128 4.54229 6.27638 4.3169 6.50145L2.51955 8.29624L4.3169 10.091C4.54229 10.3161 4.54229 10.6797 4.3169 10.9047L2.51955 12.6995L4.3169 14.4943C4.42671 14.604 4.4845 14.7482 4.4845 14.9041C4.4845 15.0599 4.42093 15.2042 4.3169 15.3138L2.51955 17.1086L4.3169 18.9034C4.42671 19.013 4.4845 19.1573 4.4845 19.3131C4.4845 19.4689 4.42093 19.6132 4.3169 19.7229L2.51955 21.5177L3.90657 22.9027L5.70393 21.1079C5.92932 20.8828 6.29342 20.8828 6.51881 21.1079L8.31616 22.9027L10.1135 21.1079C10.3331 20.8886 10.7146 20.8886 10.9284 21.1079L12.7257 22.9027L14.5231 21.1079C14.7485 20.8828 15.1126 20.8828 15.338 21.1079L17.1353 22.9027L18.9327 21.1079C19.0425 20.9983 19.187 20.9406 19.343 20.9406H19.3372Z" fill="#15181c"/>
  <path d="M5.69813 21.1083C5.92353 20.8832 6.28762 20.8832 6.51301 21.1083L8.31037 22.9031L10.1077 21.1083C10.3273 20.889 10.7088 20.889 10.9226 21.1083L12.72 22.9031L14.5173 21.1083C14.7427 20.8832 15.1068 20.8832 15.3322 21.1083L17.1295 22.9031L18.9269 21.1083C19.0367 20.9987 19.1812 20.941 19.3372 20.941C19.4933 20.941 19.6378 21.0044 19.7476 21.1083L21.5449 22.9031L22.9319 21.5181L21.1346 19.7233C21.0248 19.6136 20.967 19.4693 20.967 19.3135C20.967 19.1577 21.0306 19.0134 21.1346 18.9038L22.9319 17.109L21.1346 15.3142C21.0248 15.2046 20.967 15.0603 20.967 14.9045C20.967 14.7486 21.0306 14.6044 21.1346 14.4947L22.9319 12.6999L21.1346 10.9051C20.9092 10.6801 20.9092 10.3165 21.1346 10.0914L22.9319 8.29664L21.1346 6.50185C20.9092 6.27678 20.9092 5.91321 21.1346 5.68814L22.9319 3.89335L21.5449 2.5083L19.7476 4.30309C19.5222 4.52816 19.1581 4.52816 18.9327 4.30309L17.1353 2.5083L15.338 4.30309C15.1126 4.52816 14.7485 4.52816 14.5231 4.30309L12.7257 2.5083L10.9284 4.30309C10.703 4.52816 10.3389 4.52816 10.1135 4.30309L8.31615 2.5083L6.51879 4.30309C6.2934 4.52816 5.9293 4.52816 5.70391 4.30309L3.90656 2.5083L2.51953 3.89335L4.31689 5.68814C4.54228 5.91321 4.54228 6.27678 4.31689 6.50185L2.51953 8.29664L4.31689 10.0914C4.54228 10.3165 4.54228 10.6801 4.31689 10.9051L2.51953 12.6999L4.31689 14.4947C4.42669 14.6044 4.48449 14.7486 4.48449 14.9045C4.48449 15.0603 4.42091 15.2046 4.31689 15.3142L2.51953 17.109L4.31689 18.9038C4.42669 19.0134 4.48449 19.1577 4.48449 19.3135C4.48449 19.4693 4.42091 19.6136 4.31689 19.7233L2.51953 21.5181L3.90656 22.9031L5.70391 21.1083H5.69813Z" fill="#FF5CFF"/>
  <path d="M28.1564 12.7055L30.3641 10.501L28.1564 8.30225L25.9545 10.501L23.7468 8.30225L21.5392 10.501L19.3373 8.30225L17.1296 10.501L14.9277 8.30225L12.72 10.501L10.5123 8.30225L8.31039 10.501L10.5123 12.7055L8.31039 14.9101L10.5123 17.1088L8.31039 19.3134L10.5123 21.5121L8.31039 23.7167L10.5123 25.9212L8.31039 28.12L10.5123 30.3245L12.72 28.12L14.9277 30.3245L17.1296 28.12L19.3373 30.3245L21.5392 28.12L23.7468 30.3245L25.9545 28.12L28.1564 30.3245L30.3641 28.12L28.1564 25.9212L30.3641 23.7167L28.1564 21.5121L30.3641 19.3134L28.1564 17.1088L30.3641 14.9101L28.1564 12.7055Z" fill="#F7CC1E"/>
  <path d="M8.31039 10.5014L10.5181 12.7059L8.31039 14.9105L10.5181 17.115L8.31039 19.3195L10.5181 21.5241L8.31039 23.7286L10.5181 25.9331L8.31039 28.1377L10.5181 30.3422L12.7258 28.1377V10.5187L10.5181 8.31419L8.31039 10.5187V10.5014ZM28.1564 12.7059L30.3641 10.5014L28.1564 8.29688L25.9487 10.5014V28.1204L28.1564 30.3249L30.3641 28.1204L28.1564 25.9158L30.3641 23.7113L28.1564 21.5068L30.3641 19.3022L28.1564 17.0977L30.3641 14.8932L28.1564 12.6886V12.7059ZM17.1296 10.5014V28.1204L19.3373 30.3249L21.5449 28.1204V10.5014L19.3373 8.29688L17.1296 10.5014Z" fill="#5272FF"/>
  <path d="M30.8194 14.4758L29.022 12.6811L30.8194 10.8863C31.0448 10.6612 31.0448 10.2976 30.8194 10.0726L28.6117 7.86802C28.5019 7.75837 28.3574 7.70066 28.2014 7.70066C28.0454 7.70066 27.9009 7.76414 27.7911 7.86802L25.9937 9.66281L24.1964 7.86802C23.971 7.64295 23.6069 7.64295 23.3815 7.86802L21.5841 9.66281L19.7868 7.86802C19.5672 7.64872 19.1857 7.64872 18.9719 7.86802L17.1745 9.66281L15.3772 7.86802C15.1518 7.64295 14.7877 7.64295 14.5623 7.86802L12.7649 9.66281L10.9676 7.86802C10.8578 7.75837 10.7133 7.70066 10.5573 7.70066C10.4012 7.70066 10.2567 7.76414 10.1469 7.86802L7.93925 10.0726C7.71385 10.2976 7.71385 10.6612 7.93925 10.8863L9.7366 12.6811L7.93925 14.4758C7.82944 14.5855 7.77165 14.7298 7.77165 14.8856C7.77165 15.0414 7.83522 15.1857 7.93925 15.2953L9.7366 17.0901L7.93925 18.8849C7.71385 19.11 7.71385 19.4736 7.93925 19.6986L9.7366 21.4934L7.93925 23.2882C7.82944 23.3979 7.77165 23.5421 7.77165 23.6979C7.77165 23.8538 7.83522 23.998 7.93925 24.1077L9.7366 25.9025L7.93925 27.6973C7.82944 27.8069 7.77165 27.9512 7.77165 28.107C7.77165 28.2628 7.83522 28.4071 7.93925 28.5168L10.1469 30.7213C10.2625 30.8367 10.407 30.8887 10.5573 30.8887C10.7075 30.8887 10.852 30.8309 10.9676 30.7213L12.7649 28.9265L14.5623 30.7213C14.7877 30.9464 15.1518 30.9464 15.3772 30.7213L17.1745 28.9265L18.9719 30.7213C19.0875 30.8367 19.232 30.8887 19.3822 30.8887C19.5325 30.8887 19.677 30.8309 19.7925 30.7213L21.5899 28.9265L23.3873 30.7213C23.5028 30.8367 23.6473 30.8887 23.7976 30.8887C23.9478 30.8887 24.0923 30.8309 24.2079 30.7213L26.0053 28.9265L27.8026 30.7213C27.9182 30.8367 28.0627 30.8887 28.213 30.8887C28.3632 30.8887 28.5077 30.8309 28.6233 30.7213L30.831 28.5168C31.0564 28.2917 31.0564 27.9281 30.831 27.703L29.0336 25.9083L30.831 24.1135C31.0564 23.8884 31.0564 23.5248 30.831 23.2997L29.0336 21.505L30.831 19.7102C31.0564 19.4851 31.0564 19.1215 30.831 18.8965L29.0336 17.1017L30.831 15.3069C31.0564 15.0818 31.0564 14.7182 30.831 14.4932L30.8194 14.4758ZM10.5573 29.4805L9.17023 28.0955L10.9676 26.3007C11.193 26.0756 11.193 25.712 10.9676 25.487L9.17023 23.6922L10.9676 21.8974C11.0774 21.7877 11.1352 21.6435 11.1352 21.4876C11.1352 21.3318 11.0716 21.1876 10.9676 21.0779L9.17023 19.2831L10.9676 17.4883C11.193 17.2633 11.193 16.8997 10.9676 16.6746L9.17023 14.8798L10.9676 13.085C11.0774 12.9754 11.1352 12.8311 11.1352 12.6753C11.1352 12.5195 11.0716 12.3752 10.9676 12.2655L9.17023 10.4708L10.5573 9.08571L12.187 10.7131V27.8531L10.5573 29.4805ZM14.9726 29.4805L13.3429 27.8531V10.7131L14.9726 9.08571L16.6024 10.7131V27.8531L14.9726 29.4805ZM17.7525 27.8589V10.7189L19.3822 9.09148L21.012 10.7189V27.8589L19.3822 29.4863L17.7525 27.8589ZM23.7918 29.4805L22.162 27.8531V10.7131L23.7918 9.08571L25.4216 10.7131V27.8531L23.7918 29.4805ZM27.7968 16.6746C27.5715 16.8997 27.5715 17.2633 27.7968 17.4883L29.5942 19.2831L27.7968 21.0779C27.5715 21.303 27.5715 21.6665 27.7968 21.8916L29.5942 23.6864L27.7968 25.4812C27.5715 25.7063 27.5715 26.0698 27.7968 26.2949L29.5942 28.0897L28.2072 29.4747L26.5774 27.8473V10.7074L28.2072 9.07994L29.5942 10.465L27.7968 12.2598C27.5715 12.4848 27.5715 12.8484 27.7968 13.0735L29.5942 14.8683L27.7968 16.6631V16.6746Z" fill="#15181c"/>
</svg>"##;

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
    --tx-bg-primary: #fbfaf7;
    --tx-bg-secondary: #ffffff;
    --tx-bg-hover: #f1eee8;
    --tx-text-primary: #15181c;
    --tx-text-secondary: rgb(21 24 28 / 0.6);
    --tx-text-tertiary: rgb(21 24 28 / 0.45);
    --tx-border-secondary: rgb(21 24 28 / 0.18);
    --tx-border-tertiary: rgb(21 24 28 / 0.1);
    --tx-accent: #9c2eb0;
    --tx-accent-tint: rgb(236 104 248 / 0.08);
    --tx-on-accent: #ffffff;
    --tx-text-success: #3b6d11;
    --tx-bg-success: #ecf4df;
    --tx-text-danger: #b03a2e;
  }}
  * {{ box-sizing: border-box; }}
  body {{
    margin: 0;
    font: 14px/1.45 Lato, ui-sans-serif, system-ui, -apple-system, "Segoe UI", sans-serif;
    background: var(--tx-bg-primary);
    color: var(--tx-text-primary);
    min-height: 100vh;
    -webkit-font-smoothing: antialiased;
  }}
  main {{
    max-width: 360px;
    margin: 0 auto;
    padding: 28px 22px 24px;
  }}
  .brand-row {{
    display: flex;
    align-items: center;
    gap: 12px;
    margin: 0 0 18px;
  }}
  .mark {{
    width: 36px;
    height: 36px;
    flex: 0 0 auto;
  }}
  h1 {{
    margin: 0;
    font-size: 22px;
    font-weight: 700;
    letter-spacing: -0.02em;
    color: var(--tx-text-primary);
  }}
  .status {{
    display: inline-flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 18px;
    padding: 6px 10px;
    border-radius: 999px;
    background: var(--tx-bg-secondary);
    border: 1px solid var(--tx-border-tertiary);
    font-size: 12px;
    color: var(--tx-text-secondary);
  }}
  .dot {{
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--tx-text-tertiary);
  }}
  .status.running .dot {{ background: var(--tx-text-success); }}
  .status.running {{
    background: var(--tx-bg-success);
    color: var(--tx-text-success);
    border-color: transparent;
  }}
  .stack {{
    display: flex;
    flex-direction: column;
    gap: 8px;
  }}
  button, .check {{
    width: 100%;
    border: 1px solid var(--tx-border-tertiary);
    background: var(--tx-bg-secondary);
    color: var(--tx-text-primary);
    border-radius: 10px;
    padding: 10px 12px;
    font: inherit;
    text-align: left;
    cursor: pointer;
  }}
  button.primary {{
    background: var(--tx-accent);
    border-color: var(--tx-accent);
    color: var(--tx-on-accent);
    font-weight: 600;
  }}
  button.danger {{
    color: var(--tx-text-danger);
  }}
  button:hover, .check:hover {{
    background: var(--tx-bg-hover);
    border-color: var(--tx-border-secondary);
  }}
  button.primary:hover {{
    background: #8a279c;
    border-color: #8a279c;
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
    accent-color: var(--tx-accent);
  }}
  .sep {{
    height: 1px;
    background: var(--tx-border-tertiary);
    margin: 6px 0;
  }}
</style>
</head>
<body>
<main>
  <div class="brand-row">
    {textile_icon}
    <h1>Stitch</h1>
  </div>
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
        textile_icon = TEXTILE_ICON_SVG,
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
