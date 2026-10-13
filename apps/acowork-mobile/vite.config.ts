import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The mobile app is a browser-first PWA shell. In Tauri the same bundle runs
// inside the native webview, so no platform-specific Vite config is needed.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  // pnpm installs `node_modules/react` as a Windows junction into
  // `node_modules/.pnpm/react@<v>/…`. Node's require keeps the junction path
  // while Vite resolves to the real path, so the test graph and react-dom each
  // loaded their own React and every hook died with "Invalid hook call" —
  // no render-based test could run. Keeping symlinks unresolved makes both
  // sides use the same spelling, hence the same module instance.
  resolve: {
    preserveSymlinks: true,
  },
  server: {
    port: 19877,
    strictPort: true,
  },
  build: {
    target: 'es2022',
    sourcemap: false,
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.{ts,tsx}'],
  },
})
