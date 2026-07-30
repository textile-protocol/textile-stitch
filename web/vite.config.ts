import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

// The build output is embedded into the stitch-panel binary by rust-embed, which
// reads `web/dist` relative to the crate root — so the output directory is not
// negotiable.
//
// `vite dev` proxies /api to a panel running locally, so the frontend can be
// iterated on against a real Docker host without rebuilding the binary.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // Hashed asset names: the panel serves them immutable, and index.html no-cache.
    assetsDir: 'assets',
  },
  server: {
    port: 5420,
    proxy: {
      '/api': {
        target: process.env.PANEL_URL ?? 'http://127.0.0.1:8420',
        changeOrigin: true,
      },
    },
  },
})
