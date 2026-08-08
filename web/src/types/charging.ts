// Charge-session shapes returned by /api/charging and
// /api/charging/{id}. Field names are camelCase to match the backend's
// serde serialization (crates/api/src/charging.rs). All measured values
// are optional — any column can be NULL on a given sample.

export interface ChargeSessionSummary {
  // Session id == start timestamp in unix seconds. Also the detail key.
  id: number
  startMs: number
  endMs: number
  durationSecs: number
  location: string | null
  locationLat: number | null
  locationLon: number | null
  energyAddedKwh: number | null
  // Energy drawn from the charger (wall-side), kWh. >= energyAddedKwh;
  // the gap is charging loss. efficiencyPct = added / used.
  energyUsedKwh: number | null
  efficiencyPct: number | null
  peakPowerKw: number | null
  startSoc: number | null
  endSoc: number | null
  startRangeMi: number | null
  endRangeMi: number | null
  chargeLimitSoc: number | null
  // User-assigned tags + the cost the backend derived from them (charged
  // on energy used). `cost`/`rate` are null until a rate is configured.
  tags: string[]
  cost: number | null
  rate: number | null
  currency: string
  // True when peak power exceeds the AC Level 2 ceiling (>22 kW) — i.e. DC
  // fast charging. Drives the "Fast charging" badge and, in the detail
  // view, unlocks the manual per-charge cost.
  fastCharging: boolean
  // True when `cost` is a user-entered per-charge override rather than a
  // rate-derived value (so `rate` is null and the UI shows it as manual).
  costOverridden: boolean
  // True when the session charged inside the configured home geofence.
  // Derived (not a stored tag) — drives the auto "Home" chip + Home rate.
  atHome: boolean
}

export interface ChargePoint {
  ts: number // unix ms
  powerKw: number | null
  currentA: number | null
  voltageV: number | null
  rateMph: number | null
  soc: number | null
  rangeMi: number | null
  energyAddedKwh: number | null
}

export interface ChargeSessionDetail extends ChargeSessionSummary {
  avgPowerKw: number | null
  peakCurrentA: number | null
  avgCurrentA: number | null
  peakVoltageV: number | null
  avgVoltageV: number | null
  peakRateMph: number | null
  points: ChargePoint[]
}

// Live charge status for the dashboard banner (/api/charging/current).
// `charging` is false when the car isn't actively charging; the other
// fields are present only while charging.
export interface CurrentCharge {
  charging: boolean
  soc: number | null
  limitSoc: number | null
  powerKw: number | null
  minutesToFull: number | null
  rangeMi: number | null
}
