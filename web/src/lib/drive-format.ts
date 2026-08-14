export function formatDuration(ms: number): string {
  const totalMin = Math.max(0, Math.floor(ms / 60000))
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  if (h === 0) return `${m}m`
  return `${h}h ${m}m`
}

// BLE sample windows can outlive the drive; clamp HVAC time to its duration.
export function formatHvacRuntime(seconds: number, drivenMs?: number): string {
  let secs = Math.max(0, seconds)
  if (typeof drivenMs === "number" && drivenMs > 0) {
    secs = Math.min(secs, drivenMs / 1000)
  }
  const totalMin = Math.max(0, Math.round(secs / 60))
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  if (h === 0) return `${m}m`
  return `${h}h ${m}m`
}

export function formatDistance(mi: number, km: number, metric: boolean): string {
  const value = metric ? km : mi
  const unit = metric ? "km" : "mi"
  return `${value.toLocaleString(undefined, {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  })} ${unit}`
}

/** Format miles with grouping and the requested precision. */
export function formatMiles(mi: number, decimals = 1): string {
  return `${mi.toLocaleString(undefined, {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  })} mi`
}

/** Format a mile-based odometer in the preferred distance unit. */
export function formatOdometer(mi: number, metric: boolean, decimals = 1): string {
  const value = metric ? mi * 1.609344 : mi
  const unit = metric ? "km" : "mi"
  return `${value.toLocaleString(undefined, {
    minimumFractionDigits: decimals,
    maximumFractionDigits: decimals,
  })} ${unit}`
}

export function formatSpeed(mph: number, kmh: number, metric: boolean): string {
  const value = Math.round(metric ? kmh : mph)
  const unit = metric ? "km/h" : "mph"
  return `${value} ${unit}`
}

export function formatTempC(c: number | undefined, metric: boolean): string {
  if (c === undefined) return "—"
  if (metric) return `${Math.round(c)}°C`
  return `${Math.round((c * 9) / 5 + 32)}°F`
}

export function formatRelativeTime(iso: string, now: Date = new Date()): string {
  const t = new Date(iso)
  if (Number.isNaN(t.getTime())) return iso

  const sameDay = t.toDateString() === now.toDateString()
  const yesterday = new Date(now)
  yesterday.setDate(now.getDate() - 1)
  const isYesterday = t.toDateString() === yesterday.toDateString()

  const time = t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
  if (sameDay) return `Today ${time}`
  if (isYesterday) return `Yesterday ${time}`

  const diffMs = now.getTime() - t.getTime()
  const days = Math.floor(diffMs / (1000 * 60 * 60 * 24))
  if (days >= 0 && days < 7) {
    return `${t.toLocaleDateString([], { weekday: "long" })} ${time}`
  }
  return `${t.toLocaleDateString([], { month: "short", day: "numeric" })} ${time}`
}

// Tesla reports PSI; use the shared conversion so TPMS displays agree.
const PSI_TO_BAR = 0.0689476

export function formatPsi(psi: number | undefined, bar: boolean): string {
  if (psi === undefined) return "—"
  if (bar) return `${(psi * PSI_TO_BAR).toFixed(2)} bar`
  return `${psi.toFixed(1)} psi`
}

/** Format a percentage to at most two decimal places without trailing zeros. */
export function formatPercent(n: number): string {
  if (!Number.isFinite(n)) return "0"
  return parseFloat(n.toFixed(2)).toString()
}
