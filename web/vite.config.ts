import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';

// Fixed port so tauri.conf.json can point devUrl at it.
// `clearScreen: false` keeps Rust build output visible during dev.
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 5174,
    strictPort: true,
    watch: {
      ignored: ['**/src-tauri/**'],
    },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  build: {
    outDir: 'dist',
    target: 'es2021',
    minify: !process.env.TAURI_ENV_DEBUG,
    sourcemap: !!process.env.TAURI_ENV_DEBUG,
    // A desktop app loads from disk: one big chunk is fine.
    chunkSizeWarningLimit: 4000,
  },
});
