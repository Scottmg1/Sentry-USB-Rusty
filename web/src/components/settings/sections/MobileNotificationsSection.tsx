import { useState, useEffect } from "react"
import { NotificationsIcon, ProgressActivityIcon } from "@/components/icons"
import { cn } from "@/lib/utils"
import { PrefCard } from "@/components/settings/PrefCard"

type PairedDevice = {
  id?: string
  pairing_id?: string
  device_name: string
  platform: string
  paired_at: string
}
const devicePairingId = (d: PairedDevice) => d.id ?? d.pairing_id ?? ""

export function MobileNotificationsSection() {
  const [pairingCode, setPairingCode] = useState<string | null>(null)
  const [expiresAt, setExpiresAt] = useState<string | null>(null)
  const [pairedDevices, setPairedDevices] = useState<PairedDevice[]>([])
  const [loading, setLoading] = useState(false)
  const [devicesLoading, setDevicesLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [countdown, setCountdown] = useState(0)
  const [testState, setTestState] = useState<"idle" | "loading" | "success" | "error">("idle")

  async function loadPairedDevices() {
    try {
      const res = await fetch("/api/notifications/paired-devices")
      if (res.ok) {
        const data = await res.json()
        setPairedDevices(data.devices || [])
      }
    } catch {
      /* ignore */
    }
    setDevicesLoading(false)
  }

  useEffect(() => {
    loadPairedDevices()
  }, [])

  useEffect(() => {
    if (!expiresAt) return
    const interval = setInterval(() => {
      const remaining = Math.max(
        0,
        Math.floor((new Date(expiresAt).getTime() - Date.now()) / 1000)
      )
      setCountdown(remaining)
      if (remaining <= 0) {
        setPairingCode(null)
        setExpiresAt(null)
      }
    }, 1000)
    return () => clearInterval(interval)
  }, [expiresAt])

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
      const res = await fetch(`/api/notifications/paired-devices/${pairingId}`, {
        method: "DELETE",
      })
      if (res.ok) {
        setPairedDevices((prev) =>
          prev.filter((d) => devicePairingId(d) !== pairingId)
        )
      }
    } catch {
      /* ignore */
    }
  }

  async function sendTest() {
    setTestState("loading")
    try {
      const res = await fetch("/api/notifications/test", { method: "POST" })
      setTestState(res.ok ? "success" : "error")
    } catch {
      setTestState("error")
    }
    setTimeout(() => setTestState("idle"), 3000)
  }

  return (
    <PrefCard icon={<NotificationsIcon className="h-3.5 w-3.5" />} halo="violet" title="Mobile Notifications">
      <div className="flex items-center gap-3">
        {pairingCode ? (
          <div className="flex items-center gap-4">
            <span className="font-mono text-xl font-bold tracking-widest text-blue-400">
              {pairingCode}
            </span>
            <span className="text-xs text-slate-500">
              Expires in {Math.floor(countdown / 60)}:
              {String(countdown % 60).padStart(2, "0")}
            </span>
          </div>
        ) : (
          <button
            onClick={generateCode}
            disabled={loading}
            className="rounded-lg bg-blue-500 px-3 py-2 text-xs font-medium text-white transition-colors hover:bg-blue-600 disabled:opacity-50"
          >
            {loading && <ProgressActivityIcon className="mr-1 inline h-3.5 w-3.5 animate-spin" />}
            Generate Code
          </button>
        )}
      </div>

      {pairingCode && (
        <p className="text-xs text-slate-600">
          Enter this code in the Sentry USB mobile app under Settings → Pair for Notifications.
        </p>
      )}

      {error && <p className="text-xs text-red-400">{error}</p>}

      {devicesLoading ? (
        <p className="text-xs text-slate-600">Loading paired devices...</p>
      ) : pairedDevices.length > 0 ? (
        <div className="space-y-2">
          <p className="section-label">Paired Devices</p>
          {pairedDevices.map((device) => (
            <div
              key={devicePairingId(device)}
              className="flex items-center gap-3 rounded-xl border border-white/5 bg-white/[0.02] px-3 py-2.5"
            >
              <span className="text-sm text-slate-300">{device.device_name}</span>
              <span className="rounded-md bg-white/5 px-1.5 py-0.5 text-[10px] font-medium text-slate-500">
                {device.platform.toUpperCase()}
              </span>
              <span className="flex-1" />
              <button
                onClick={() => removeDevice(devicePairingId(device))}
                className="text-xs text-red-400/60 transition-colors hover:text-red-400"
              >
                Remove
              </button>
            </div>
          ))}
          <button
            onClick={sendTest}
            disabled={testState === "loading"}
            className={cn(
              "mt-1 w-full rounded-xl border border-white/5 bg-white/[0.03] px-3 py-2.5 text-xs transition-colors disabled:opacity-50",
              testState === "success"
                ? "text-emerald-400"
                : testState === "error"
                ? "text-red-400"
                : "text-slate-400 hover:bg-white/[0.06] hover:text-slate-300"
            )}
          >
            {testState === "loading"
              ? "Sending..."
              : testState === "success"
              ? "✓ Test sent!"
              : testState === "error"
              ? "Failed to send"
              : "Send Test Notification"}
          </button>
        </div>
      ) : (
        <p className="text-xs text-slate-600">No mobile devices paired yet.</p>
      )}
    </PrefCard>
  )
}
