import { useEffect, useState, lazy, Suspense } from "react"
import { BrowserRouter, Routes, Route } from "react-router-dom"
import { ProgressActivityIcon, WarningIcon } from "@/components/icons"
import { AppShell } from "@/components/layout/AppShell"
import { SetupWizard } from "@/components/setup/SetupWizard"
import { SetupProgress } from "@/components/setup/SetupProgress"
import { AuthProvider, useAuth } from "@/hooks/useAuth"
import { ErrorBoundary } from "@/components/ErrorBoundary"

// Lazy routes — each page becomes its own JS chunk. Visiting the
// Dashboard no longer pulls in xterm (Terminal), leaflet (Viewer), or
// recharts (FSDAnalytics) on first paint. The Login screen is also
// lazy so unauthenticated users don't pay for the shell.
const Dashboard = lazy(() => import("@/pages/Dashboard"))
const Viewer = lazy(() => import("@/pages/Viewer"))
const Files = lazy(() => import("@/pages/Files"))
const Logs = lazy(() => import("@/pages/Logs"))
const Settings = lazy(() => import("@/pages/Settings"))
const Drives = lazy(() => import("@/pages/Drives"))
const DriveDetail = lazy(() => import("@/pages/DriveDetail"))
const Charging = lazy(() => import("@/pages/Charging"))
const ChargeSessionDetail = lazy(() => import("@/pages/ChargeSessionDetail"))
// Dev-only mock-data preview of in-progress UI; not routed in production.
const PreviewCharging = import.meta.env.DEV
  ? lazy(() => import("@/pages/PreviewCharging"))
  : null
const PreviewSupport = import.meta.env.DEV
  ? lazy(() => import("@/pages/PreviewSupport"))
  : null
const Support = lazy(() => import("@/pages/Support"))
const Terminal = lazy(() => import("@/pages/Terminal"))
const FSDAnalytics = lazy(() => import("@/pages/FSDAnalytics"))
const SafetyScore = lazy(() => import("@/pages/SafetyScore"))
const Community = lazy(() => import("@/pages/Community"))
const Notifications = lazy(() => import("@/pages/Notifications"))
const Snapshots = lazy(() => import("@/pages/Snapshots"))
const Login = lazy(() => import("@/pages/Login"))

type AppState = "loading" | "setup" | "configuring" | "finalizing" | "ready" | "setup_error"

type SetupErrorInfo = { kind?: string; message?: string }

export default function App() {
  return (
    <ErrorBoundary fallback={(error) => <CrashScreen error={error} />}>
      <AuthProvider>
        <AppContent />
      </AuthProvider>
    </ErrorBoundary>
  )
}

/** Last-resort fallback: an uncaught render error anywhere would otherwise
 *  unmount the whole root and leave a blank page with no recovery. */
function CrashScreen({ error }: { error: Error }) {
  return (
    <div className="flex h-screen items-center justify-center bg-slate-950 p-4">
      <div className="flex w-full max-w-md flex-col items-center gap-4 rounded-2xl border border-white/10 bg-white/[0.03] p-8 text-center">
        <h2 className="text-lg font-semibold text-slate-100">Something went wrong</h2>
        <p className="break-all text-xs text-slate-500">{String(error?.message ?? error)}</p>
        <button
          onClick={() => window.location.reload()}
          className="rounded-lg bg-blue-500/15 px-4 py-2 text-sm font-medium text-blue-400 transition-colors hover:bg-blue-500/25"
        >
          Reload
        </button>
      </div>
    </div>
  )
}

