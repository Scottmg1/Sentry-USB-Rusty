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
        // code doesn't bust the cache for libraries that haven't
        // moved. Each library lives in its own content-hashed file.
        // Vite 8 / Rolldown removed object-form manualChunks; this is
        // the codeSplitting equivalent (matched by node_modules path).
        codeSplitting: {
          groups: [
            { name: 'vendor-react', test: /[\\/]node_modules[\\/](react|react-dom|react-router|react-router-dom)[\\/]/ },
            { name: 'vendor-charts', test: /[\\/]node_modules[\\/]recharts[\\/]/ },
            { name: 'vendor-maps', test: /[\\/]node_modules[\\/]leaflet[\\/]/ },
            { name: 'vendor-term', test: /[\\/]node_modules[\\/]@xterm[\\/]/ },
            { name: 'vendor-icons', test: /[\\/]node_modules[\\/]lucide-react[\\/]/ },
          ],
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
    proxy: {
      '/api': 'http://localhost:8788',
      '/TeslaCam': 'http://localhost:8788',
    },
  },
})
