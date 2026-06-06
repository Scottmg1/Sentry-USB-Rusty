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
 * stays visible whenever there's recent battery data.
 *
 * Charging vs idle is conveyed by color + icon — green pulsing
 * `BatteryCharging` for active charging, subdued `Battery` when not.
 * Both states show `battery % · range`; the charging state additionally
 * shows the estimated time-to-full when the car reports it. No redundant
 * "Charging" / "Not charging" text — the color and icon already convey
 * that state. Same unit pattern as drive-detail: range comes from
 * `DRIVE_MAP_UNIT` in setup config (default imperial).
 */
export default function ChargingBanner() {
  const [cur, setCur] = useState<CurrentCharge | null>(null)
  // Distance unit, sourced from setup config (DRIVE_MAP_UNIT). Default
  // imperial — same default as the wizard and the other dashboard
  // surfaces, so the first paint never shows an unintended unit.
  const [metric, setMetric] = useState(false)

  useEffect(() => {
    let alive = true
    const tick = () =>
      fetchCurrentCharge()
        .then((c) => alive && setCur(c))
        // Keep the last good state on a transient fetch error (a single
        // 30s-poll timeout shouldn't collapse a live banner).
        .catch(() => {})
    tick()
    const id = setInterval(tick, POLL_MS)
    return () => {
      alive = false
      clearInterval(id)
    }
  }, [])

  useEffect(() => {
    let cancelled = false
    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        if (cancelled) return
        const entry = cfg?.DRIVE_MAP_UNIT
        if (!entry) return
        const val =
          typeof entry === "object" ? (entry.active ? entry.value : null) : entry
        if (val !== null && val !== undefined) setMetric(val === "km")
      })
      .catch(() => {
        /* non-critical — fall back to default unit */
      })
    return () => {
      cancelled = true
    }
  }, [])

  // No recent battery data → nothing to show.
  if (!cur || cur.soc == null) return null

  const soc = Math.round(cur.soc)
  const range =
    cur.rangeMi != null
      ? metric
        ? `${Math.round(cur.rangeMi * 1.609344)} km`
        : `${Math.round(cur.rangeMi)} mi`
      : null

  if (cur.charging) {
    // Active charging — green strip, pulsing icon. Color carries the
    // "this is charging" signal so the text doesn't have to. Time-to-full
    // is the one piece of info color can't convey, so it stays.
    const toFull = fmtToFull(cur.minutesToFull)
    return (
      <div className="mb-4 flex flex-wrap items-center gap-x-2 gap-y-0.5 rounded-xl border border-emerald-500/20 bg-emerald-500/10 px-4 py-2.5 text-sm text-emerald-300 tabular-nums">
        <BatteryCharging className="h-4 w-4 shrink-0 animate-pulse" />
        <span className="font-medium text-emerald-200">{soc}%</span>
        {range && (
          <span className="flex items-center gap-2">
            <span className="text-emerald-400/50">·</span>
            {range}
          </span>
        )}
        {toFull && (
          <span className="flex items-center gap-2">
            <span className="text-emerald-400/50">·</span>
            {toFull}
          </span>
        )}
      </div>
    )
  }

  // Idle — subdued strip. Same content; color + icon tell the story.
  return (
    <div className="mb-4 flex flex-wrap items-center gap-x-2 gap-y-0.5 rounded-xl border border-white/10 bg-slate-800/40 px-4 py-2.5 text-sm text-slate-300 tabular-nums">
      <Battery className="h-4 w-4 shrink-0 text-slate-400" />
      <span className="font-medium text-slate-200">{soc}%</span>
      {range && (
        <span className="flex items-center gap-2">
          <span className="text-slate-500">·</span>
          {range}
        </span>
      )}
    </div>
  )
}
