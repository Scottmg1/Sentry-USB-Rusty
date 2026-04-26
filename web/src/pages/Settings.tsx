import { useState, useEffect, useRef } from "react"
import {
  Settings as SettingsIcon,
  RotateCcw,
  Unplug,
  RefreshCw,
  Bluetooth,
  Gauge,
  Wand2,
  Download,
  Loader2,
  CheckCircle,
  AlertCircle,
  Stethoscope,
  ChevronDown,
  ChevronRight,
  AlertTriangle,
  XCircle,
  HeartPulse,
  Wifi,
  WifiOff,
  Clock,
  Bell,
  Save,
  Users,
  Paintbrush,
  Volume2,
} from "lucide-react"
import { api } from "@/lib/api"
import { cn } from "@/lib/utils"
import { SetupWizard } from "@/components/setup/SetupWizard"
import { wsClient } from "@/lib/ws"
import { useKeepAwake } from "@/hooks/useKeepAwake"
import { useAwayMode } from "@/hooks/useAwayMode"

// ─── Shared primitives ──────────────────────────────────────────────────────

type ActionState = "idle" | "loading" | "success" | "error"

function ActionButton({
  icon: Icon,
  label,
  description,
  variant = "default",
  onClick,
  successMessage = "Done!",
  errorMessage = "Failed",
}: {
  icon: React.ElementType
  label: string
  description: string
  variant?: "default" | "danger"
  onClick: () => void | string | Promise<void | string>
  successMessage?: string
  errorMessage?: string
}) {
  const [state, setState] = useState<ActionState>("idle")
  const [msg, setMsg] = useState("")

  async function handleClick() {
    if (state === "loading") return
    setState("loading")
    setMsg("")
    try {
      const result = await onClick()
      if (result === "confirm") {
        setState("idle")
        setMsg("")
        return
      }
      setState("success")
      setMsg(typeof result === "string" ? result : successMessage)
      setTimeout(() => { setState("idle"); setMsg("") }, 5000)
    } catch (err) {
      setState("error")
      setMsg(err instanceof Error ? err.message : errorMessage)
      setTimeout(() => { setState("idle"); setMsg("") }, 5000)
    }
  }

  return (
    <button
      onClick={handleClick}
      disabled={state === "loading"}
      className="glass-card glass-card-hover flex items-start gap-3 p-4 text-left transition-all disabled:opacity-70"
    >
      <div
        className={cn(
          "flex h-10 w-10 shrink-0 items-center justify-center rounded-xl transition-colors",
          state === "loading" ? "bg-blue-500/15 text-blue-400" :
            state === "success" ? "bg-emerald-500/15 text-emerald-400" :
              state === "error" ? "bg-red-500/15 text-red-400" :
                variant === "danger"
                  ? "bg-red-500/15 text-red-400"
                  : "bg-blue-500/15 text-blue-400"
        )}
      >
        {state === "loading" ? (
          <Loader2 className="h-5 w-5 animate-spin" />
        ) : state === "success" ? (
          <CheckCircle className="h-5 w-5" />
        ) : state === "error" ? (
          <AlertCircle className="h-5 w-5" />
        ) : (
          <Icon className="h-5 w-5" />
        )}
      </div>
      <div>
        <p className="text-sm font-medium text-slate-200">{label}</p>
        <p className={cn(
          "mt-0.5 text-xs",
          state === "success" ? "text-emerald-400" :
            state === "error" ? "text-red-400" :
              "text-slate-500"
        )}>
          {msg || description}
        </p>
      </div>
    </button>
  )
}

// ─── USB Drive Toggle ──────────────────────────────────────────────────────

function UsbDriveToggle() {
  const [state, setState] = useState<ActionState>("idle")
  const [connected, setConnected] = useState<boolean | null>(null)

  useEffect(() => {
    let mounted = true
    async function poll() {
      try {
        const data = await api.getStatus()
        if (mounted) setConnected(data.drives_active === "yes")
      } catch { /* ignore */ }
    }
    poll()
    const id = setInterval(poll, 4000)
    return () => { mounted = false; clearInterval(id) }
  }, [])

  async function handleToggle() {
    if (state === "loading") return
    setState("loading")
    try {
      const res = await fetch("/api/system/toggle-drives", { method: "POST" })
      if (!res.ok) throw new Error("Failed to toggle drives")
      setState("success")
      // Re-fetch status after toggle
      try {
        const data = await api.getStatus()
        setConnected(data.drives_active === "yes")
      } catch { /* ignore */ }
      setTimeout(() => setState("idle"), 3000)
    } catch {
      setState("error")
      setTimeout(() => setState("idle"), 3000)
    }
  }

  return (
    <button
      onClick={handleToggle}
      disabled={state === "loading"}
      className="glass-card glass-card-hover flex items-start gap-3 p-4 text-left transition-all disabled:opacity-70"
    >
      <div className={cn(
        "flex h-10 w-10 shrink-0 items-center justify-center rounded-xl transition-colors",
        state === "loading" ? "bg-blue-500/15 text-blue-400" :
          state === "success" ? "bg-emerald-500/15 text-emerald-400" :
            state === "error" ? "bg-red-500/15 text-red-400" :
              "bg-blue-500/15 text-blue-400"
      )}>
        {state === "loading" ? (
          <Loader2 className="h-5 w-5 animate-spin" />
        ) : state === "success" ? (
          <CheckCircle className="h-5 w-5" />
        ) : state === "error" ? (
          <AlertCircle className="h-5 w-5" />
        ) : (
          <Unplug className="h-5 w-5" />
        )}
      </div>
      <div>
        <p className="text-sm font-medium text-slate-200">Toggle USB Drives</p>
        <div className="mt-0.5 flex items-center gap-1.5">
          {connected !== null && (
            <span className={cn(
              "inline-block h-1.5 w-1.5 rounded-full",
              connected ? "bg-emerald-400" : "bg-amber-400"
            )} />
          )}
          <p className={cn(
            "text-xs",
            state === "success" ? "text-emerald-400" :
              state === "error" ? "text-red-400" :
                "text-slate-500"
          )}>
            {state === "success" ? "Toggled!" : state === "error" ? "Failed" :
              connected === null ? "Checking..." :
                connected ? "Connected" : "Disconnected"}
          </p>
        </div>
      </div>
    </button>
  )
}

// ─── BLE Pairing ────────────────────────────────────────────────────────────

type BleState = "idle" | "initiating" | "waiting" | "polling" | "paired" | "error"

function BlePairButton() {
  const [bleState, setBleState] = useState<BleState>("idle")
  const [bleMsg, setBleMsg] = useState("")
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(() => {
    fetch("/api/system/ble-status?quick=true")
      .then(r => r.json())
      .then(data => {
        if (data.status === "paired") {
          setBleState("paired")
          setBleMsg("Paired — click to re-pair")
        } else if (data.status === "keys_generated") {
          setBleState("idle")
          setBleMsg("")
          // Keys exist but not flagged as paired — run a full (non-quick)
          // verification in the background.  If the car is actually paired
          // (e.g. user paired outside the UI flow), this will detect it and
          // write the paired flag so the status updates.
          fetch("/api/system/ble-status")
            .then(r => r.json())
            .then(d => {
              if (d.status === "paired") {
                setBleState("paired")
                setBleMsg("Paired — click to re-pair")
              }
            })
            .catch(() => { })
        }
      })
      .catch(() => { })
  }, [])

  useEffect(() => {
    const unsub = wsClient.subscribe("ble_status", (data: unknown) => {
      const d = data as { status: string; error?: string; output?: string }
      if (d.status === "pairing") {
        setBleState("initiating")
        setBleMsg("Sending pairing request to car...")
      } else if (d.status === "error") {
        setBleState("error")
        const errMsg = d.error || "Unknown error"
        if (errMsg.includes("maximum number of BLE")) {
          setBleMsg("Too many BLE devices active. Turn off Bluetooth on nearby phone keys and try again.")
        } else if (errMsg.includes("timed out")) {
          setBleMsg("BLE connection timed out. Make sure the Pi is near the car and try again.")
        } else {
          setBleMsg(errMsg)
        }
        cleanup()
      } else if (d.status === "waiting") {
        setBleState("waiting")
        setBleMsg("Tap your keycard on the center console to confirm pairing.")
        startPolling()
      }
    })
    return () => {
      unsub()
      cleanup()
    }
  }, []) // eslint-disable-line react-hooks/exhaustive-deps

  function cleanup() {
    if (pollRef.current) { clearInterval(pollRef.current); pollRef.current = null }
    if (timeoutRef.current) { clearTimeout(timeoutRef.current); timeoutRef.current = null }
  }

  function startPolling() {
    cleanup()
    let count = 0
    pollRef.current = setInterval(async () => {
      count++
      try {
        const res = await fetch("/api/system/ble-status")
        if (res.ok) {
          const data = await res.json()
          if (data.status === "paired") {
            setBleState("paired")
            setBleMsg("Successfully paired with car!")
            cleanup()
            return
          }
        }
      } catch { /* ignore fetch errors during polling */ }
      if (count >= 12) {
        setBleState("error")
        setBleMsg("Pairing timed out. Make sure you tapped your keycard on the center console, then try again.")
        cleanup()
      }
    }, 5000)
    // Safety timeout at 65 seconds
    timeoutRef.current = setTimeout(() => {
      if (bleState !== "paired" && bleState !== "error") {
        setBleState("error")
        setBleMsg("Pairing timed out. Please try again.")
        cleanup()
      }
    }, 65000)
  }

  async function handlePair() {
    setBleState("initiating")
    setBleMsg("Sending pairing request...")
    try {
      const res = await fetch("/api/system/ble-pair", { method: "POST" })
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        throw new Error(data.error || "Failed to initiate BLE pairing")
      }
    } catch (err) {
      setBleState("error")
      setBleMsg(err instanceof Error ? err.message : "Failed to initiate pairing")
    }
  }

  function handleReset() {
    cleanup()
    setBleState("idle")
    setBleMsg("")
  }

  function handlePairedClick() {
    handlePair()
  }

  const isActive = bleState !== "idle" && bleState !== "paired" && bleState !== "error"

  return (
    <button
      onClick={bleState === "idle" ? handlePair : bleState === "paired" ? handlePairedClick : bleState === "error" ? handleReset : undefined}
      disabled={isActive}
      className="glass-card glass-card-hover flex items-start gap-3 p-4 text-left transition-all disabled:opacity-70"
    >
      <div
        className={cn(
          "flex h-8 w-8 shrink-0 items-center justify-center rounded-xl transition-colors",
          bleState === "paired" ? "bg-emerald-500/15 text-emerald-400" :
            bleState === "error" ? "bg-red-500/15 text-red-400" :
              isActive ? "bg-amber-500/15 text-amber-400" :
                "bg-blue-500/15 text-blue-400"
        )}
      >
        {isActive ? (
          <Loader2 className="h-4 w-4 animate-spin" />
        ) : bleState === "paired" ? (
          <CheckCircle className="h-4 w-4" />
        ) : bleState === "error" ? (
          <AlertCircle className="h-4 w-4" />
        ) : (
          <Bluetooth className="h-4 w-4" />
        )}
      </div>
      <div className="min-w-0 flex-1">
        <p className="text-sm font-medium text-slate-200">
          {bleState === "paired" ? "BLE Paired" : bleState === "error" ? "BLE Pairing Failed" : isActive ? "Pairing..." : "Pair BLE"}
        </p>
        <p className={cn(
          "mt-0.5 text-xs",
          bleState === "paired" ? "text-emerald-400" :
            bleState === "error" ? "text-red-400" :
              bleState === "waiting" ? "text-amber-400 font-medium" :
                "text-slate-500"
        )}>
          {bleMsg || "Initiate Bluetooth Low Energy pairing with your car"}
        </p>
      </div>
    </button>
  )
}

