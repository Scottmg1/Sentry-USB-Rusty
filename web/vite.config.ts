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
      // Stable vendor chunks preserve library caches across app-only updates.
        manualChunks(id: string) {
          if (/[\\/]node_modules[\\/](react|react-dom|react-router|react-router-dom)[\\/]/.test(id)) return 'vendor-react'
          if (/[\\/]node_modules[\\/]recharts[\\/]/.test(id)) return 'vendor-charts'
          if (/[\\/]node_modules[\\/]leaflet[\\/]/.test(id)) return 'vendor-maps'
          if (/[\\/]node_modules[\\/]@xterm[\\/]/.test(id)) return 'vendor-term'
        // Vendored icons change independently from the views that use them.
          if (/[\\/]src[\\/]components[\\/]icons\.tsx$/.test(id)) return 'vendor-icons'
        },
      },
    },
    // Do not preload heavy vendors that are reachable only through lazy routes.
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
      // SENTRYUSB_API can point development at a remote backend.
      '/api': process.env.SENTRYUSB_API || 'http://localhost:8788',
      '/TeslaCam': process.env.SENTRYUSB_API || 'http://localhost:8788',
    },
  },
})
