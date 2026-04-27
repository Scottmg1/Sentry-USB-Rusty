# Developer Guide

Guide for building Sentry USB from source, understanding the architecture, and contributing.

## Architecture

```
Browser (React SPA)  ←→  Go API Server (single ARM binary)  ←→  Shell Scripts + Pi Hardware
```

- **Frontend**: React 19 + TypeScript + Vite + TailwindCSS 4 — builds to static files
- **Backend**: Go 1.25+ HTTP server with REST API + WebSocket for live updates
- **Legacy**: Existing bash scripts preserved; Go shells out to them
- **Embedding**: The Go binary embeds the frontend via `//go:embed all:static`

## Project Structure

```
SentryUSB/
├── web/                  # React frontend (Vite + TailwindCSS)
│   └── src/
│       ├── components/
│       │   ├── layout/   # AppShell, Sidebar, MobileNav
│       │   └── setup/    # SetupWizard + 9 step components
│       ├── pages/        # Dashboard, Viewer, Drives, Files, Logs, Settings, Support
│       └── lib/          # API client, WebSocket hook, utilities
├── server/               # Go API server
│   ├── api/              # HTTP handlers (status, config, files, setup, system, etc.)
│   ├── config/           # Config file parser/writer
│   ├── drives/           # Drive map data handling
│   ├── shell/            # Safe subprocess execution
│   ├── ws/               # WebSocket hub
│   └── static/           # Embedded frontend files (copied from web/dist)
├── run/                  # Runtime scripts (archiveloop, gadget, sync, etc.)
├── setup/                # Pi setup & configuration scripts
├── tools/                # Utility scripts (resize, diagnostics)
├── pi-gen-sources/       # Pi image build configuration
├── .github/workflows/    # CI: build-image, release, shellcheck
└── wiki/                 # Documentation (GitHub wiki)
```

## Prerequisites

- **Node.js 20+** with npm — for building the frontend
- **Go 1.25+** — for compiling the server binary
- Both must be available in your PATH

## Development Setup

### Frontend

```bash
cd web
npm install
npm run dev          # Starts Vite dev server on http://localhost:5173
```

The dev server proxies `/api/*` requests to the Go backend at `localhost:8788`.

### Backend

```bash
cd server
go mod tidy
make dev             # Starts Go API server on :8788 in dev mode
```

In dev mode (`-dev` flag), the Go server does not serve embedded static files — it expects the Vite dev server to handle the frontend.

### Running Both Together

1. Terminal 1: `cd web && npm run dev` (frontend on :5173)
2. Terminal 2: `cd server && make dev` (backend on :8788)
3. Open `http://localhost:5173` in your browser

The Vite config proxies API calls from :5173 to :8788 automatically.

## Production Build

### Step-by-Step

```bash
# 1. Build the frontend
cd web
npm run build                    # Outputs to web/dist/

# 2. Copy frontend into server/static for Go embedding
cd ../server
make copy-static                 # Copies web/dist/* → server/static/

# 3. Cross-compile for Raspberry Pi
GOOS=linux GOARCH=arm64 go build -o bin/sentryusb-linux-arm64 .     # Pi 4/5
GOOS=linux GOARCH=arm GOARM=7 go build -o bin/sentryusb-linux-armv7 .  # Pi Zero
```

### PowerShell One-Liner

```powershell
cd web; npm run build; cd ..; Remove-Item -Recurse -Force "server\static\*"; Copy-Item -Recurse "web\dist\*" "server\static\"; cd server; $env:GOOS="linux"; $env:GOARCH="arm64"; go build -o "bin/sentryusb-linux-arm64" .; $env:GOOS="linux"; $env:GOARCH="arm"; $env:GOARM="7"; go build -o "bin/sentryusb-linux-armv7" .; cd ..
```

### Make Targets

| Target | Description |
|--------|-------------|
| `make build` | Build for current platform (includes copy-static) |
| `make build-arm64` | Cross-compile for Pi 4/5 (64-bit ARM) |
| `make build-armv7` | Cross-compile for Pi Zero (32-bit ARM) |
| `make build-all` | Build both ARM targets |
| `make copy-static` | Copy `web/dist/*` → `server/static/` |
| `make dev` | Start Go server in dev mode on :8788 |
| `make clean` | Remove build artifacts |