function AppContent() {
  const [appState, setAppState] = useState<AppState>("loading")
  const [setupError, setSetupError] = useState<SetupErrorInfo | null>(null)
  const { state: authState, login } = useAuth()

  useEffect(() => {
    let cancelled = false
    async function checkStatus() {
      try {
        const res = await fetch("/api/setup/status")
        const data = await res.json()
        if (cancelled) return
        if (data.setup_finished) {
          setAppState("ready")
        } else if (data.error) {
          // Setup stopped on an error. Show a terminal "fix & retry" state
          // instead of the perpetual "Setting Up" spinner — this is what a
          // page reload after a config failure used to get stuck on.
          setSetupError(data.error)
          setAppState("setup_error")
        } else if (data.setup_running) {
          setAppState("configuring")
        } else {
          setAppState("setup")
        }
      } catch {
        if (!cancelled) setAppState("ready")
      }
    }
    checkStatus()
    return () => { cancelled = true }
  }, [])

  // Poll while configuring — wait for setup to finish. The backend sets
  // SENTRYUSB_SETUP_FINISHED *before* the 5-second delay and final
  // `systemctl reboot`, so going straight to "ready" here would land the
  // user on the dashboard just in time for the Pi to kill the network.
  // Transition to "finalizing" first and let that effect wait for the
  // reboot to actually happen.
  useEffect(() => {
    if (appState !== "configuring") return
    const interval = setInterval(async () => {
      try {
        const res = await fetch("/api/setup/status")
        const data = await res.json()
        if (data.setup_finished) {
          setAppState("finalizing")
        } else if (data.error) {
          // Failed mid-run (e.g. a config validation bail) — surface it
          // instead of spinning forever.
          setSetupError(data.error)
          setAppState("setup_error")
        }
      } catch {
        // Server rebooting — keep polling
      }
    }, 3000)
    return () => clearInterval(interval)
  }, [appState])

  // Finalizing: wait for the server to drop off (confirming the final
  // reboot actually started) and then come back before redirecting to
  // the dashboard. Without the `wentDown` gate we could bounce straight
  // to "ready" on the very next poll and then lose the connection mid-
  // navigation when the Pi finally reboots. Matches the same pattern
  // the SetupWizard's own finalize effect uses.
  useEffect(() => {
    if (appState !== "finalizing") return
    let wentDown = false
    const id = setInterval(async () => {
      try {
        const res = await fetch("/api/setup/status")
        if (res.ok && wentDown) {
          setAppState("ready")
        }
      } catch {
        wentDown = true
      }
    }, 2000)
    return () => clearInterval(id)
  }, [appState])

  // Still checking
  if (appState === "loading") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="h-6 w-6 animate-spin rounded-full border-2 border-blue-500 border-t-transparent" />
      </div>
    )
  }

  // Setup stopped on an error — terminal "fix & retry" state. Replaces the
  // old behavior where a config failure left the UI on the "Setting Up"
  // spinner forever while the device silently re-ran the doomed setup.
  if (appState === "setup_error") {
    const isConfig = setupError?.kind === "config"
    return (
      <div className="flex min-h-screen items-center justify-center bg-slate-950 p-4">
        <div className="flex w-full max-w-lg flex-col items-center gap-6 rounded-2xl border border-amber-500/20 bg-white/[0.03] p-10 text-center">
          <div className="flex h-16 w-16 items-center justify-center rounded-full bg-amber-500/20">
            <WarningIcon className="h-8 w-8 text-amber-400" />
          </div>
          <div>
            <h2 className="text-xl font-semibold text-slate-100">
              {isConfig ? "Setup needs a configuration fix" : "Setup hit a problem"}
            </h2>
            <p className="mt-2 text-sm text-slate-400">
              {setupError?.message || "Setup could not finish."}
            </p>
            <p className="mt-4 text-xs text-slate-600">
              {isConfig
                ? "Adjust your settings and re-run setup. Your recordings are not affected."
                : "Retry setup, or open the configuration to make changes."}
            </p>
          </div>
          <div className="flex flex-wrap items-center justify-center gap-3">
            <button
              onClick={() => { setSetupError(null); setAppState("setup") }}
              className="rounded-lg bg-blue-500/15 px-4 py-2 text-sm font-medium text-blue-400 transition-colors hover:bg-blue-500/25"
            >
              Open setup to fix
            </button>
            {!isConfig && (
              <button
                onClick={async () => {
                  setSetupError(null)
                  setAppState("configuring")
                  try {
                    await fetch("/api/setup/run", { method: "POST" })
                  } catch {
                    /* poll loop will re-surface any error */
                  }
                }}
                className="rounded-lg border border-white/10 px-4 py-2 text-sm font-medium text-slate-300 transition-colors hover:bg-white/[0.06]"
              >
                Retry setup
              </button>
            )}
          </div>
        </div>
      </div>
    )
  }

  // Setup is actively running (user refreshed during setup)
  if (appState === "configuring") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="flex w-full max-w-lg flex-col items-center gap-6 rounded-2xl border border-white/10 bg-white/[0.03] p-10 text-center">
          <div className="flex h-16 w-16 items-center justify-center rounded-full bg-blue-500/20">
            <ProgressActivityIcon className="h-8 w-8 animate-spin text-blue-400" />
          </div>
          <div>
            <h2 className="text-xl font-semibold text-slate-100">Setting Up Sentry USB</h2>
            <p className="mt-2 text-sm text-slate-400">
              Setup is in progress. The device will reboot several times — this is normal.
            </p>
            <p className="mt-4 text-xs text-slate-600">
              This page will automatically refresh when setup is complete.
              Do not power off the device. This may take 10–20 minutes.
            </p>
          </div>
          <SetupProgress />
        </div>
      </div>
    )
  }

  // Setup finished but the Pi hasn't rebooted yet — stay on this screen
  // until the network drops and recovers so we don't redirect the user
  // into a dashboard that's about to vanish.
  if (appState === "finalizing") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="flex w-full max-w-lg flex-col items-center gap-6 rounded-2xl border border-white/10 bg-white/[0.03] p-10 text-center">
          <div className="flex h-16 w-16 items-center justify-center rounded-full bg-emerald-500/20">
            <ProgressActivityIcon className="h-8 w-8 animate-spin text-emerald-400" />
          </div>
          <div>
            <h2 className="text-xl font-semibold text-slate-100">Almost Done!</h2>
            <p className="mt-2 text-sm text-slate-400">
              Setup complete. Rebooting one last time to apply everything — this page will
              redirect automatically once Sentry USB is back online.
            </p>
          </div>
        </div>
      </div>
    )
  }

  // Setup not done — show wizard full screen
  if (appState === "setup") {
    return (
      <div className="min-h-screen bg-slate-950 p-4">
        <SetupWizard onClose={() => setAppState("ready")} />
      </div>
    )
  }

  // Auth check — show login if required and not authenticated
  if (authState === "loading") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="h-6 w-6 animate-spin rounded-full border-2 border-blue-500 border-t-transparent" />
      </div>
    )
  }

  if (authState === "unauthenticated") {
    return (
      <Suspense fallback={<RouteFallback />}>
        <Login onLogin={login} />
      </Suspense>
    )
  }

  return (
    <BrowserRouter>
      <Suspense fallback={<RouteFallback />}>
        <Routes>
          <Route element={<AppShell />}>
            <Route path="/" element={<Dashboard />} />
            <Route path="/viewer" element={<Viewer />} />
            <Route path="/files" element={<Files />} />
            <Route path="/logs" element={<Logs />} />
            <Route path="/drives" element={<Drives />} />
            <Route path="/drives/:id" element={<DriveDetail />} />
            <Route path="/charging" element={<Charging />} />
            <Route path="/charging/:id" element={<ChargeSessionDetail />} />
            <Route path="/fsd" element={<FSDAnalytics />} />
            <Route path="/safety" element={<SafetyScore />} />
            <Route path="/support" element={<Support />} />
            <Route path="/terminal" element={<Terminal />} />
            <Route path="/community" element={<Community />} />
            <Route path="/notifications" element={<Notifications />} />
            <Route path="/snapshots" element={<Snapshots />} />
            <Route path="/settings" element={<Settings />} />
          </Route>
          {import.meta.env.DEV && PreviewCharging && (
            <Route path="/preview/charging" element={<PreviewCharging />} />
          )}
          {import.meta.env.DEV && PreviewSupport && (
            <Route path="/preview/support" element={<PreviewSupport />} />
          )}
        </Routes>
      </Suspense>
    </BrowserRouter>
  )
}

function RouteFallback() {
  return (
    <div className="flex h-screen items-center justify-center bg-slate-950">
      <ProgressActivityIcon className="h-6 w-6 animate-spin text-blue-400" />
    </div>
  )
}
