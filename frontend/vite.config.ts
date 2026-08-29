import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// Config aligned with src-tauri/tauri.conf.json:
// - build.frontendDist = "../dist" -> outDir below
// - build.devUrl = "http://localhost:1420" -> server.port/strictPort below
export default defineConfig({
  plugins: [react()],
  clearScreen: false, // Preserve Rust compiler output in the terminal.
  server: {
    port: 1420,
    strictPort: true,
    // host: true binds both 127.0.0.1 and ::1 -- without it Vite's dev
    // server binds only ::1, and the webview's "localhost" resolves to
    // 127.0.0.1 first on this machine, so it got a hard connection-refused
    // instead of falling back. TEMP: added live during dev-scaffolding
    // testing (2026-08-28); fine to keep, but flag for Jason to decide if
    // it belongs long-term or should be narrowed back to '127.0.0.1' only.
    host: true,
    watch: {
      // Don't watch src-tauri -- avoids double-rebuild loops with cargo watch.
      ignored: ['**/src-tauri/**'],
    },
  },
  build: {
    outDir: 'dist',
  },
})