// ─── Mobile Notifications ───────────────────────────────────────────────────

// Backend may return either `id` or legacy `pairing_id` depending on server version.
type PairedDevice = { id?: string; pairing_id?: string; device_name: string; platform: string; paired_at: string }
const devicePairingId = (d: PairedDevice) => d.id ?? d.pairing_id ?? ""

function MobileNotificationsSection() {
  const [pairingCode, setPairingCode] = useState<string | null>(null)
  const [expiresAt, setExpiresAt] = useState<string | null>(null)
  const [pairedDevices, setPairedDevices] = useState<PairedDevice[]>([])
  const [loading, setLoading] = useState(false)
  const [devicesLoading, setDevicesLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [countdown, setCountdown] = useState(0)
  const [testState, setTestState] = useState<"idle" | "loading" | "success" | "error">("idle")

  useEffect(() => {
    loadPairedDevices()
  }, [])

  useEffect(() => {
    if (!expiresAt) return
    const interval = setInterval(() => {
      const remaining = Math.max(0, Math.floor((new Date(expiresAt).getTime() - Date.now()) / 1000))
      setCountdown(remaining)
      if (remaining <= 0) {
        setPairingCode(null)
        setExpiresAt(null)
      }
    }, 1000)
    return () => clearInterval(interval)
  }, [expiresAt])

  async function loadPairedDevices() {
    try {
      const res = await fetch("/api/notifications/paired-devices")
      if (res.ok) {
        const data = await res.json()
        setPairedDevices(data.devices || [])
      }
    } catch { /* ignore */ }
    setDevicesLoading(false)
  }

  async function generateCode() {
    setLoading(true)
    setError(null)
    try {
      const res = await fetch("/api/notifications/generate-code", { method: "POST" })
      if (!res.ok) {
        const data = await res.json()
        throw new Error(data.error || "Failed to generate code")
      }
      const data = await res.json()
      setPairingCode(data.code)
      setExpiresAt(data.expires_at)
    } catch (err) {
      setError(err instanceof Error ? err.message : "Failed to generate code")
    }
    setLoading(false)
  }

  async function removeDevice(pairingId: string) {
    if (!pairingId) return
    try {
      const res = await fetch(`/api/notifications/paired-devices/${pairingId}`, { method: "DELETE" })
      if (res.ok) {
        setPairedDevices(prev => prev.filter(d => devicePairingId(d) !== pairingId))
      }
    } catch { /* ignore */ }
  }

  async function sendTest() {
    setTestState("loading")
    try {
      const res = await fetch("/api/notifications/test", { method: "POST" })
      if (res.ok) {
        setTestState("success")
      } else {
        setTestState("error")
      }
    } catch {
      setTestState("error")
    }
    setTimeout(() => setTestState("idle"), 3000)
  }

  return (
    <div className="glass-card overflow-hidden">
      <div className="flex items-center gap-3 border-b border-white/5 px-3 py-2">
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-violet-500/15">
          <Bell className="h-3.5 w-3.5 text-violet-400" />
        </div>
        <h3 className="text-sm font-semibold text-slate-200">Mobile Notifications</h3>
      </div>
      <div className="p-3 space-y-2">
        {/* Generate Code */}
        <div className="flex items-center gap-3">
          {pairingCode ? (
            <div className="flex items-center gap-4">
              <span className="font-mono text-xl font-bold tracking-widest text-blue-400">{pairingCode}</span>
              <span className="text-xs text-slate-500">
                Expires in {Math.floor(countdown / 60)}:{String(countdown % 60).padStart(2, "0")}
              </span>
            </div>
          ) : (
            <button
              onClick={generateCode}
              disabled={loading}
              className="rounded-lg bg-blue-500 px-3 py-2 text-xs font-medium text-white transition-colors hover:bg-blue-600 disabled:opacity-50"
            >
              {loading ? (
                <Loader2 className="inline h-3.5 w-3.5 animate-spin mr-1" />
              ) : null}
              Generate Code
            </button>
          )}
        </div>

        {pairingCode && (
          <p className="text-xs text-slate-600">
            Enter this code in the Sentry USB mobile app under Settings → Pair for Notifications.
          </p>
        )}

        {error && (
          <p className="text-xs text-red-400">{error}</p>
        )}

        {/* Paired Devices */}
        {devicesLoading ? (
          <p className="text-xs text-slate-600">Loading paired devices...</p>
        ) : pairedDevices.length > 0 ? (
          <div className="space-y-2">
            <p className="section-label">Paired Devices</p>
            {pairedDevices.map(device => (
              <div key={devicePairingId(device)} className="flex items-center gap-3 rounded-xl border border-white/5 bg-white/[0.02] px-3 py-2.5">
                <span className="text-sm text-slate-300">{device.device_name}</span>
                <span className="rounded-md bg-white/5 px-1.5 py-0.5 text-[10px] font-medium text-slate-500">{device.platform.toUpperCase()}</span>
                <span className="flex-1" />
                <button
                  onClick={() => removeDevice(devicePairingId(device))}
                  className="text-xs text-red-400/60 hover:text-red-400 transition-colors"
                >
                  Remove
                </button>
              </div>
            ))}
            <button
              onClick={sendTest}
              disabled={testState === "loading"}
              className="mt-1 w-full rounded-xl border border-white/5 bg-white/[0.03] px-3 py-2.5 text-xs text-slate-400 hover:bg-white/[0.06] hover:text-slate-300 transition-colors disabled:opacity-50"
            >
              {testState === "loading" ? "Sending..." : testState === "success" ? "✓ Test sent!" : testState === "error" ? "Failed to send" : "Send Test Notification"}
            </button>
          </div>
        ) : (
          <p className="text-xs text-slate-600">No mobile devices paired yet.</p>
        )}
      </div>
    </div>
  )
}

// ─── Health Check ───────────────────────────────────────────────────────────

type HealthItem = { name: string; status: "pass" | "warn" | "fail"; detail?: string }
type HealthCategory = { name: string; items: HealthItem[] }
type HealthReport = { summary: string; categories: HealthCategory[] }

function HealthCheckButton() {
  const [loading, setLoading] = useState(false)
  const [report, setReport] = useState<HealthReport | null>(null)
  const [expanded, setExpanded] = useState<Record<string, boolean>>({})

  async function runCheck() {
    setLoading(true)
    setReport(null)
    try {
      const res = await fetch("/api/system/health-check")
      if (!res.ok) throw new Error("Health check failed")
      const data = await res.json()
      setReport(data)
      const exp: Record<string, boolean> = {}
      for (const cat of data.categories) {
        if (cat.items.some((i: HealthItem) => i.status !== "pass")) exp[cat.name] = true
      }
      setExpanded(exp)
    } catch { setReport(null) }
    setLoading(false)
  }

  const statusIcon = (s: string) => {
    if (s === "pass") return <CheckCircle className="h-3.5 w-3.5 text-emerald-400" />
    if (s === "warn") return <AlertTriangle className="h-3.5 w-3.5 text-amber-400" />
    return <XCircle className="h-3.5 w-3.5 text-red-400" />
  }

  if (!report) {
    return (
      <button
        onClick={runCheck}
        disabled={loading}
        className="glass-card glass-card-hover flex items-start gap-3 p-4 text-left transition-all disabled:opacity-70"
      >
        <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-blue-500/15 text-blue-400">
          {loading ? <Loader2 className="h-5 w-5 animate-spin" /> : <Stethoscope className="h-5 w-5" />}
        </div>
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium text-slate-200">{loading ? "Running..." : "Health Check"}</p>
          <p className="mt-0.5 text-xs text-slate-500">Verify files, services & config</p>
        </div>
      </button>
    )
  }

  const failCount = report.categories.reduce((n, c) => n + c.items.filter(i => i.status === "fail").length, 0)
  const warnCount = report.categories.reduce((n, c) => n + c.items.filter(i => i.status === "warn").length, 0)

  return (
    <>
      {/* Button stays in the grid */}
      <button
        onClick={runCheck}
        disabled={loading}
        className="glass-card glass-card-hover flex items-start gap-3 p-4 text-left transition-all disabled:opacity-70"
      >
        <div className={cn(
          "flex h-10 w-10 shrink-0 items-center justify-center rounded-xl",
          failCount > 0 ? "bg-red-500/15 text-red-400" : warnCount > 0 ? "bg-amber-500/15 text-amber-400" : "bg-emerald-500/15 text-emerald-400"
        )}>
          {loading ? <Loader2 className="h-5 w-5 animate-spin" /> : <Stethoscope className="h-5 w-5" />}
        </div>
        <div className="min-w-0 flex-1">
          <p className="text-sm font-medium text-slate-200">{loading ? "Running..." : "Health Check"}</p>
          <p className="mt-0.5 text-xs">
            <span className={cn(
              failCount > 0 ? "text-red-400" : warnCount > 0 ? "text-amber-400" : "text-emerald-400"
            )}>{report.summary}</span>
            <span className="text-slate-600"> — tap to view</span>
          </p>
        </div>
      </button>

      {/* Modal overlay */}
      <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm" onClick={() => setReport(null)}>
        <div className="glass-card relative flex max-h-[80vh] w-full max-w-lg flex-col overflow-hidden" onClick={e => e.stopPropagation()}>
          <div className="flex shrink-0 items-center justify-between border-b border-white/5 px-4 py-3">
            <div className="flex items-center gap-2">
              <Stethoscope className={cn("h-4 w-4", failCount > 0 ? "text-red-400" : warnCount > 0 ? "text-amber-400" : "text-emerald-400")} />
              <span className="text-sm font-semibold text-slate-200">Health Check</span>
              <span className={cn(
                "rounded-full px-2 py-0.5 text-xs font-medium",
                failCount > 0 ? "bg-red-500/15 text-red-400" : warnCount > 0 ? "bg-amber-500/15 text-amber-400" : "bg-emerald-500/15 text-emerald-400"
              )}>{report.summary}</span>
            </div>
            <div className="flex gap-2">
              <button onClick={runCheck} disabled={loading}
                className="rounded-lg px-3 py-1 text-xs text-slate-400 hover:bg-white/5 hover:text-slate-200 disabled:opacity-50">
                {loading ? "Running..." : "Re-run"}
              </button>
              <button onClick={() => setReport(null)}
                className="rounded-lg px-3 py-1 text-xs text-slate-500 hover:bg-white/5 hover:text-slate-300">Close</button>
            </div>
          </div>
          <div className="flex-1 overflow-y-auto px-4 py-2">
            {report.categories.map(cat => {
              const isOpen = expanded[cat.name] ?? false
              const catFails = cat.items.filter(i => i.status === "fail").length
              const catWarns = cat.items.filter(i => i.status === "warn").length
              return (
                <div key={cat.name} className="border-b border-white/5 last:border-0">
                  <button
                    onClick={() => setExpanded(p => ({ ...p, [cat.name]: !isOpen }))}
                    className="flex w-full items-center gap-2 py-2 text-left"
                  >
                    {isOpen ? <ChevronDown className="h-3.5 w-3.5 text-slate-500" /> : <ChevronRight className="h-3.5 w-3.5 text-slate-500" />}
                    <span className="flex-1 text-xs font-medium text-slate-300">{cat.name}</span>
                    {catFails > 0 && <span className="rounded-md bg-red-500/15 px-1.5 py-0.5 text-[10px] text-red-400">{catFails} fail</span>}
                    {catWarns > 0 && <span className="rounded-md bg-amber-500/15 px-1.5 py-0.5 text-[10px] text-amber-400">{catWarns} warn</span>}
                    {catFails === 0 && catWarns === 0 && <span className="rounded-md bg-emerald-500/15 px-1.5 py-0.5 text-[10px] text-emerald-400">all pass</span>}
                  </button>
                  {isOpen && (
                    <div className="mb-2 space-y-0.5 pl-5">
                      {cat.items.map((item, i) => (
                        <div key={i} className="flex items-start gap-2 py-0.5">
                          {statusIcon(item.status)}
                          <span className="text-xs text-slate-300">{item.name}</span>
                          {item.detail && <span className="text-xs text-slate-600">— {item.detail}</span>}
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              )
            })}
          </div>
        </div>
      </div>
    </>
  )
}

// ─── Speed Test ─────────────────────────────────────────────────────────────

function SpeedTestButton() {
  const [running, setRunning] = useState(false)
  const [mbps, setMbps] = useState<string | null>(null)
  const [error, setError] = useState(false)
  const cancelRef = useRef(false)
  const readerRef = useRef<ReadableStreamDefaultReader<Uint8Array> | null>(null)

  async function runOnce() {
    const res = await fetch("/api/system/speedtest")
    if (!res.ok || !res.body) throw new Error("Speed test failed")

    const reader = res.body.getReader()
    readerRef.current = reader
    const start = Date.now()
    let totalBytes = 0
    let lastUpdate = start

    try {
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        totalBytes += value.length

        const now = Date.now()
        if (now - lastUpdate >= 250) {
          const elapsedSec = (now - start) / 1000
          if (elapsedSec > 0) setMbps(((totalBytes * 8) / elapsedSec / 1_000_000).toFixed(1))
          lastUpdate = now
        }
      }
    } finally {
      readerRef.current = null
    }

    const elapsed = (Date.now() - start) / 1000
    if (elapsed > 0 && totalBytes > 0) {
      setMbps(((totalBytes * 8) / elapsed / 1_000_000).toFixed(1))
    }
  }

  async function startTest() {
    setRunning(true)
    cancelRef.current = false
    setMbps(null)
    setError(false)
    while (!cancelRef.current) {
      try {
        await runOnce()
        if (cancelRef.current) break
      } catch {
        if (cancelRef.current) break
        setError(true)
        break
      }
    }
    setRunning(false)
  }

  function stopTest() {
    cancelRef.current = true
    if (readerRef.current) {
      readerRef.current.cancel().catch(() => {})
      readerRef.current = null
    }
  }

  return (
    <button
      onClick={running ? stopTest : startTest}
      className="glass-card glass-card-hover flex items-start gap-3 p-4 text-left transition-all"
    >
      <div className={cn(
        "flex h-10 w-10 shrink-0 items-center justify-center rounded-xl transition-colors",
        running ? "bg-amber-500/15 text-amber-400" : "bg-blue-500/15 text-blue-400"
      )}>
        {running ? <Loader2 className="h-5 w-5 animate-spin" /> : <Gauge className="h-5 w-5" />}
      </div>
      <div className="min-w-0 flex-1">
        <p className="text-sm font-medium text-slate-200">
          {running ? "Stop Speed Test" : "Speed Test"}
        </p>
        {mbps ? (
          <p className="mt-0.5 text-sm font-bold text-blue-400">{mbps} <span className="text-xs font-normal text-slate-500">Mbps</span></p>
        ) : (
          <p className="mt-0.5 text-xs text-slate-500">
            {error ? "Speed test failed" : running ? "Starting..." : "Test network throughput"}
          </p>
        )}
      </div>
    </button>
  )
}

// ─── Raw Config Editor ──────────────────────────────────────────────────────

type UpdateStatus = "idle" | "checking_internet" | "checking" | "downloading" | "installing" | "updating_scripts" | "restarting" | "reconnecting" | "done" | "error"

interface RawConfigEntry {
  value: string
  active: boolean
}

function RawConfigEditor({ config, onClose }: { config: Record<string, RawConfigEntry>; onClose: () => void }) {
  const [entries, setEntries] = useState<Record<string, { value: string; active: boolean }>>(() => {
    const e: Record<string, { value: string; active: boolean }> = {}
    for (const [k, v] of Object.entries(config)) {
      e[k] = { value: v.value, active: v.active }
    }
    return e
  })
  const [saving, setSaving] = useState(false)
  const [saveMsg, setSaveMsg] = useState<string | null>(null)
  const [newKey, setNewKey] = useState("")
  const [newVal, setNewVal] = useState("")

  const sortedKeys = Object.keys(entries).sort()

  async function handleSave() {
    setSaving(true)
    setSaveMsg(null)
    try {
      const configData: Record<string, string> = {}
      for (const [k, v] of Object.entries(entries)) {
        if (v.active) configData[k] = v.value
      }
      const res = await fetch("/api/setup/config", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(configData),
      })
      if (!res.ok) throw new Error("Failed to save")
      setSaveMsg("Saved successfully")
      setTimeout(() => setSaveMsg(null), 3000)
    } catch (err) {
      setSaveMsg(err instanceof Error ? err.message : "Save failed")
    } finally {
      setSaving(false)
    }
  }

  function addEntry() {
    if (!newKey.trim()) return
    setEntries(prev => ({ ...prev, [newKey.trim()]: { value: newVal, active: true } }))
    setNewKey("")
    setNewVal("")
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
      <div className="glass-card relative flex h-[90vh] w-full flex-col overflow-hidden sm:h-[85vh] sm:max-w-3xl">
        <div className="flex shrink-0 items-center justify-between border-b border-white/5 px-6 py-4">
          <h2 className="text-lg font-semibold text-slate-100">Raw Configuration</h2>
          <div className="flex gap-2">
            {saveMsg && <span className={cn("text-xs self-center", saveMsg.includes("success") ? "text-emerald-400" : "text-red-400")}>{saveMsg}</span>}
            <button onClick={handleSave} disabled={saving}
              className="rounded-xl bg-blue-500 px-4 py-1.5 text-sm font-medium text-white hover:bg-blue-600 disabled:opacity-50">
              {saving ? "Saving..." : "Save"}
            </button>
            <button onClick={onClose}
              className="rounded-xl px-3 py-1.5 text-sm text-slate-500 hover:bg-white/5 hover:text-slate-300">Close</button>
          </div>
        </div>
        <div className="flex-1 overflow-y-auto px-6 py-4">
          <div className="space-y-1">
            {sortedKeys.map(key => (
              <div key={key} className="flex items-center gap-2 rounded-xl border border-white/5 bg-white/[0.02] px-3 py-2.5">
                <input type="checkbox" checked={entries[key].active}
                  onChange={e => setEntries(prev => ({ ...prev, [key]: { ...prev[key], active: e.target.checked } }))}
                  className="toggle-switch" />
                <span className={cn("w-28 shrink-0 truncate font-mono text-xs sm:w-48", entries[key].active ? "text-blue-400" : "text-slate-600")}>{key}</span>
                <input type="text" value={entries[key].value}
                  onChange={e => setEntries(prev => ({ ...prev, [key]: { ...prev[key], value: e.target.value } }))}
                  className="flex-1 rounded-lg border border-white/10 bg-white/5 px-2.5 py-1.5 font-mono text-xs text-slate-200 outline-none focus:border-blue-500/50" />
                <button onClick={() => setEntries(prev => { const n = { ...prev }; delete n[key]; return n })}
                  className="text-xs text-slate-600 hover:text-red-400 transition-colors">✕</button>
              </div>
            ))}
          </div>
          <div className="mt-4 flex items-center gap-2">
            <input type="text" value={newKey} onChange={e => setNewKey(e.target.value)}
              placeholder="NEW_KEY" className="w-48 rounded-lg border border-white/10 bg-white/5 px-2.5 py-1.5 font-mono text-xs text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50" />
            <input type="text" value={newVal} onChange={e => setNewVal(e.target.value)}
              placeholder="value" className="flex-1 rounded-lg border border-white/10 bg-white/5 px-2.5 py-1.5 font-mono text-xs text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50" />
            <button onClick={addEntry}
              className="rounded-lg bg-blue-500/20 px-3 py-1.5 text-xs font-medium text-blue-400 hover:bg-blue-500/30">Add</button>
          </div>
        </div>
      </div>
    </div>
  )
}

// ─── Preferences: Keep Awake ────────────────────────────────────────────────

const KEEP_AWAKE_MODES = [
  { value: "", label: "Off", desc: "Keep-awake disabled" },
  { value: "manual", label: "Manual", desc: "Button on Dashboard with duration picker" },
  { value: "auto", label: "Automatic", desc: "Stays awake while you're browsing" },
]

function KeepAwakePreference() {
  const { mode, updateMode } = useKeepAwake()

  return (
    <div className="glass-card overflow-hidden p-3">
      <div className="flex items-center gap-3 mb-2">
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-rose-500/15">
          <HeartPulse className="h-3.5 w-3.5 text-rose-400" />
        </div>
        <h3 className="text-sm font-semibold text-slate-200">Keep Awake</h3>
      </div>
      <div className="grid grid-cols-3 gap-1.5">
        {KEEP_AWAKE_MODES.map((m) => (
          <button
            key={m.value}
            onClick={() => updateMode(m.value)}
            className={cn(
              "rounded-lg border px-2.5 py-1.5 text-center text-xs font-medium transition-all",
              (mode ?? "") === m.value
                ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
            )}
          >
            {m.label}
          </button>
        ))}
      </div>
      <p className="mt-1.5 text-[10px] text-slate-600">
        {KEEP_AWAKE_MODES.find(m => m.value === (mode ?? ""))?.desc}
      </p>
    </div>
  )
}

// ─── Preferences: Away Mode ─────────────────────────────────────────────────

const AWAY_MODE_PRESETS = [
  { value: 60, label: "1h" },
  { value: 120, label: "2h" },
  { value: 240, label: "4h" },
  { value: 480, label: "8h" },
]

function AwayModeControl() {
  const { status, enable, disable } = useAwayMode()
  const [selectedDuration, setSelectedDuration] = useState(240)
  const [customHours, setCustomHours] = useState("")
  const [customMinutes, setCustomMinutes] = useState("")
  const [useCustom, setUseCustom] = useState(false)
  const [enabling, setEnabling] = useState(false)
  const [confirmOpen, setConfirmOpen] = useState(false)

  const isActive = status.state === "active"

  function getCustomMinutes() {
    const h = parseInt(customHours) || 0
    const m = parseInt(customMinutes) || 0
    return h * 60 + m
  }

  function handleEnableClick() {
    const durationMin = useCustom ? Math.min(Math.max(getCustomMinutes(), 1), 1440) : selectedDuration
    if (isNaN(durationMin) || durationMin <= 0) return
    setConfirmOpen(true)
  }

  async function handleConfirmEnable() {
    const durationMin = useCustom ? Math.min(Math.max(getCustomMinutes(), 1), 1440) : selectedDuration
    setConfirmOpen(false)
    setEnabling(true)
    await enable(durationMin)
    setEnabling(false)
  }

  function formatRemaining(sec: number) {
    const h = Math.floor(sec / 3600)
    const m = Math.floor((sec % 3600) / 60)
    if (h > 0) return `${h}h ${m}m remaining`
    return `${m}m remaining`
  }

  function getProgress() {
    if (!status.enabled_at || !status.expires_at || !status.remaining_sec) return 0
    const total = (new Date(status.expires_at).getTime() - new Date(status.enabled_at).getTime()) / 1000
    if (total <= 0) return 100
    return Math.round(((total - status.remaining_sec) / total) * 100)
  }

  return (
    <div className="glass-card overflow-hidden p-3">
      <div className="flex items-center gap-3 mb-2">
        <div className={cn(
          "flex h-7 w-7 shrink-0 items-center justify-center rounded-lg",
          isActive ? "bg-blue-500/15" : "bg-slate-500/10"
        )}>
          {isActive ? (
            <Wifi className="h-3.5 w-3.5 text-blue-400" />
          ) : (
            <WifiOff className="h-3.5 w-3.5 text-slate-500" />
          )}
        </div>
        <h3 className="text-sm font-semibold text-slate-200">Away Mode</h3>
        {isActive && (
          <span className="ml-auto rounded-full bg-blue-500/15 px-2 py-0.5 text-[10px] font-semibold text-blue-400">
            Active
          </span>
        )}
      </div>

      <div className="space-y-2">
        {/* Non-RTC warning */}
        {status.has_rtc === false && (
          <div className="flex gap-2 rounded-lg border border-amber-500/20 bg-amber-500/5 p-2 text-[10px] text-amber-400/80">
            <AlertTriangle className="h-3 w-3 shrink-0 mt-0.5" />
            <p>No RTC detected — timer saved every 30s, may lose accuracy on reboot.</p>
          </div>
        )}

        {isActive ? (
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-2 text-xs text-blue-400">
                <Clock className="h-3 w-3" />
                <span className="font-medium">{formatRemaining(status.remaining_sec ?? 0)}</span>
              </div>
              <button
                onClick={disable}
                className="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-1.5 text-[11px] font-medium text-red-400 transition-colors hover:bg-red-500/20"
              >
                Disable
              </button>
            </div>
            <div className="h-1 rounded-full bg-white/5 overflow-hidden">
              <div
                className="h-full rounded-full bg-blue-500/60 transition-all duration-1000"
                style={{ width: `${getProgress()}%` }}
              />
            </div>
          </div>
        ) : (
          <div className="space-y-2">
            <div className="flex items-center gap-1.5">
              {AWAY_MODE_PRESETS.map((p) => (
                <button
                  key={p.value}
                  onClick={() => { setSelectedDuration(p.value); setUseCustom(false) }}
                  className={cn(
                    "rounded-lg border px-2.5 py-1.5 text-xs font-medium transition-all",
                    !useCustom && selectedDuration === p.value
                      ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                      : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
                  )}
                >
                  {p.label}
                </button>
              ))}
              <button
                onClick={() => setUseCustom(true)}
                className={cn(
                  "rounded-lg border px-2.5 py-1.5 text-xs font-medium transition-all",
                  useCustom
                    ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                    : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
                )}
              >
                Custom
              </button>
              <button
                onClick={handleEnableClick}
                disabled={enabling || (useCustom && getCustomMinutes() <= 0)}
                className="ml-auto rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 transition-colors hover:bg-blue-500/25 disabled:opacity-40"
              >
                {enabling ? "Enabling..." : "Enable"}
              </button>
            </div>
            {useCustom && (
              <div className="flex items-center gap-1.5">
                <input
                  type="number"
                  min="0"
                  max="24"
                  step="1"
                  placeholder="0"
                  value={customHours}
                  onChange={(e) => setCustomHours(e.target.value)}
                  className="w-14 rounded-lg border border-white/10 bg-white/5 px-2 py-1.5 text-xs text-slate-200 placeholder:text-slate-600 focus:border-blue-500/40 focus:outline-none"
                />
                <span className="text-[10px] text-slate-500">hrs</span>
                <input
                  type="number"
                  min="0"
                  max="59"
                  step="1"
                  placeholder="0"
                  value={customMinutes}
                  onChange={(e) => setCustomMinutes(e.target.value)}
                  className="w-14 rounded-lg border border-white/10 bg-white/5 px-2 py-1.5 text-xs text-slate-200 placeholder:text-slate-600 focus:border-blue-500/40 focus:outline-none"
                />
                <span className="text-[10px] text-slate-500">min</span>
              </div>
            )}

            {/* Confirmation dialog */}
            {confirmOpen && (
              <div className="rounded-xl border border-amber-500/30 bg-amber-500/5 p-4 space-y-3">
                <div className="flex items-start gap-2">
                  <AlertTriangle className="h-4 w-4 shrink-0 mt-0.5 text-amber-400" />
                  <div className="text-xs text-amber-300/90 space-y-1">
                    <p className="font-semibold">You may lose connection to this page</p>
                    <p className="text-amber-400/70">
                      Enabling Away Mode will start the WiFi hotspot and disconnect from your home network.
                      {status.ap_ssid && (
                        <> To continue using the web UI, connect your device to <span className="font-medium text-amber-300">"{status.ap_ssid}"</span></>
                      )}
                      {status.ap_ip && (
                        <> and navigate to <span className="font-medium text-amber-300">http://{status.ap_ip}</span></>
                      )}
                      .
                    </p>
                  </div>
                </div>
                <div className="flex gap-2">
                  <button
                    onClick={handleConfirmEnable}
                    className="flex-1 rounded-xl border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs font-medium text-amber-400 transition-colors hover:bg-amber-500/20"
                  >
                    Enable Away Mode
                  </button>
                  <button
                    onClick={() => setConfirmOpen(false)}
                    className="flex-1 rounded-xl border border-white/10 bg-white/[0.02] px-3 py-2 text-xs font-medium text-slate-400 transition-colors hover:bg-white/[0.05]"
                  >
                    Cancel
                  </button>
                </div>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  )
}

// ─── Config Backup ──────────────────────────────────────────────────────────

interface BackupEntry {
  date: string
  timestamp: string
  location: string
  size: number
  filename: string
}

function ConfigBackupSection() {
  const [backupLocation, setBackupLocation] = useState<string>("archive")
  const [lastBackup, setLastBackup] = useState<{ date: string; timestamp: string } | null>(null)
  const [backupState, setBackupState] = useState<ActionState>("idle")
  const [loaded, setLoaded] = useState(false)

  // Restore state
  const [showRestore, setShowRestore] = useState(false)
  const [backups, setBackups] = useState<BackupEntry[]>([])
  const [loadingBackups, setLoadingBackups] = useState(false)
  const [restoreState, setRestoreState] = useState<"idle" | "confirm" | "restoring" | "success" | "error">("idle")
  const [selectedBackup, setSelectedBackup] = useState<BackupEntry | null>(null)
  const [restoreResult, setRestoreResult] = useState<{ date: string; hostname: string } | null>(null)

  useEffect(() => {
    // Load current preference
    fetch("/api/config/preference?key=backup_location")
      .then((r) => r.json())
      .then((d) => {
        if (d.value) setBackupLocation(d.value)
      })
      .catch(() => {})

    // Load latest backup info
    fetch("/api/system/backups")
      .then((r) => r.json())
      .then((backups: { date: string; timestamp: string }[]) => {
        if (backups && backups.length > 0) setLastBackup(backups[0])
        setLoaded(true)
      })
      .catch(() => setLoaded(true))
  }, [])

  // Load backup list when restore panel opens
  useEffect(() => {
    if (!showRestore) return
    setLoadingBackups(true)
    fetch("/api/system/backups")
      .then((r) => r.json())
      .then((data: BackupEntry[]) => {
        setBackups(data || [])
        setLoadingBackups(false)
      })
      .catch(() => {
        setBackups([])
        setLoadingBackups(false)
      })
  }, [showRestore])

  async function handleLocationChange(loc: string) {
    setBackupLocation(loc)
    await fetch("/api/config/preference", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key: "backup_location", value: loc }),
    })
  }

  async function handleBackupNow() {
    setBackupState("loading")
    try {
      const res = await fetch("/api/system/backup?force=1", { method: "POST" })
      if (!res.ok) throw new Error("Backup failed")
      const result = await res.json()
      setLastBackup({ date: result.date, timestamp: new Date().toISOString() })
      setBackupState("success")
      setTimeout(() => setBackupState("idle"), 3000)
    } catch {
      setBackupState("error")
      setTimeout(() => setBackupState("idle"), 3000)
    }
  }

  async function handleRestore() {
    if (!selectedBackup) return
    setRestoreState("restoring")
    try {
      // Fetch the full backup data
      const backupRes = await fetch(`/api/system/backup/${selectedBackup.date}`)
      if (!backupRes.ok) throw new Error("Failed to fetch backup")
      const backupData = await backupRes.json()

      // Send to restore endpoint
      const restoreRes = await fetch("/api/system/restore", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(backupData),
      })
      if (!restoreRes.ok) throw new Error("Restore failed")
      const result = await restoreRes.json()

      setRestoreResult({ date: result.date, hostname: result.hostname })
      setRestoreState("success")
    } catch {
      setRestoreState("error")
      setTimeout(() => {
        setRestoreState("idle")
        setSelectedBackup(null)
      }, 3000)
    }
  }

  return (
    <div className="glass-card overflow-hidden p-3">
      <div className="flex items-center gap-3 mb-2">
        <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-lg bg-blue-500/15">
          <Save className="h-3.5 w-3.5 text-blue-400" />
        </div>
        <h3 className="text-sm font-semibold text-slate-200">Config Backup</h3>
      </div>
      <div className="space-y-2">
        {/* Location + backup trigger inline */}
        <div className="flex items-center gap-2">
          <span className="text-[10px] text-slate-500">Location:</span>
          <div className="flex gap-1">
            <button
              onClick={() => handleLocationChange("archive")}
              className={cn(
                "rounded-lg border px-2.5 py-1 text-[11px] font-medium transition-all",
                backupLocation === "archive"
                  ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                  : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
              )}
            >
              Archive Server
            </button>
            <button
              onClick={() => handleLocationChange("ssd")}
              className={cn(
                "rounded-lg border px-2.5 py-1 text-[11px] font-medium transition-all",
                backupLocation === "ssd"
                  ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                  : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
              )}
            >
              Local SSD
            </button>
          </div>
          <div className="ml-auto flex items-center gap-2">
            <span className="text-[10px] text-slate-600">
              {loaded && lastBackup
                ? new Date(lastBackup.timestamp).toLocaleDateString(undefined, { month: "short", day: "numeric" })
                : loaded ? "No backups" : ""}
            </span>
            <button
              onClick={handleBackupNow}
              disabled={backupState === "loading"}
              className={cn(
                "rounded-lg px-2.5 py-1 text-[11px] font-medium transition-all",
                backupState === "success"
                  ? "bg-emerald-500/20 text-emerald-400"
                  : backupState === "error"
                  ? "bg-red-500/20 text-red-400"
                  : backupState === "loading"
                  ? "bg-blue-500/20 text-blue-400"
                  : "bg-white/5 text-slate-300 hover:bg-white/10"
              )}
            >
              {backupState === "loading" && "Backing up..."}
              {backupState === "success" && "Done!"}
              {backupState === "error" && "Failed"}
              {backupState === "idle" && "Backup Now"}
            </button>
          </div>
        </div>

        {/* Restore section */}
        <div className="border-t border-white/5 pt-2">
          {restoreState === "success" && restoreResult ? (
            <div className="rounded-xl border border-emerald-500/20 bg-emerald-500/5 p-4">
              <div className="flex items-start gap-3">
                <CheckCircle className="mt-0.5 h-5 w-5 shrink-0 text-emerald-400" />
                <div>
                  <p className="text-sm font-medium text-emerald-300">Config Restored</p>
                  <p className="mt-1 text-xs text-slate-400">
                    Backup from {restoreResult.date} has been restored
                    {restoreResult.hostname ? ` (${restoreResult.hostname})` : ""}.
                    Run setup to apply the restored configuration.
                  </p>
                  <button
                    onClick={() => {
                      setRestoreState("idle")
                      setSelectedBackup(null)
                      setRestoreResult(null)
                      setShowRestore(false)
                    }}
                    className="mt-3 rounded-lg bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10"
                  >
                    Done
                  </button>
                </div>
              </div>
            </div>
          ) : restoreState === "confirm" && selectedBackup ? (
            <div className="rounded-xl border border-amber-500/20 bg-amber-500/5 p-4">
              <div className="flex items-start gap-3">
                <AlertTriangle className="mt-0.5 h-5 w-5 shrink-0 text-amber-400" />
                <div>
                  <p className="text-sm font-medium text-amber-300">Confirm Restore</p>
                  <p className="mt-1 text-xs text-slate-400">
                    This will overwrite your current configuration with the backup from{" "}
                    <span className="text-slate-300">
                      {new Date(selectedBackup.timestamp).toLocaleDateString(undefined, {
                        weekday: "short",
                        month: "short",
                        day: "numeric",
                        year: "numeric",
                      })}
                    </span>
                    . SSH keys, BLE pairing, and notification credentials will also be restored.
                    You will need to run setup afterward to apply changes.
                  </p>
                  <div className="mt-3 flex gap-2">
                    <button
                      onClick={() => { setRestoreState("idle"); setSelectedBackup(null) }}
                      className="rounded-lg border border-white/10 px-3 py-1.5 text-xs font-medium text-slate-400 transition-colors hover:bg-white/5"
                    >
                      Cancel
                    </button>
                    <button
                      onClick={handleRestore}
                      className="rounded-lg bg-amber-500/20 px-3 py-1.5 text-xs font-medium text-amber-300 transition-colors hover:bg-amber-500/30"
                    >
                      Restore Config
                    </button>
                  </div>
                </div>
              </div>
            </div>
          ) : !showRestore ? (
            <button
              onClick={() => setShowRestore(true)}
              className="flex w-full items-center justify-center gap-2 rounded-xl border border-white/10 bg-white/[0.02] px-4 py-2.5 text-xs text-slate-400 transition-colors hover:border-white/20 hover:bg-white/[0.04] hover:text-slate-300"
            >
              <RotateCcw className="h-3.5 w-3.5" />
              Restore from Backup
            </button>
          ) : (
            <div className="space-y-2">
              <div className="flex items-center justify-between">
                <p className="text-xs font-medium text-slate-400">Available Backups</p>
                <button
                  onClick={() => { setShowRestore(false); setSelectedBackup(null); setRestoreState("idle") }}
                  className="text-[10px] text-slate-600 transition-colors hover:text-slate-400"
                >
                  Close
                </button>
              </div>
              {loadingBackups ? (
                <div className="flex items-center justify-center py-4">
                  <Loader2 className="h-4 w-4 animate-spin text-blue-400" />
                  <span className="ml-2 text-xs text-slate-500">Scanning for backups...</span>
                </div>
              ) : backups.length === 0 ? (
                <p className="py-3 text-center text-xs text-slate-500">
                  No backups found. Backups are created automatically after each archive.
                </p>
              ) : (
                <div className="max-h-48 space-y-1.5 overflow-y-auto">
                  {backups.map((b) => (
                    <button
                      key={b.date}
                      onClick={() => { setSelectedBackup(b); setRestoreState("confirm") }}
                      disabled={restoreState === "restoring"}
                      className="flex w-full items-center justify-between rounded-lg border border-white/5 bg-white/[0.02] px-3 py-2.5 text-left transition-colors hover:bg-white/[0.05] hover:border-white/10 disabled:opacity-50"
                    >
                      <div>
                        <p className="text-xs font-medium text-slate-300">
                          {new Date(b.timestamp).toLocaleDateString(undefined, {
                            weekday: "short",
                            month: "short",
                            day: "numeric",
                            year: "numeric",
                          })}
                        </p>
                        <p className="text-[10px] text-slate-500">
                          {new Date(b.timestamp).toLocaleTimeString(undefined, {
                            hour: "2-digit",
                            minute: "2-digit",
                          })}
                          {" · "}
                          {b.location === "archive" ? "Archive server" : "Local SSD"}
                          {" · "}
                          {(b.size / 1024).toFixed(1)} KB
                        </p>
                      </div>
                      {restoreState === "restoring" && selectedBackup?.date === b.date ? (
                        <Loader2 className="h-4 w-4 animate-spin text-blue-400" />
                      ) : (
                        <RotateCcw className="h-3.5 w-3.5 text-slate-500" />
                      )}
                    </button>
                  ))}
                </div>
              )}
            </div>
          )}
          {restoreState === "error" && (
            <div className="mt-2 flex items-center gap-2 rounded-lg bg-red-500/10 px-3 py-2 text-xs text-red-400">
              <AlertCircle className="h-3.5 w-3.5 shrink-0" />
              Restore failed. Please try again.
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

// ─── Community Features section ────────────────────────────────────────────

function CommunityFeaturesSection({ onOpenWizardForWraps }: { onOpenWizardForWraps: () => void }) {
  const [wrapsEnabled, setWrapsEnabled] = useState<boolean>(true)
  const [chimesEnabled, setChimesEnabled] = useState<boolean>(true)
  const [hasWrapsPartition, setHasWrapsPartition] = useState<boolean>(false)
  const [loaded, setLoaded] = useState(false)

  function refreshState() {
    Promise.all([
      fetch("/api/config/preference?key=community_wraps_enabled").then((r) => r.json()).catch(() => ({ value: null })),
      fetch("/api/config/preference?key=community_chimes_enabled").then((r) => r.json()).catch(() => ({ value: null })),
      fetch("/api/setup/config").then((r) => r.ok ? r.json() : null).catch(() => null),
    ]).then(([wraps, chimes, config]) => {
      setWrapsEnabled(wraps?.value == null ? true : wraps.value !== "disabled")
      setChimesEnabled(chimes?.value == null ? true : chimes.value !== "disabled")
      if (config) {
        const entry = config.WRAPS_SIZE as { value?: string; active?: boolean } | undefined
        const raw = entry?.active ? (entry.value ?? "") : ""
        const num = parseInt(raw.replace(/[^0-9]/g, "") || "0", 10)
        setHasWrapsPartition(num > 0)
      } else {
        setHasWrapsPartition(false)
      }
      setLoaded(true)
    })
  }

  useEffect(() => {
    refreshState()
    function onPrefsChanged() { refreshState() }
    window.addEventListener("community-prefs-changed", onPrefsChanged)
    return () => window.removeEventListener("community-prefs-changed", onPrefsChanged)
  }, [])

  async function setPref(key: "community_wraps_enabled" | "community_chimes_enabled", enabled: boolean) {
    await fetch("/api/config/preference", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key, value: enabled ? "enabled" : "disabled" }),
    }).catch(() => {})
    window.dispatchEvent(new CustomEvent("community-prefs-changed"))
  }

  async function handleWrapsToggle(next: boolean) {
    if (next && !hasWrapsPartition) {
      // Need to allocate a partition — open the setup wizard pre-set to enable wraps.
      onOpenWizardForWraps()
      return
    }
    setWrapsEnabled(next)
    await setPref("community_wraps_enabled", next)
  }

  async function handleChimesToggle(next: boolean) {
    setChimesEnabled(next)
    await setPref("community_chimes_enabled", next)
  }

  return (
    <div className="glass-card overflow-hidden">
      <div className="flex items-center gap-3 border-b border-white/5 px-3 py-2.5">
        <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-blue-500/20">
          <Users className="h-4 w-4 text-blue-400" />
        </div>
        <div className="min-w-0 flex-1">
          <p className="text-xs font-semibold text-slate-200">Community Features</p>
          <p className="text-[10px] text-slate-500">Choose which community features appear in the sidebar</p>
        </div>
      </div>

      {/* Wraps toggle */}
      <div className="px-3 py-2.5">
        <label className="flex cursor-pointer items-start justify-between gap-3">
          <div className="flex items-start gap-2">
            <Paintbrush className="mt-0.5 h-3.5 w-3.5 shrink-0 text-blue-400" />
            <div>
              <span className="text-xs font-medium text-slate-200">Wraps &amp; License Plates</span>
              <span className="block text-[10px] text-slate-500 mt-0.5">
                {hasWrapsPartition
                  ? wrapsEnabled
                    ? "Tab visible. Toggle off to hide without removing your wraps drive."
                    : "Hidden. Toggle on to show — your wraps drive is preserved."
                  : "Requires a dedicated drive partition. Toggling on opens the setup wizard."}
              </span>
            </div>
          </div>
          <input
            type="checkbox"
            checked={wrapsEnabled}
            disabled={!loaded}
            onChange={(e) => handleWrapsToggle(e.target.checked)}
            className="toggle-switch mt-0.5"
          />
        </label>
      </div>

      {/* Chimes toggle */}
      <div className="border-t border-white/5 px-3 py-2.5">
        <label className="flex cursor-pointer items-start justify-between gap-3">
          <div className="flex items-start gap-2">
            <Volume2 className="mt-0.5 h-3.5 w-3.5 shrink-0 text-blue-400" />
            <div>
              <span className="text-xs font-medium text-slate-200">Lock Chimes</span>
              <span className="block text-[10px] text-slate-500 mt-0.5">
                Custom Tesla lock-chime sounds. No partition required — toggle freely.
              </span>
            </div>
          </div>
          <input
            type="checkbox"
            checked={chimesEnabled}
            disabled={!loaded}
            onChange={(e) => handleChimesToggle(e.target.checked)}
            className="toggle-switch mt-0.5"
          />
        </label>
      </div>
    </div>
  )
}

// ─── Tab definitions ────────────────────────────────────────────────────────

// ─── Main Settings page ────────────────────────────────────────────────────

export default function Settings() {
  const [confirmReboot, setConfirmReboot] = useState(false)
  const [wizardOpen, setWizardOpen] = useState(false)
  const [wizardInitialData, setWizardInitialData] = useState<Record<string, string> | undefined>(undefined)
  const [rawConfigOpen, setRawConfigOpen] = useState(false)
  const [rawConfig, setRawConfig] = useState<Record<string, RawConfigEntry> | null>(null)
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus>("idle")
  const [updateError, setUpdateError] = useState<string | null>(null)
  const [updateMessage, setUpdateMessage] = useState<string | null>(null)
  const [isCheckingUpdate, setIsCheckingUpdate] = useState(false)
  const [version, setVersion] = useState<string | null>(null)
  const [piConfig, setPiConfig] = useState<{ uses_ble: string } | null>(null)
  const [stableUpdate, setStableUpdate] = useState<{ version: string; release_url: string; release_notes: string } | null>(null)
  const [prereleaseUpdate, setPrereleaseUpdate] = useState<{ version: string; release_url: string; release_notes: string } | null>(null)
  const [revertStable, setRevertStable] = useState<{ version: string; release_url: string; release_notes: string } | null>(null)
  const [autoUpdateEnabled, setAutoUpdateEnabled] = useState(true)
  const [includePrerelease, setIncludePrerelease] = useState(false)

  useEffect(() => {
    fetch("/api/system/version")
      .then(r => r.json())
      .then(data => setVersion(data.version || "unknown"))
      .catch(() => setVersion("unknown"))
  }, [updateStatus])

  useEffect(() => {
    fetch("/api/config")
      .then(r => r.json())
      .then(data => setPiConfig(data))
      .catch(() => { })
    fetch("/api/system/update-status")
      .then(r => r.json())
      .then(data => {
        if (data.stable?.available) {
          setStableUpdate({ version: data.stable.version, release_url: data.stable.release_url, release_notes: data.stable.release_notes })
        } else if (data.update_available) {
          setStableUpdate({ version: data.latest_version, release_url: data.release_url, release_notes: data.release_notes })
        }
        if (data.prerelease?.available) {
          setPrereleaseUpdate({ version: data.prerelease.version, release_url: data.prerelease.release_url, release_notes: data.prerelease.release_notes })
        }
        if (data.revert_stable) {
          setRevertStable({ version: data.revert_stable.version, release_url: data.revert_stable.release_url, release_notes: data.revert_stable.release_notes })
        }
      })
      .catch(() => { })
    fetch("/api/config/preference?key=auto_update_check")
      .then(r => r.json())
      .then(data => setAutoUpdateEnabled(data.value !== "disabled"))
      .catch(() => { })
    fetch("/api/config/preference?key=update_channel")
      .then(r => r.json())
      .then(data => setIncludePrerelease(data.value === "prerelease"))
      .catch(() => { })
  }, [])

  async function handleCheckForUpdate(oneTimePrerelease = false) {
    setIsCheckingUpdate(true)
    setStableUpdate(null)
    setPrereleaseUpdate(null)
    setRevertStable(null)
    setUpdateError(null)
    try {
      const wantPrerelease = includePrerelease || oneTimePrerelease
      const url = "/api/system/check-update" + (wantPrerelease ? "?include_prerelease=true" : "")
      const res = await fetch(url, { method: "POST" })
      if (!res.ok) throw new Error("Failed to check for updates")
      const data = await res.json()
      if (data.error) {
        setUpdateError(data.error)
      } else {
        let foundAny = false
        if (data.stable?.available) {
          setStableUpdate({ version: data.stable.version, release_url: data.stable.release_url, release_notes: data.stable.release_notes })
          foundAny = true
        } else if (data.update_available) {
          setStableUpdate({ version: data.latest_version, release_url: data.release_url, release_notes: data.release_notes })
          foundAny = true
        }
        if (data.prerelease?.available) {
          setPrereleaseUpdate({ version: data.prerelease.version, release_url: data.prerelease.release_url, release_notes: data.prerelease.release_notes })
          foundAny = true
        }
        if (data.revert_stable) {
          setRevertStable({ version: data.revert_stable.version, release_url: data.revert_stable.release_url, release_notes: data.revert_stable.release_notes })
          foundAny = true
        }
        if (!foundAny) {
          setUpdateStatus("done")
          setUpdateMessage(`You're up to date (${data.current_version || version})`)
          setTimeout(() => { setUpdateStatus("idle"); setUpdateMessage(null) }, 4000)
        }
      }
    } catch (err) {
      setUpdateError(err instanceof Error ? err.message : "Failed to check for updates")
    } finally {
      setIsCheckingUpdate(false)
    }
  }

  async function handleInstallUpdate(targetVersion?: string) {
    setUpdateStatus("checking_internet")
    setUpdateError(null)
    setUpdateMessage("Checking internet connection...")

    const unsubscribe = wsClient.subscribe("update_status", (data: unknown) => {
      const msg = data as { status?: string; message?: string; error?: string }
      if (msg.error) {
        setUpdateStatus("error")
        setUpdateError(msg.error)
        setUpdateMessage(null)
        return
      }
      if (msg.status) {
        const statusMap: Record<string, UpdateStatus> = {
          checking_internet: "checking_internet",
          checking: "checking",
          remounting: "installing",
          downloading: "downloading",
          installing: "installing",
          updating_scripts: "updating_scripts",
          restarting: "restarting",
        }
        setUpdateStatus(statusMap[msg.status] || "installing")
      }
      if (msg.message) {
        setUpdateMessage(msg.message)
      }
    })

    try {
      const checkRes = await fetch("/api/system/check-internet")
      const checkData = await checkRes.json()
      if (!checkData.connected) {
        setUpdateStatus("error")
        setUpdateError("No internet connection. Connect to WiFi first.")
        setUpdateMessage(null)
        unsubscribe()
        return
      }

      const res = await fetch("/api/system/update", {
        method: "POST",
        headers: targetVersion ? { "Content-Type": "application/json" } : {},
        body: targetVersion ? JSON.stringify({ version: targetVersion }) : undefined,
      })
      if (!res.ok) throw new Error("Failed to start update")

      let reconnected = false
      setTimeout(() => {
        unsubscribe()
        setUpdateStatus("reconnecting")
        setUpdateMessage("Waiting for device to come back online...")

        const pollInterval = setInterval(async () => {
          try {
            const r = await fetch("/api/system/version")
            if (r.ok) {
              const data = await r.json()
              reconnected = true
              clearInterval(pollInterval)
              setStableUpdate(null)
              setPrereleaseUpdate(null)
              setRevertStable(null)
              setUpdateStatus("done")
              setUpdateMessage(`Update complete — now running ${data.version || "latest"}`)
              setVersion(data.version || "unknown")
              setTimeout(() => { setUpdateStatus("idle"); setUpdateMessage(null) }, 6000)
            }
          } catch {
            // Still restarting, keep polling
          }
        }, 3000)
        setTimeout(() => {
          if (!reconnected) {
            clearInterval(pollInterval)
            setUpdateStatus("idle")
            setUpdateMessage(null)
            setUpdateError("Update may still be in progress. Refresh the page in a moment.")
          }
        }, 180000)
      }, 20000)
    } catch (err) {
      unsubscribe()
      setUpdateStatus("error")
      setUpdateError(err instanceof Error ? err.message : "Update failed")
      setUpdateMessage(null)
      setRevertStable(null)
    }
  }

  function handleReboot() {
    if (!confirmReboot) {
      setConfirmReboot(true)
      setTimeout(() => setConfirmReboot(false), 10000)
      return "confirm"
    }
    fetch("/api/system/reboot", { method: "POST" })
    setConfirmReboot(false)
    return "Rebooting..."
  }

  return (
    <div className="space-y-4">
      {/* Header */}
      <div>
        <h1 className="text-2xl font-bold text-slate-100">Settings</h1>
        <p className="mt-1 text-sm text-slate-500">
          Configure and manage your Sentry USB device
        </p>
      </div>

      {/* Setup Wizard CTA */}
      <div className="glass-card overflow-hidden">
        <div className="flex flex-col gap-3 p-3 sm:flex-row sm:items-center">
          <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-xl bg-blue-500/20">
            <Wand2 className="h-4 w-4 text-blue-400" />
          </div>
          <div className="flex-1">
            <h2 className="text-sm font-semibold text-slate-100">
              Setup Wizard
            </h2>
            <p className="mt-0.5 text-xs text-slate-400">
              Configure WiFi, archive, notifications, and more.
            </p>
          </div>
          <div className="flex shrink-0 flex-wrap gap-2">
            <button
              onClick={async () => {
                try {
                  const res = await fetch("/api/setup/config")
                  if (!res.ok) throw new Error("Failed")
                  const data = await res.json()
                  let content = "# sentryusb.conf - exported from Sentry USB UI\n"
                  for (const [k, v] of Object.entries(data)) {
                    const entry = v as { value: string; active: boolean }
                    if (entry.active) {
                      content += `export ${k}='${entry.value}'\n`
                    } else {
                      content += `# export ${k}='${entry.value}'\n`
                    }
                  }
                  const blob = new Blob([content], { type: "text/plain" })
                  const url = URL.createObjectURL(blob)
                  const a = document.createElement("a")
                  a.href = url
                  a.download = "sentryusb.conf"
                  a.click()
                  URL.revokeObjectURL(url)
                } catch { /* ignore */ }
              }}
              className="shrink-0 rounded-xl border border-white/10 bg-white/5 px-3 py-2 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10"
            >
              <Download className="mr-1.5 inline h-3.5 w-3.5" />
              Export Config
            </button>
            <button
              onClick={async () => {
                try {
                  const res = await fetch("/api/setup/config")
                  if (res.ok) {
                    const data = await res.json()
                    const flat: Record<string, string> = {}
                    for (const [k, v] of Object.entries(data)) {
                      const entry = v as { value: string; active: boolean }
                      if (entry.active) flat[k] = entry.value
                    }
                    setWizardInitialData(flat)
                  }
                } catch { /* use empty data */ }
                setWizardOpen(true)
              }}
              className="shrink-0 rounded-xl bg-blue-500 px-3 py-2 text-xs font-medium text-white transition-colors hover:bg-blue-600"
            >
              Open Wizard
            </button>
          </div>
        </div>
      </div>

      {/* Quick Actions — 3x2 */}
      <div>
        <p className="section-label mb-2 px-1">Quick Actions</p>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
          <UsbDriveToggle />
          <ActionButton
            icon={RefreshCw}
            label="Archive Sync"
            description="Start archiving clips now"
            successMessage="Archive sync started"
            onClick={async () => {
              const res = await fetch("/api/system/trigger-sync", { method: "POST" })
              if (!res.ok) throw new Error("Failed to trigger sync")
            }}
          />
          <SpeedTestButton />
          <HealthCheckButton />
          <ActionButton
            icon={RotateCcw}
            label={confirmReboot ? "Confirm Reboot" : "Restart Pi"}
            description={confirmReboot ? "Press again to reboot" : "Reboot the device"}
            variant={confirmReboot ? "danger" : "default"}
            onClick={handleReboot}
          />
          <ActionButton
            icon={SettingsIcon}
            label="Raw Config"
            description="View/edit config file"
            onClick={async () => {
              const res = await fetch("/api/setup/config")
              if (!res.ok) throw new Error("Failed to load config")
              const data = await res.json()
              setRawConfig(data)
              setRawConfigOpen(true)
              return "confirm"
            }}
          />
        </div>
      </div>

      {/* Preferences */}
      <div>
        <p className="section-label mb-2 px-1">Preferences</p>
        <div className="grid grid-cols-1 gap-2 lg:grid-cols-3">
          <KeepAwakePreference />
          <AwayModeControl />
          <ConfigBackupSection />
          <CommunityFeaturesSection
            onOpenWizardForWraps={async () => {
              try {
                const res = await fetch("/api/setup/config")
                if (res.ok) {
                  const data = await res.json()
                  const flat: Record<string, string> = {}
                  for (const [k, v] of Object.entries(data)) {
                    const entry = v as { value: string; active: boolean }
                    if (entry.active) flat[k] = entry.value
                  }
                  flat._community_wraps_enabled = "true"
                  setWizardInitialData(flat)
                } else {
                  setWizardInitialData({ _community_wraps_enabled: "true" })
                }
              } catch {
                setWizardInitialData({ _community_wraps_enabled: "true" })
              }
              setWizardOpen(true)
            }}
          />
        </div>
      </div>

      {/* Connections & Updates — side by side */}
      <div>
        <p className="section-label mb-2 px-1">Connections & Updates</p>
        <div className="grid grid-cols-1 gap-2 lg:grid-cols-2">
          <div className="space-y-2">
            <MobileNotificationsSection />
            {piConfig?.uses_ble === "yes" && <BlePairButton />}
          </div>

          {/* Update tile — version + check + banners merged */}
          <div className="glass-card overflow-hidden">
            <div className="flex items-center gap-3 border-b border-white/5 px-3 py-2.5">
              <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-emerald-500/20">
                {updateStatus === "error" ? (
                  <AlertCircle className="h-4 w-4 text-red-400" />
                ) : updateStatus === "done" ? (
                  <CheckCircle className="h-4 w-4 text-emerald-400" />
                ) : updateStatus !== "idle" ? (
                  <Loader2 className="h-4 w-4 animate-spin text-blue-400" />
                ) : (
                  <Download className="h-4 w-4 text-emerald-400" />
                )}
              </div>
              <div className="flex-1 min-w-0">
                <div className="flex items-center gap-2">
                  <h3 className="text-sm font-semibold text-slate-200">Sentry USB</h3>
                  <span className="font-mono text-[11px] text-slate-500">{version || "..."}</span>
                </div>
                <p className="mt-0.5 text-xs text-slate-400 truncate">
                  {updateStatus === "idle" && !updateError && "Check for and install the latest version"}
                  {updateStatus === "idle" && updateError && <span className="text-red-400">{updateError}</span>}
                  {updateStatus === "error" && <span className="text-red-400">{updateError || "Update failed."}</span>}
                  {updateStatus === "done" && <span className="text-emerald-400">{updateMessage || "Update complete!"}</span>}
                  {updateStatus !== "idle" && updateStatus !== "error" && updateStatus !== "done" && (
                    updateMessage || "Working..."
                  )}
                </p>
              </div>
              <button
                onClick={() => handleCheckForUpdate()}
                disabled={isCheckingUpdate || (updateStatus !== "idle" && updateStatus !== "error" && updateStatus !== "done")}
                className="shrink-0 rounded-lg bg-emerald-500 px-3 py-2 text-xs font-medium text-white transition-colors hover:bg-emerald-600 disabled:opacity-50"
              >
                {isCheckingUpdate ? (
                  <span className="flex items-center gap-1.5">
                    <Loader2 className="h-3.5 w-3.5 animate-spin" /> Checking
                  </span>
                ) : "Check for Updates"}
              </button>
            </div>

            {/* Available update banners */}
            {stableUpdate && updateStatus === "idle" && (
              <div className="border-b border-white/5 bg-emerald-500/5 px-3 py-2.5">
                <div className="flex items-center justify-between gap-2">
                  <div className="min-w-0">
                    <p className="text-xs font-semibold text-emerald-300">Stable: {stableUpdate.version}</p>
                    <p className="mt-0.5 text-[11px] text-slate-400">
                      Updates server, scripts & BLE daemon.{" "}
                      <a href={stableUpdate.release_url} target="_blank" rel="noopener noreferrer"
                        className="text-blue-400 hover:text-blue-300 underline">Notes</a>
                    </p>
                  </div>
                  <button
                    onClick={() => handleInstallUpdate(stableUpdate.version)}
                    className="shrink-0 rounded-lg bg-emerald-500 px-3 py-1.5 text-[11px] font-medium text-white hover:bg-emerald-600"
                  >
                    Install
                  </button>
                </div>
              </div>
            )}

            {prereleaseUpdate && updateStatus === "idle" && (
              <div className="border-b border-white/5 bg-amber-500/5 px-3 py-2.5">
                <div className="flex items-center justify-between gap-2">
                  <div className="min-w-0">
                    <p className="text-xs font-semibold text-amber-300">Pre-release: {prereleaseUpdate.version}</p>
                    <p className="mt-0.5 text-[11px] text-slate-400">
                      Test build — may contain bugs.{" "}
                      <a href={prereleaseUpdate.release_url} target="_blank" rel="noopener noreferrer"
                        className="text-blue-400 hover:text-blue-300 underline">Notes</a>
                    </p>
                  </div>
                  <button
                    onClick={() => handleInstallUpdate(prereleaseUpdate.version)}
                    className="shrink-0 rounded-lg bg-amber-500 px-3 py-1.5 text-[11px] font-medium text-white hover:bg-amber-600"
                  >
                    Install
                  </button>
                </div>
              </div>
            )}

            {revertStable && updateStatus === "idle" && (
              <div className="border-b border-white/5 bg-blue-500/5 px-3 py-2.5">
                <div className="flex items-center justify-between gap-2">
                  <div className="min-w-0">
                    <p className="text-xs font-semibold text-blue-300">Revert to Stable: {revertStable.version}</p>
                    <p className="mt-0.5 text-[11px] text-slate-400">
                      Downgrade from pre-release to latest stable.{" "}
                      <a href={revertStable.release_url} target="_blank" rel="noopener noreferrer"
                        className="text-blue-400 hover:text-blue-300 underline">Notes</a>
                    </p>
                  </div>
                  <button
                    onClick={() => handleInstallUpdate(revertStable.version)}
                    className="shrink-0 rounded-lg bg-blue-500 px-3 py-1.5 text-[11px] font-medium text-white hover:bg-blue-600"
                  >
                    Revert
                  </button>
                </div>
              </div>
            )}

            {/* Update preferences */}
            <div className="px-3 py-2">
              <label className="flex cursor-pointer items-center justify-between">
                <span className="text-xs text-slate-400">Auto-check after each archive</span>
                <input
                  type="checkbox"
                  checked={autoUpdateEnabled}
                  onChange={async (e) => {
                    const enabled = e.target.checked
                    setAutoUpdateEnabled(enabled)
                    await fetch("/api/config/preference", {
                      method: "PUT",
                      headers: { "Content-Type": "application/json" },
                      body: JSON.stringify({ key: "auto_update_check", value: enabled ? "enabled" : "disabled" }),
                    }).catch(() => { })
                  }}
                  className="toggle-switch"
                />
              </label>
            </div>
            <div className="border-t border-white/5 px-3 py-2">
              <label className="flex cursor-pointer items-center justify-between gap-3">
                <div>
                  <span className="text-xs text-slate-400">Include pre-releases</span>
                  <span className="block text-[10px] text-slate-600 mt-0.5">Test builds may contain bugs</span>
                </div>
                <input
                  type="checkbox"
                  checked={includePrerelease}
                  onChange={async (e) => {
                    const enabled = e.target.checked
                    setIncludePrerelease(enabled)
                    await fetch("/api/config/preference", {
                      method: "PUT",
                      headers: { "Content-Type": "application/json" },
                      body: JSON.stringify({ key: "update_channel", value: enabled ? "prerelease" : "stable" }),
                    }).catch(() => { })
                  }}
                  className="toggle-switch"
                />
              </label>
            </div>
            {/* Links footer */}
            <div className="border-t border-white/5 px-3 py-2 flex items-center gap-3">
              <a href="https://github.com/Scottmg1/Sentry-USB" target="_blank" rel="noopener noreferrer"
                className="text-xs text-blue-400 hover:text-blue-300">GitHub</a>
              <a
                href="https://discord.gg/9QZEzVwdnt"
                target="_blank"
                rel="noopener noreferrer"
                className="inline-flex items-center gap-1 text-xs text-[#5865F2] hover:text-[#7289DA]"
              >
                <svg className="h-3.5 w-3.5" viewBox="0 0 24 24" fill="currentColor">
                  <path d="M20.317 4.37a19.791 19.791 0 0 0-4.885-1.515.074.074 0 0 0-.079.037c-.21.375-.444.864-.608 1.25a18.27 18.27 0 0 0-5.487 0 12.64 12.64 0 0 0-.617-1.25.077.077 0 0 0-.079-.037A19.736 19.736 0 0 0 3.677 4.37a.07.07 0 0 0-.032.027C.533 9.046-.32 13.58.099 18.057a.082.082 0 0 0 .031.057 19.9 19.9 0 0 0 5.993 3.03.078.078 0 0 0 .084-.028c.462-.63.874-1.295 1.226-1.994a.076.076 0 0 0-.041-.106 13.107 13.107 0 0 1-1.872-.892.077.077 0 0 1-.008-.128 10.2 10.2 0 0 0 .372-.292.074.074 0 0 1 .077-.01c3.928 1.793 8.18 1.793 12.062 0a.074.074 0 0 1 .078.01c.12.098.246.198.373.292a.077.077 0 0 1-.006.127 12.299 12.299 0 0 1-1.873.892.077.077 0 0 0-.041.107c.36.698.772 1.362 1.225 1.993a.076.076 0 0 0 .084.028 19.839 19.839 0 0 0 6.002-3.03.077.077 0 0 0 .032-.054c.5-5.177-.838-9.674-3.549-13.66a.061.061 0 0 0-.031-.03zM8.02 15.33c-1.183 0-2.157-1.085-2.157-2.419 0-1.333.956-2.419 2.157-2.419 1.21 0 2.176 1.096 2.157 2.42 0 1.333-.956 2.418-2.157 2.418zm7.975 0c-1.183 0-2.157-1.085-2.157-2.419 0-1.333.955-2.419 2.157-2.419 1.21 0 2.176 1.096 2.157 2.42 0 1.333-.946 2.418-2.157 2.418z" />
                </svg>
                Discord
              </a>
            </div>
          </div>
        </div>
      </div>

      {/* Setup Wizard Modal */}
      {wizardOpen && (
        <SetupWizard initialData={wizardInitialData} onClose={() => { setWizardOpen(false); setWizardInitialData(undefined) }} />
      )}

      {/* Raw Config Editor Modal */}
      {rawConfigOpen && rawConfig && (
        <RawConfigEditor config={rawConfig} onClose={() => { setRawConfigOpen(false); setRawConfig(null) }} />
      )}
    </div>
  )
}
