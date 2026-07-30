//! Make sure the panel's asset folder exists before the crate is compiled.
//!
//! `rust_embed` fails to expand if `web/dist` is missing, and that folder is Vite
//! output: absent on a fresh checkout, and wiped on every `npm run build`. A
//! tracked placeholder file inside it doesn't survive, because Vite empties the
//! directory. Creating it here instead means `cargo build --features panel` works
//! on a checkout with no frontend build — the panel then serves a page explaining
//! how to build one (see src/panel/http/assets.rs).

fn main() {
    // Only the panel feature embeds assets; the bot and the desktop app don't.
    if std::env::var_os("CARGO_FEATURE_PANEL").is_none() {
        return;
    }
    let dist = std::path::Path::new("web/dist");
    if let Err(e) = std::fs::create_dir_all(dist) {
        // Not fatal on its own: if the folder does exist, the embed still works.
        // Say so rather than failing the build on a read-only source tree.
        println!("cargo:warning=couldn't create {}: {e}", dist.display());
    }
    println!("cargo:rerun-if-changed=web/dist");
}
