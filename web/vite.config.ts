import path from "path"
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  build: {
    rollupOptions: {
      output: {
        // Named vendor chunks so an OTA update that only changes app
        // code doesn't bust the cache for libraries that haven't moved.
        // Each library lives in its own content-hashed file. Standard
        // Rollup `manualChunks` function form — the prior `codeSplitting`
        // key is a rolldown-vite-only API, but this build runs plain
        // vite@8, so it failed to build. Same vendor groups, working syntax.
        manualChunks(id: string) {
          if (/[\\/]node_modules[\\/](react|react-dom|react-router|react-router-dom)[\\/]/.test(id)) return 'vendor-react'
          if (/[\\/]node_modules[\\/]recharts[\\/]/.test(id)) return 'vendor-charts'
          if (/[\\/]node_modules[\\/]leaflet[\\/]/.test(id)) return 'vendor-maps'
          if (/[\\/]node_modules[\\/]@xterm[\\/]/.test(id)) return 'vendor-term'
          // Icons are vendored app source (src/components/icons.tsx) rather
          // than a package, but they change far less often than the views
          // that use them, so keep them in their own cacheable chunk.
          if (/[\\/]src[\\/]components[\\/]icons\.tsx$/.test(id)) return 'vendor-icons'
        },
      },
    },
    // Vite's default modulepreload walks every transitively-reachable
    // async chunk and bakes a <link rel="modulepreload"> for each.
    // That defeats lazy-loading for heavy vendors: leaflet/xterm/
    // recharts get preloaded on every page just because *some* lazy
    // route eventually pulls them in. Strip those from the initial
    // preload list — they'll still be fetched on-demand when the
    // lazy chunk that needs them is loaded (one extra RTT at
    // navigation time, but only for users who actually visit that
    // chunk's route).
    modulePreload: {
      resolveDependencies: (_filename, deps) =>
        deps.filter(
          (d) =>
            !d.includes('vendor-charts') &&
            !d.includes('vendor-maps') &&
            !d.includes('vendor-term'),
        ),
    },
  },
  server: {
    allowedHosts: true,
    proxy: {
      // Backend API target. Defaults to the local Rust server on :8788;
      // set SENTRYUSB_API (e.g. http://sentryusb.local) to develop the
      // UI against a live Pi without running the backend locally.
      '/api': process.env.SENTRYUSB_API || 'http://localhost:8788',
      '/TeslaCam': process.env.SENTRYUSB_API || 'http://localhost:8788',
    },
  },
})
