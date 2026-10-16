import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

// The phone app's UI (docs/mobile-companion.md). Tauri serves it from
// `dist` in builds and from this dev server (on the LAN, for a phone) in
// `tauri android dev`.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 1430,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: 'ws', host, port: 1431 } : undefined,
  },
});
