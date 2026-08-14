# Sentry USB Web Frontend

The Sentry USB web interface — a React single-page application with a dark glassmorphism design.

## Tech Stack

- **React 19** + TypeScript
- **Vite** — build tooling and dev server
- **TailwindCSS** — utility-first styling
- **Material Symbols** — icons, vendored as inline SVG in `src/components/icons.tsx`
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

## Icons

Icons are Google Material Symbols, vendored as inline SVG in
`src/components/icons.tsx` so the app ships no icon font and makes no external
request. That file is generated — don't edit it by hand.

To add or remove one, put its name (exactly as shown on
[fonts.google.com/icons](https://fonts.google.com/icons)) in
`scripts/icons/symbols.mjs`, then regenerate:

```bash
npm run icons             # rewrites src/components/icons.tsx
npm run icons -- --check  # CI: fails if the committed file is stale
```

Each symbol becomes a component named after it (`delete` → `DeleteIcon`), taking
the same props as any `<svg>`, so Tailwind sizing (`h-4 w-4`) works as usual.

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
