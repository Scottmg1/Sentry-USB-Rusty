import { useEffect, useState } from "react"
import { Battery, BatteryCharging } from "lucide-react"
import { fetchCurrentCharge } from "@/api/charging"
import type { CurrentCharge } from "@/types/charging"

const POLL_MS = 30_000

/** "3h 45m to full" / "45m to full" — null/0 renders nothing. */
function fmtToFull(mins: number | null): string | null {
  if (mins == null || mins <= 0) return null
  const h = Math.floor(mins / 60)
  const m = mins % 60
  if (h > 0) return `~${h}h ${m}m to full`
  return `~${m}m to full`
}

/**
 * Persistent dashboard car-status banner. Polls /api/charging/current and
 * stays visible whenever there's recent battery data: a green "charging"
 * strip with time-to-full while charging, and a subdued battery readout
 * when idle (so it doesn't vanish the moment a charge ends). Renders
 * nothing only when there's no recent telemetry at all.
 */
export default function ChargingBanner() {
  const [cur, setCur] = useState<CurrentCharge | null>(null)

  useEffect(() => {
    let alive = true
    const tick = () =>
      fetchCurrentCharge()
        .then((c) => alive && setCur(c))
        .catch(() => alive && setCur(null))
    tick()
    const id = setInterval(tick, POLL_MS)
    return () => {
      alive = false
      clearInterval(id)
    }
  }, [])

  // No recent battery data → nothing to show.
  if (!cur || cur.soc == null) return null

  const soc = Math.round(cur.soc)

  if (cur.charging) {
    const parts: string[] = [
      cur.limitSoc != null ? `${soc}% → ${cur.limitSoc}%` : `${soc}%`,
    ]
    if (cur.powerKw != null) parts.push(`${cur.powerKw} kW`)
    const toFull = fmtToFull(cur.minutesToFull)
    if (toFull) parts.push(toFull)

    return (
      <div className="mb-4 flex flex-wrap items-center gap-x-2 gap-y-0.5 rounded-xl border border-emerald-500/20 bg-emerald-500/10 px-4 py-2.5 text-sm text-emerald-300 tabular-nums">
        <BatteryCharging className="h-4 w-4 shrink-0 animate-pulse" />
        <span className="font-medium text-emerald-200">Charging</span>
        {parts.map((p, i) => (
          <span key={i} className="flex items-center gap-2">
            <span className="text-emerald-400/50">·</span>
            {p}
          </span>
        ))}
      </div>
    )
  }

  // Idle — persistent battery readout so the banner stays put.
  return (
    <div className="mb-4 flex flex-wrap items-center gap-x-2 gap-y-0.5 rounded-xl border border-white/10 bg-slate-800/40 px-4 py-2.5 text-sm text-slate-300 tabular-nums">
      <Battery className="h-4 w-4 shrink-0 text-slate-400" />
      <span className="font-medium text-slate-200">{soc}%</span>
      {cur.rangeMi != null && (
        <span className="flex items-center gap-2">
          <span className="text-slate-500">·</span>
          {Math.round(cur.rangeMi)} mi
        </span>
      )}
      <span className="text-slate-500">·</span>
      <span className="text-slate-400">Not charging</span>
    </div>
  )
}
