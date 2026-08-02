export interface DriveSummary {
  id: number
  date: string
  startTime: string
  endTime: string
  durationMs: number
  distanceMi: number
  distanceKm: number
  avgSpeedMph: number
  maxSpeedMph: number
  avgSpeedKmh: number
  maxSpeedKmh: number
  clipCount: number
  pointCount: number
  startPoint: [number, number] | null
  endPoint: [number, number] | null
  tags?: string[]
  fsdEngagedMs: number
  fsdDisengagements: number
  fsdAccelPushes: number
  fsdPercent: number
  fsdDistanceKm: number
  fsdDistanceMi: number
  autosteerEngagedMs: number
  autosteerPercent: number
  autosteerDistanceKm: number
  autosteerDistanceMi: number
  taccEngagedMs: number
  taccPercent: number
  taccDistanceKm: number
  taccDistanceMi: number
  assistedPercent: number
  batteryPctStart?: number
  batteryPctEnd?: number
  batteryPctUsed?: number
  interiorTempMinC?: number
  interiorTempMaxC?: number
  exteriorTempAvgC?: number
  hvacRuntimeS?: number
  tireFlPsi?: number
  tireFrPsi?: number
  tireRlPsi?: number
  tireRrPsi?: number
  odometerMiStart?: number
  odometerMiEnd?: number
  odometerMiDriven?: number
  startLocation?: string
  endLocation?: string
  source?: string
  externalSignature?: string
  tessieAutopilotPercent?: number
  /** Detected Summon / Smart Summon drive — present only when true. */
  summon?: boolean
}

export interface FsdEvent {
  lat: number
  lng: number
  type: "disengagement" | "accel_push"
}

export interface DriveDetail extends Omit<DriveSummary, "startPoint" | "endPoint"> {
  points: [number, number, number, number][]
  gearStates?: number[]
  fsdStates?: number[]
  fsdEvents?: FsdEvent[]
}

export interface RouteOverview {
  id: number
  points: [number, number][]
  source?: string
  // ISO start_time of the drive, used to join with DriveSummary entries.
  // The integer `id` is NOT a reliable join key because /api/drives uses
  // sub-clip-aware grouping while /api/drives/routes uses full-clip
  // grouping; the enumerations can drift apart on drives split by
  // mid-clip park gaps.
  startTime: string
}
