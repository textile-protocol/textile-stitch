# stitch-panel frontend

Vite + React + Tailwind. The build output in `dist/` is compiled into the
`stitch-panel` binary by `rust-embed`, so the panel ships as one file with no
static directory to keep in sync.

Not a yarn workspace of the monorepo, on purpose: it's built in a node stage of
`Dockerfile.panel` and nothing in `web/` or `api/` imports it. `cargo build` for
the bot itself never needs node.

## Working on it

```bash
npm install
npm run dev          # http://localhost:5420, /api proxied to 127.0.0.1:8420
```

Point it at a panel somewhere else with `PANEL_URL=http://host:8420 npm run dev`.

You need a panel running to develop against, since every screen is API-driven:

```bash
cd ..
STITCH_PANEL_PASSWORD_HASH="$(cargo run --features panel --bin stitch-panel -- hash-password)" \
  cargo run --features panel --bin stitch-panel
```

## Building

```bash
npm run build        # typechecks, then bundles into dist/
```

`cargo build --features panel` embeds whatever is in `dist/` at that moment. A
checkout with no frontend build still compiles: `build.rs` creates the empty folder
`rust-embed` needs, and the panel serves a page telling you to run the build.

## Layout

| Path | What |
|------|------|
| `src/api.ts` | The only place that fetches. Turns API errors into `ApiError`. |
| `src/sse.ts` | SSE reader over `fetch`, because approve/dry-run are POSTs. |
| `src/types.ts` | Mirrors the serde structs in `src/panel/http/`. |
| `src/theme.css` | The `--tx-*` tokens, shared with the desktop app's palette. |
| `src/components/ui.tsx` | Buttons, cards, fields, banners, loading and empty states. |
| `src/pages/` | Login, Fleet, AddBot, BotDetail. |

Server error prose is shown verbatim. It comes from the config writer and the
TOML validator, which know exactly what's wrong; the UI doesn't second-guess it.
