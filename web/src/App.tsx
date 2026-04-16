import { useEffect, useState } from "react"
import { BrowserRouter, Routes, Route } from "react-router-dom"
import { Loader2 } from "lucide-react"
import { AppShell } from "@/components/layout/AppShell"
import Dashboard from "@/pages/Dashboard"
import Viewer from "@/pages/Viewer"
import Files from "@/pages/Files"
import Logs from "@/pages/Logs"
import Settings from "@/pages/Settings"
import Drives from "@/pages/Drives"
import Support from "@/pages/Support"
import Terminal from "@/pages/Terminal"
import FSDAnalytics from "@/pages/FSDAnalytics"
import Community from "@/pages/Community"
import Notifications from "@/pages/Notifications"
import Login from "@/pages/Login"
import { SetupWizard } from "@/components/setup/SetupWizard"
import { SetupProgress } from "@/components/setup/SetupProgress"
import { AuthProvider, useAuth } from "@/hooks/useAuth"

type AppState = "loading" | "setup" | "configuring" | "ready"

export default function App() {
  return (
    <AuthProvider>
      <AppContent />
    </AuthProvider>
  )
}

function AppContent() {
  const [appState, setAppState] = useState<AppState>("loading")
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

  // Poll while configuring — wait for setup to finish
  useEffect(() => {
    if (appState !== "configuring") return
    const interval = setInterval(async () => {
      try {
        const res = await fetch("/api/setup/status")
        const data = await res.json()
        if (data.setup_finished) {
          setAppState("ready")
        }
      } catch {
        // Server rebooting — keep polling
      }
    }, 5000)
    return () => clearInterval(interval)
  }, [appState])

  // Still checking
  if (appState === "loading") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="h-6 w-6 animate-spin rounded-full border-2 border-blue-500 border-t-transparent" />
      </div>
    )
  }

  // Setup is actively running (user refreshed during setup)
  if (appState === "configuring") {
    return (
      <div className="flex h-screen items-center justify-center bg-slate-950">
        <div className="flex w-full max-w-lg flex-col items-center gap-6 rounded-2xl border border-white/10 bg-white/[0.03] p-10 text-center">
          <div className="flex h-16 w-16 items-center justify-center rounded-full bg-blue-500/20">
            <Loader2 className="h-8 w-8 animate-spin text-blue-400" />
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
    return <Login onLogin={login} />
  }

  return (
    <BrowserRouter>
      <Routes>
        <Route element={<AppShell />}>
          <Route path="/" element={<Dashboard />} />
          <Route path="/viewer" element={<Viewer />} />
          <Route path="/files" element={<Files />} />
          <Route path="/logs" element={<Logs />} />
          <Route path="/drives" element={<Drives />} />
          <Route path="/fsd" element={<FSDAnalytics />} />
          <Route path="/support" element={<Support />} />
          <Route path="/terminal" element={<Terminal />} />
          <Route path="/community" element={<Community />} />
          <Route path="/notifications" element={<Notifications />} />
          <Route path="/settings" element={<Settings />} />
        </Route>
      </Routes>
    </BrowserRouter>
  )
}
