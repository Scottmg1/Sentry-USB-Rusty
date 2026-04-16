# Sentry USB Web Frontend

The Sentry USB web interface — a React single-page application with a dark glassmorphism design.

## Tech Stack

- **React 19** + TypeScript
- **Vite** — build tooling and dev server
- **TailwindCSS** — utility-first styling
- **Lucide React** — icons
- **Leaflet** — drive map visualization

## Development

```bash
npm install
npm run dev     # Starts dev server on http://localhost:5173
```

The dev server proxies `/api/*` requests to the Go backend at `localhost:8788`. Start the backend in dev mode:

```bash
cd ../server
make dev        # Starts Go API on :8788
```

## Production Build

```bash
npm run build   # Outputs to dist/
```

The built files are embedded into the Go binary via `go:embed`. After building the frontend:

```bash
cd ../server
make copy-static build-arm64   # Copies dist/ → static/, cross-compiles
```

## Pages

| Page | Description |
|------|-------------|
| **Dashboard** | System status, CPU temp, WiFi, disk space, snapshots, drive map |
| **Viewer** | Multi-camera clip viewer with synced playback (6 cameras) |
| **Files** | Browse/upload/delete Music, LightShow, and Boombox files |
| **Logs** | Live-tailing of archiveloop, setup, and diagnostics logs |
| **Settings** | Setup Wizard, quick actions, system update, reboot |

## Structure

```
src/
├── components/
│   ├── layout/        # AppShell, Sidebar, MobileNav
│   └── setup/         # SetupWizard + 9 step components
├── pages/             # Dashboard, Viewer, Files, Logs, Settings
└── lib/               # API client, WebSocket hook, utilities
```
