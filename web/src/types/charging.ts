// Charging API fields use serde's camelCase and may be null per sample.

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
  // Wall-side energy; its difference from energyAddedKwh is charging loss.
  energyUsedKwh: number | null
  efficiencyPct: number | null
  peakPowerKw: number | null
  startSoc: number | null
  endSoc: number | null
  startRangeMi: number | null
  endRangeMi: number | null
  chargeLimitSoc: number | null
  // Cost is based on wall-side energy and remains null without a rate.
  tags: string[]
  cost: number | null
  rate: number | null
  currency: string
  // Peak power above 22 kW identifies DC fast charging.
  fastCharging: boolean
  // Manual cost overrides have no derived rate.
  costOverridden: boolean
  // Derived from the home geofence rather than stored as a tag.
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

// Live metrics and controls require a fresh active-charging response.
export interface CurrentCharge {
  charging: boolean
  soc: number | null
  limitSoc: number | null
  powerKw: number | null
  currentA: number | null
  voltageV: number | null
  rateMph: number | null
  energyAddedKwh: number | null
  minutesToFull: number | null
  rangeMi: number | null
  chargingAmps: number | null
  maxChargingAmps: number | null
  chargePortOpen: boolean | null
  controlsAvailable: boolean
  controlsValidUntilTs: number | null
}