> **Important**: The Go binary embeds files from `server/static/` via `//go:embed`. You **must** run `make copy-static` (or copy manually) after every frontend build, before compiling. Otherwise the binary will embed stale frontend files.

## API Endpoints

The Go server exposes these REST API routes:

### Status & Config
| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/status` | System status (CPU, disk, WiFi, USB, etc.) |
| GET | `/api/config` | Current configuration |
| GET | `/api/wifi` | Detected WiFi configuration |

### Setup
| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/setup/status` | Setup progress (running, finished, etc.) |
| GET | `/api/setup/config` | Read setup config (sentryusb.conf values) |
| PUT | `/api/setup/config` | Save setup config |
| POST | `/api/setup/run` | Trigger setup process |

### Files & Clips
| Method | Endpoint | Description |
|--------|----------|-------------|
| GET | `/api/clips` | List dashcam clips |
| GET | `/api/files/ls` | List files in a directory |
| POST | `/api/files/upload` | Upload a file |
| DELETE | `/api/files` | Delete a file |
| GET | `/api/files/download` | Download a file |
| GET | `/api/files/download-zip` | Download files as ZIP |

### System
| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/api/system/reboot` | Reboot the Pi |
| POST | `/api/system/toggle-drives` | Toggle USB drive mount |
| POST | `/api/system/trigger-sync` | Manually trigger archive sync |
| POST | `/api/system/ble-pair` | Initiate BLE pairing |
| POST | `/api/system/update` | Run system update |
| GET | `/api/system/version` | Current version info |
| GET | `/api/system/health-check` | Comprehensive health check |

### Notifications (Mobile Push Pairing)
| Method | Endpoint | Description |
|--------|----------|-------------|
| POST | `/api/notifications/generate-code` | Generate a 6-character pairing code (expires in 5 min) |
| GET | `/api/notifications/paired-devices` | List all paired mobile devices |
| DELETE | `/api/notifications/paired-devices/{id}` | Remove a paired device |

### WebSocket
| Endpoint | Description |
|----------|-------------|
| `/api/ws` | WebSocket for real-time updates (setup status, logs, etc.) |

## Building a Pi Image

The SentryUSB Pi image is built using [pi-gen](https://github.com/RPi-Distro/pi-gen). See [pi-gen-sources/Readme.md](https://github.com/Sentry-Six/Sentry-USB-Rusty/blob/main-dev/pi-gen-sources/Readme.md) for details.

Quick method:
```bash
./build-image.sh
```

Or pass a pre-built binary:
```bash
./build-image.sh server/bin/sentryusb-linux-arm64
```

Images are also built automatically via GitHub Actions on every release.

## CI / GitHub Actions

| Workflow | Trigger | Description |
|----------|---------|-------------|
| `build-image.yml` | Release publish or manual | Builds Pi images (arm64 + armhf) |
| `release.yml` | Manual | Builds and uploads server binaries |
| `shellcheck.yml` | Push | Lints shell scripts |

## Common Issues

- **"pattern all:static: no matching files found"**: `server/static/` is empty. Run `make copy-static` or `npm run build` in `web/` first.
- **Frontend changes not appearing**: You forgot to copy the build output. The Go binary embeds from `server/static/`, not `web/dist/`.
- **Stale JS bundle**: Check the hash in the filename (e.g., `index-Dda2tfbK.js`). If unchanged, your source edits may not have been saved.

## Tech Stack Summary

| Component | Technology |
|-----------|-----------|
| Frontend framework | React 19 + TypeScript |
| Build tool | Vite 7 |
| Styling | TailwindCSS 4 |
| Icons | Lucide React |
| Maps | Leaflet |
| Routing | React Router 7 |
| Backend | Go 1.25 |
| Embedding | `//go:embed` |
| WebSocket | gorilla-style hub (custom) |
| Target OS | Raspberry Pi OS (Bookworm) |
| Target arch | ARM64 (Pi 4/5), ARMv7 (Pi Zero) |
