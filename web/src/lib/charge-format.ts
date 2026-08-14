// Display formatters for charge-session values. Each tolerates
// null/undefined and renders an em dash so the UI never shows "NaN" or
// "null" when a sample column was empty.

export function fmtEnergy(kwh: number | null | undefined): string {
  if (kwh == null) return "—"
  return `${kwh.toFixed(1)} kWh`
}

export function fmtSoc(pct: number | null | undefined): string {
  if (pct == null) return "—"
  return `${Math.round(pct)}%`
}

export function fmtPower(kw: number | null | undefined): string {
  if (kw == null) return "—"
  return `${Math.round(kw)} kW`
}

export function fmtCurrent(amps: number | null | undefined): string {
  if (amps == null) return "—"
  return `${Math.round(amps)} A`
}

export function fmtVoltage(volts: number | null | undefined): string {
  if (volts == null) return "—"
  return `${Math.round(volts)} V`
}

// Charging rate in the configured distance unit per hour. Input is mph
// because Tesla reports miles regardless of the dashboard preference.
export function fmtChargeRateUnit(
  mph: number | null | undefined,
  metric: boolean,
): string {
  if (mph == null) return "—"
  return metric
    ? `${Math.round(mph * 1.609344)} km/h`
    : `${Math.round(mph)} mph`
}

// Range in the configured distance unit. Input is miles (the car's
// native unit); metric converts to km.
export function fmtRangeUnit(
  mi: number | null | undefined,
  metric: boolean,
): string {
  if (mi == null) return "—"
  return metric ? `${Math.round(mi * 1.609344)} km` : `${Math.round(mi)} mi`
}

// "~3h 45m to full" / "~45m to full". null/0 → null.
export function fmtToFull(mins: number | null | undefined): string | null {
  if (mins == null || mins <= 0) return null
  const h = Math.floor(mins / 60)
  const m = mins % 60
  return h > 0 ? `~${h}h ${m}m to full` : `~${m}m to full`
}

export function fmtMoney(
  amount: number | null | undefined,
  currency: string | null | undefined,
): string {
  if (amount == null) return "—"
  const sym = currency || "$"
  return `${sym}${amount.toFixed(2)}`
}

export function fmtPercent(pct: number | null | undefined): string {
  if (pct == null) return "—"
  return `${Math.round(pct)}%`
}

export function fmtDuration(secs: number | null | undefined): string {
  if (secs == null || secs <= 0) return "—"
  const h = Math.floor(secs / 3600)
  const m = Math.floor((secs % 3600) / 60)
  if (h > 0) return `${h}h ${m}m`
  if (m > 0) return `${m}m`
  return `${secs}s`
}
