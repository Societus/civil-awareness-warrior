import { defineConfig } from 'vite';
import { sveltekit } from '@sveltejs/kit/vite';
import adapter from '@sveltejs/adapter-static';

export default defineConfig({
  plugins: [
    sveltekit({
      // SPA mode for Tauri: everything client-side, single index.html.
      adapter: adapter({ fallback: 'index.html' })
    })
  ],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // ignore cargo build artifacts
      ignored: ['**/src-tauri/**']
    }
  },
  envPrefix: ['VITE_', 'TAURI_'],
  build: {
    // vite 8 defaults: oxc transpile/minify, target baseline-widely-available
    sourcemap: !!process.env.TAURI_DEBUG
  }
});
