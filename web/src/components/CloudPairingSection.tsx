import { useEffect, useMemo, useRef, useState } from "react"
import { Cloud, CloudOff, Loader2, Trash2, Upload } from "lucide-react"
import { cn } from "@/lib/utils"
import { wsClient } from "@/lib/ws"

type CloudPairingState = "idle" | "handshaking" | "polling" | "complete" | "error"

type CloudStatus = {
  paired: boolean
  userId: string | null
  piId: string | null
  pairedAt: string | null
  lastUploadAt: string | null
  lastUploadError: string | null
  pendingRouteCount: number
  totalUploadedRouteCount: number
  dekRotationGeneration: number | null
  cloudBaseUrl: string
  pairingState: CloudPairingState
  pairingError: string | null
}

type Props = {
  compact?: boolean
}

export default function CloudPairingSection({ compact = false }: Props) {
  const [status, setStatus] = useState<CloudStatus | null>(null)
  const [code, setCode] = useState("")
  const [submitting, setSubmitting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [confirmUnpair, setConfirmUnpair] = useState(false)

  const sessionStartRef = useRef<{ pending: number; uploaded: number } | null>(null)

  useEffect(() => {
    let mounted = true
    let timer: ReturnType<typeof setTimeout> | null = null

    async function refetch() {
      try {
        const res = await fetch("/api/cloud/status")
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
        const data: CloudStatus = await res.json()
        if (!mounted) return
        if (data.paired && !sessionStartRef.current) {
          sessionStartRef.current = {
            pending: data.pendingRouteCount + data.totalUploadedRouteCount,
            uploaded: data.totalUploadedRouteCount,
          }
        }
        if (!data.paired) sessionStartRef.current = null
        setStatus(data)
        scheduleNext(data)
      } catch {
        if (mounted) {
          if (timer) clearTimeout(timer)
          timer = setTimeout(refetch, 5000)
        }
      }
    }

    function scheduleNext(data: CloudStatus | null) {
      if (timer) clearTimeout(timer)
      const fast =
        data?.pairingState === "handshaking" ||
        data?.pairingState === "polling" ||
        (data?.paired && data.pendingRouteCount > 0)
      timer = setTimeout(refetch, fast ? 1000 : 30000)
    }

    refetch()

    const unsubStatus = wsClient.subscribe("cloud_status_changed", () => {
      if (mounted) refetch()
    })
    const unsubUpload = wsClient.subscribe("cloud_upload", () => {
      if (mounted) refetch()
    })

    return () => {
      mounted = false
      if (timer) clearTimeout(timer)
      unsubStatus()
      unsubUpload()
    }
  }, [])

  async function startPairing() {
    if (submitting || !/^\d{6}$/.test(code)) return
    setSubmitting(true)
    setError(null)
    try {
      const res = await fetch("/api/cloud/pair/begin", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ code }),
      })
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        throw new Error(data.error || `HTTP ${res.status}`)
      }
      setCode("")
    } catch (err) {
      setError(err instanceof Error ? err.message : "pairing failed")
    } finally {
      setSubmitting(false)
    }
  }

  async function cancelPairing() {
    try {
      await fetch("/api/cloud/pair/cancel", { method: "POST" })
    } catch {}
  }

  async function unpair() {
    try {
      const res = await fetch("/api/cloud/unpair", { method: "POST" })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      setConfirmUnpair(false)
    } catch (err) {
      setError(err instanceof Error ? err.message : "unpair failed")
    }
  }

  const paired = status?.paired ?? false
  const pairingState = status?.pairingState ?? "idle"
  const inFlight =
    pairingState === "handshaking" || pairingState === "polling"
  const lastUploadDisplay = status?.lastUploadAt
    ? new Date(status.lastUploadAt).toLocaleString()
    : null

  const uploadProgress = useMemo(() => {
    if (!paired || !status) return null
    const session = sessionStartRef.current
    if (!session || session.pending === 0) return null
    const done = Math.max(0, status.totalUploadedRouteCount - session.uploaded)
    const remaining = status.pendingRouteCount
    const total = done + remaining
    if (total === 0) return null
    const pct = total > 0 ? Math.min(100, (done / total) * 100) : 0
    return { done, remaining, total, pct }
  }, [paired, status])

  return (
    <div className="glass-card overflow-hidden">
      <div className="flex items-center gap-3 border-b border-white/5 px-3 py-2">
        <div
          className={cn(
            "flex h-7 w-7 shrink-0 items-center justify-center rounded-lg",
            paired ? "bg-emerald-500/15" : "bg-sky-500/15",
          )}
        >
          {paired ? (
            <Cloud className="h-3.5 w-3.5 text-emerald-400" />
          ) : (
            <CloudOff className="h-3.5 w-3.5 text-sky-400" />
          )}
        </div>
        <h3 className="text-sm font-semibold text-slate-200">SentryCloud</h3>
        {paired && (
          <span className="ml-auto rounded-full bg-emerald-500/15 px-2 py-0.5 text-[10px] font-semibold text-emerald-400">
            PAIRED
          </span>
        )}
      </div>

      <div className="space-y-3 p-3">
        {!paired && !inFlight && (
          <>
            <p className="text-xs text-slate-400">
              Encrypted upload of your drive data to{" "}
              <span className="font-mono text-slate-300">sentryusb.com</span>.
              Pair with the 6-digit code from your account's Settings → Devices.
            </p>
            <div className="flex flex-col gap-2 sm:flex-row sm:items-stretch">
              <input
                type="text"
                inputMode="numeric"
                pattern="\d{6}"
                maxLength={6}
                value={code}
                onChange={(e) => setCode(e.target.value.replace(/\D/g, "").slice(0, 6))}
                onKeyDown={(e) => { if (e.key === "Enter" && code.length === 6) startPairing() }}
                placeholder="000000"
                aria-label="6-digit pairing code"
                className="font-mono w-full sm:w-44 rounded-lg border border-white/10 bg-slate-950/40 px-4 py-3 text-center text-2xl font-semibold tracking-[0.35em] text-slate-100 placeholder:text-slate-700 focus:border-sky-400/60 focus:outline-none focus:ring-2 focus:ring-sky-500/20 transition"
              />
              <button
                onClick={startPairing}
                disabled={submitting || code.length !== 6}
                className="flex items-center justify-center gap-2 rounded-lg bg-sky-500 px-5 py-3 text-sm font-semibold text-white shadow-sm transition-colors hover:bg-sky-400 disabled:cursor-not-allowed disabled:opacity-40"
              >
                {submitting ? <Loader2 className="h-4 w-4 animate-spin" /> : <Cloud className="h-4 w-4" />}
                {submitting ? "Pairing…" : "Pair"}
              </button>
            </div>
            {error && (
              <p className="text-xs text-red-400">{error}</p>
            )}
          </>
        )}

        {!paired && inFlight && (
          <div className="flex items-center justify-between gap-3 rounded-lg border border-sky-500/20 bg-sky-500/5 px-3 py-2.5">
            <div className="flex items-center gap-2">
              <Loader2 className="h-4 w-4 animate-spin text-sky-400" />
              <p className="text-xs text-slate-300">
                {pairingState === "handshaking"
                  ? "Connecting to cloud…"
                  : "Waiting for browser to finish…"}
              </p>
            </div>
            <button
              onClick={cancelPairing}
              className="rounded-lg border border-white/10 bg-white/5 px-2.5 py-1 text-[11px] text-slate-300 hover:bg-white/10"
            >
              Cancel
            </button>
          </div>
        )}

        {!paired && pairingState === "error" && (
          <p className="text-xs text-red-400">
            {status?.pairingError || "Pairing failed."}
          </p>
        )}

        {paired && status && (
          <>
            {uploadProgress && (
              <div className="rounded-lg border border-sky-500/20 bg-sky-500/5 px-3 py-2.5">
                <div className="mb-2 flex items-center justify-between gap-2 text-[11px]">
                  <div className="flex items-center gap-1.5 text-sky-300">
                    <Upload className="h-3.5 w-3.5 animate-pulse" />
                    <span className="font-medium">Uploading</span>
                  </div>
                  <span className="text-slate-400">
                    {uploadProgress.done.toLocaleString()} / {uploadProgress.total.toLocaleString()} routes
                  </span>
                </div>
                <div className="h-1.5 overflow-hidden rounded-full bg-slate-900/60">
                  <div
                    className="h-full rounded-full bg-sky-400 transition-[width] duration-300"
                    style={{ width: `${uploadProgress.pct}%` }}
                  />
                </div>
              </div>
            )}

            <div className={cn(
              "grid gap-2 text-[11px]",
              compact ? "grid-cols-2" : "grid-cols-2 sm:grid-cols-4",
            )}>
              <Stat label="Pending" value={status.pendingRouteCount.toLocaleString()} />
              <Stat label="Uploaded" value={status.totalUploadedRouteCount.toLocaleString()} />
              <Stat label="Pi ID" value={status.piId?.slice(0, 8) ?? "—"} mono />
              <Stat label="Last upload" value={lastUploadDisplay ?? "—"} />
            </div>

            {status.lastUploadError && (
              <p className="text-[11px] text-red-400">
                Last error: <span className="font-mono">{status.lastUploadError}</span>
              </p>
            )}

            {confirmUnpair ? (
              <div className="flex flex-col gap-2 rounded-lg border border-red-500/30 bg-red-500/10 p-2.5">
                <p className="text-[11px] text-red-200">
                  Unpair this Pi? Local credentials are wiped. Re-pair from your account to resume uploads.
                </p>
                <div className="flex gap-2">
                  <button
                    onClick={unpair}
                    className="flex-1 rounded-md bg-red-500 px-3 py-1.5 text-[11px] font-semibold text-white hover:bg-red-600"
                  >
                    Unpair
                  </button>
                  <button
                    onClick={() => setConfirmUnpair(false)}
                    className="flex-1 rounded-md border border-white/10 bg-white/5 px-3 py-1.5 text-[11px] text-slate-300 hover:bg-white/10"
                  >
                    Keep paired
                  </button>
                </div>
              </div>
            ) : (
              <button
                onClick={() => setConfirmUnpair(true)}
                className="flex items-center gap-1.5 rounded-md border border-white/10 bg-white/5 px-2.5 py-1.5 text-[11px] font-medium text-slate-300 transition-colors hover:border-red-500/30 hover:bg-red-500/10 hover:text-red-300"
              >
                <Trash2 className="h-3 w-3" />
                Unpair this Pi
              </button>
            )}
            {error && (
              <p className="text-xs text-red-400">{error}</p>
            )}
          </>
        )}
      </div>
    </div>
  )
}

function Stat({ label, value, mono }: { label: string; value: string; mono?: boolean }) {
  return (
    <div className="rounded-md border border-white/5 bg-slate-900/30 px-2 py-1.5">
      <p className="text-[10px] uppercase tracking-wide text-slate-500">{label}</p>
      <p className={cn("truncate text-slate-200", mono && "font-mono")}>{value}</p>
    </div>
  )
}
