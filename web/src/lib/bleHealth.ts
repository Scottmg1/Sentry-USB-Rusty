export type BleHealthSeverity = "green" | "yellow" | "red"

export type BleHealthCode =
  | "connected"
  | "paused_archiving"
  | "paused_keep_awake"
  | "reconnecting"
  | "delayed"
  | "idle"
  | "repair_required"

export interface BleHealth {
  severity: BleHealthSeverity
  code: BleHealthCode
  since_ts: number | null
  label: string
  guidance: string
}

export interface BleHealthPresentation extends BleHealth {
  repairRequired: boolean
}

const SEVERITIES = new Set<BleHealthSeverity>(["green", "yellow", "red"])
const CODES = new Set<BleHealthCode>([
  "connected",
  "paused_archiving",
  "paused_keep_awake",
  "reconnecting",
  "delayed",
  "idle",
  "repair_required",
])

function isBleHealth(value: unknown): value is BleHealth {
  if (typeof value !== "object" || value === null) return false
  const candidate = value as Partial<BleHealth>
  return (
    SEVERITIES.has(candidate.severity as BleHealthSeverity) &&
    CODES.has(candidate.code as BleHealthCode) &&
    (candidate.since_ts === null || typeof candidate.since_ts === "number") &&
    typeof candidate.label === "string" &&
    typeof candidate.guidance === "string"
  )
}

export function presentBleHealth(
  health: unknown,
  secondsAgo: number | null,
): BleHealthPresentation {
  if (isBleHealth(health)) {
    return {
      ...health,
      repairRequired: health.code === "repair_required",
    }
  }

  if (secondsAgo !== null && Number.isFinite(secondsAgo) && secondsAgo < 60) {
    return {
      severity: "green",
      code: "connected",
      since_ts: null,
      label: "Connected",
      guidance: "Authenticated vehicle telemetry is flowing.",
      repairRequired: false,
    }
  }

  if (secondsAgo !== null && Number.isFinite(secondsAgo) && secondsAgo < 600) {
    return {
      severity: "yellow",
      code: "delayed",
      since_ts: null,
      label: "Telemetry delayed",
      guidance: "SentryUSB is waiting for the next vehicle response.",
      repairRequired: false,
    }
  }

  return {
    severity: "yellow",
    code: "idle",
    since_ts: null,
    label: "Telemetry idle",
    guidance:
      "The car may be asleep. Wake it, then check Bluetooth Logs if values do not refresh.",
    repairRequired: false,
  }
}

/**
 * Turn confirmed BLE health plus live motion/charge signals into the
 * dashboard's vehicle-state label. A non-green health state always wins:
 * missing power or stale telemetry is not evidence that the car is asleep.
 */
export function deriveVehicleStatusLabel(
  health: BleHealthPresentation,
  isDriving: boolean,
  charging: boolean,
): string {
  if (health.severity !== "green") return health.label
  if (charging) return "Charging"
  if (isDriving) return "Driving"
  return "Idle"
}
