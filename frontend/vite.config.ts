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
    watch: {
      // Don't watch src-tauri -- avoids double-rebuild loops with cargo watch.
      ignored: ['**/src-tauri/**'],
    },
  },
  build: {
    outDir: 'dist',
  },
})
