const API_BASE = "/api"

async function request<T>(path: string, options?: RequestInit): Promise<T> {
  const res = await fetch(`${API_BASE}${path}`, {
    headers: {
      "Content-Type": "application/json",
      ...options?.headers,
    },
    ...options,
  })
  if (!res.ok) {
    throw new Error(`API error: ${res.status} ${res.statusText}`)
  }
  return res.json() as Promise<T>
}

export interface PiStatus {
  cpu_temp: string
  num_snapshots: string
  snapshot_oldest: string
  snapshot_newest: string
  total_space: string
  free_space: string
  uptime: string
  drives_active: string
  /** UDC host-link state; unlike drives_active, "configured" confirms enumeration. */
  udc_state?: string
  /** Seconds since the car last wrote to cam_disk.bin, -1 when unknown. */
  cam_last_write_secs?: number
  wifi_ssid: string
  wifi_strength: string
  wifi_ip: string
  ether_ip: string
  ether_speed: string
  fan_speed: string
  sbc_model?: string
  /** Negative dBm value parsed from iwconfig. */
  wifi_signal_dbm?: number
  wifi_rx_bps?: number
  wifi_tx_bps?: number
  ether_rx_bps?: number
  ether_tx_bps?: number
}

export interface DriveStats {
  drives_count: number
  routes_count: number
  processed_count: number
  total_distance_km: number
  total_distance_mi: number
  total_duration_ms: number
  fsd_engaged_ms: number
  fsd_distance_km: number
  fsd_distance_mi: number
  fsd_percent: number
  fsd_disengagements: number
  fsd_accel_pushes: number
  autosteer_engaged_ms: number
  autosteer_distance_km: number
  autosteer_distance_mi: number
  tacc_engaged_ms: number
  tacc_distance_km: number
  tacc_distance_mi: number
  assisted_percent: number
}

export interface DriveStatus {
  running: boolean
  routes_count: number
  processed_count: number
  phase?: string
  current?: number
  total?: number
  archiving?: boolean
  process_current?: number
  process_total?: number
}

export interface EventMeta {
  timestamp?: string
  city?: string
  reason?: string
  camera?: string
  latitude?: string
  longitude?: string
}

export interface ClipGroup {
  name: string
  clips: ClipEntry[]
  hasMore?: boolean
}

export interface ClipEntry {
  date: string
  path: string
  files: string[]
  event?: EventMeta
}

export interface StorageBreakdown {
  cam_size: number
  music_size: number
  lightshow_size: number
  boombox_size: number
  snapshots_size: number
  total_space: number
  free_space: number
}

interface FSDDayStats {
  date: string
  dayName: string
  disengagements: number
  accelPushes: number
  fsdPercent: number
  drives: number
  fsdDistanceKm?: number
  fsdDistanceMi?: number
  totalDurationMs?: number
  fsdEngagedMs?: number
}

export interface FSDAnalytics {
  period: string
  period_start: string
  total_drives: number
  fsd_sessions: number
  fsd_percent: number
  today_percent: number
  best_day: string
  best_day_percent: number
  fsd_engaged_ms: number
  fsd_distance_km: number
  fsd_distance_mi: number
  total_distance_km: number
  total_distance_mi: number
  disengagements: number
  accel_pushes: number
  daily: FSDDayStats[]
  fsd_grade: string
  streak_days: number
  fsd_time_formatted: string
  avg_disengagements_per_drive: number
  avg_accel_pushes_per_drive: number
  autosteer_engaged_ms: number
  autosteer_distance_km: number
  autosteer_distance_mi: number
  tacc_engaged_ms: number
  tacc_distance_km: number
  tacc_distance_mi: number
  assisted_percent: number
}

export interface SafetyScoreBreakdown {
  score: number
  hardBrakePct: number
  aggrTurnPct: number
  speedingPct: number
  nightPct: number
  hardBrakePenalty: number
  aggrTurnPenalty: number
  speedingPenalty: number
  nightPenalty: number
  fsdSharePct: number
  fsdReliefPct: number
}

export interface SafetyDayStats {
  date: string
  dayName: string
  score: number | null
  drives: number
  distanceMi: number
  distanceKm: number
  hardBrakeEvents: number
  aggrTurnEvents: number
  speedingMs: number
  nightMi: number
  nightMs: number
  nightWeightedMs: number
  movingMs: number
  imuMovingMs: number
  manualMovingMs: number
  hardBrakeMs: number
  aggrTurnMs: number
  brakeAnyMs: number
  turnAnyMs: number
  coveragePct: number
  eligible: boolean
}

export interface SafetyAnalytics {
  modelId: string
  modelLabel: string
  period: string
  periodStart: string
  totalDrives: number
  scoredDrives: number
  compatibleDays: number
  compatibleDistanceMi: number
  totalNativeDistanceMi: number
  coveragePct: number
  unavailableFactors: string[]
  score: SafetyScoreBreakdown | null
  totalDistanceMi: number
  totalDistanceKm: number
  movingMs: number
  imuMovingMs: number
  nightMs: number
  nightWeightedMs: number
  manualMovingMs: number
  hardBrakeEvents: number
  hardBrakeMs: number
  aggrTurnEvents: number
  aggrTurnMs: number
  speedingMs: number
  brakeAnyMs: number
  turnAnyMs: number
  nightMi: number
  nightKm: number
  assistedPercent: number
  fsdDisengagements: number
  daily: SafetyDayStats[]
  bestDay: string
  bestDayScore: number | null
}

export interface TelemetryFrame {
  t: number
  lat: number
  lng: number
  speed_mps: number
  gear: number
  autopilot: number
  accel_pos: number
}

export interface ClipTelemetry {
  frames: TelemetryFrame[]
  duration_sec: number
  has_gps: boolean
  has_autopilot: boolean
}

export const api = {
  // Travel Mode keeps the USB gadget connected while archiving. Omitted
  // optional cadence and retry flags leave their persisted values unchanged.
  getTravelMode: () =>
    request<{
      enabled: boolean
      half_snapshots: boolean
      fast_retry: boolean
      snapshot_interval_sec: number
    }>("/travel-mode/status"),
  setTravelMode: (enabled: boolean, halfSnapshots?: boolean, fastRetry?: boolean) =>
    request<{
      ok: boolean
      enabled: boolean
      half_snapshots: boolean
      fast_retry: boolean
      snapshot_interval_sec: number
    }>("/travel-mode", {
      method: "POST",
      body: JSON.stringify({
        enabled,
        ...(halfSnapshots === undefined ? {} : { half_snapshots: halfSnapshots }),
        ...(fastRetry === undefined ? {} : { fast_retry: fastRetry }),
      }),
    }),
  getStatus: () => request<PiStatus>("/status"),
  getStorageBreakdown: () => request<StorageBreakdown>("/status/storage"),
  getDriveStats: () => request<DriveStats>("/drives/stats"),
  getDriveStatus: () => request<DriveStatus>("/drives/status"),
  getFSDAnalytics: (period: string = "week") =>
    request<FSDAnalytics>(`/drives/fsd-analytics?period=${period}`),
  getSafetyAnalytics: (period: string = "month") =>
    request<SafetyAnalytics>(`/drives/safety-analytics?period=${period}`),
  getClipTelemetry: (clipPath: string, file: string) =>
    request<ClipTelemetry>(`/clips/telemetry?path=${encodeURIComponent(clipPath)}&file=${encodeURIComponent(file)}`),
}
