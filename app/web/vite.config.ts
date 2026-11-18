import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// `npm run dev` serves the UI with hot reload and forwards the API to a dev
// server (`cargo dev`, port 8080;
// FLWRS_API overrides it). `app/build.rs` sets FLWRS_WEB_OUT_DIR to build
// into cargo's OUT_DIR.
const api = process.env.FLWRS_API ?? 'http://127.0.0.1:8080';

export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: process.env.FLWRS_WEB_OUT_DIR ?? 'dist',
    emptyOutDir: true,
    target: 'es2022',
    // No inline scripts: the server's Content-Security-Policy allows only 'self'.
    modulePreload: { polyfill: false },
    assetsInlineLimit: 0,
  },
  server: {
    proxy: {
      '/api': { target: api, changeOrigin: true },
      '/api-docs': { target: api, changeOrigin: true },
    },
  },
});
