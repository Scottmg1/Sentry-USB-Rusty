import { useEffect, useRef, useState } from "react"
import { Link } from "react-router-dom"
import {
  Thermometer,
  HardDrive,
  Wifi,
  Clock,
  Camera,
  Activity,
  Cable,
  Info,
  HeartPulse,
  Timer,
  Zap,
  ChevronRight,
  Download,
  AlertTriangle,
  Wind,
} from "lucide-react"
import { api } from "@/lib/api"
import { useKeepAwake } from "@/hooks/useKeepAwake"
import { useUpdateAvailable } from "@/hooks/useUpdateAvailable"
import type { PiStatus, DriveStats, StorageBreakdown } from "@/lib/api"
import { wsClient } from "@/lib/ws"
import { formatUptime, formatBytes, formatTemp } from "@/lib/utils"

function getTempColor(milliC: number): "emerald" | "amber" | "red" {
  if (milliC < 55000) return "emerald"
  if (milliC < 70000) return "amber"
  return "red"
}

function getWifiStrengthBars(strength: string): number {
  if (!strength) return 0
  const parts = strength.split("/")
  if (parts.length !== 2) return 0
  const ratio = parseInt(parts[0]) / parseInt(parts[1])
  if (ratio > 0.75) return 4
  if (ratio > 0.5) return 3
  if (ratio > 0.25) return 2
  return 1
}

interface ProcessProgress {
  current: number
  total: number
}

interface ProgressSample {
  time: number
  current: number
}

const RATE_WINDOW = 6 // ~30s at 5s poll interval

// Computes ETA using a rolling window of recent samples for a responsive rate estimate.
function computeETA(current: number, total: number, history: ProgressSample[]): string | null {
  if (history.length < 2) return null
  const oldest = history[0]
  const newest = history[history.length - 1]
  const elapsed = (newest.time - oldest.time) / 1000
  const done = newest.current - oldest.current
  if (done <= 0 || elapsed < 5) return null
  const rate = done / elapsed
  const remaining = (total - current) / rate
  if (!isFinite(remaining) || remaining <= 0) return null
  if (remaining < 60) return `~${Math.round(remaining)}s`
  if (remaining < 3600) return `~${Math.round(remaining / 60)}m`
  return `~${(remaining / 3600).toFixed(1)}h`
}

export default function Dashboard() {
  const [status, setStatus] = useState<PiStatus | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [uptime, setUptime] = useState(0)
  const [driveStats, setDriveStats] = useState<DriveStats | null>(null)
  const [storageBreakdown, setStorageBreakdown] = useState<StorageBreakdown | null>(null)
  const [archiveProgress, setArchiveProgress] = useState<ProcessProgress | null>(null)
  const [processing, setProcessing] = useState(false)
  const [processProgress, setProcessProgress] = useState<ProcessProgress | null>(null)
  const [metric, setMetric] = useState(false)
  const [useFahrenheit, setUseFahrenheit] = useState(false)
  const [rtcWarning, setRtcWarning] = useState<string | null>(null)

  const archiveHistoryRef = useRef<ProgressSample[]>([])
  const processHistoryRef = useRef<ProgressSample[]>([])
  const updateInfo = useUpdateAvailable()

  const active = archiveProgress !== null || processing

  useEffect(() => {
    let mounted = true
    let uptimeInterval: ReturnType<typeof setInterval>

    async function fetchStatus() {
      try {
        const data = await api.getStatus()
        if (!mounted) return
        setStatus(data)
        setUptime(parseFloat(data.uptime))
        setError(null)
      } catch {
        if (mounted) setError("Unable to connect to Sentry USB")
      }
    }

    async function fetchDriveStats() {
      try {
        const [stats, driveStatus] = await Promise.all([
          api.getDriveStats(),
          api.getDriveStatus(),
        ])
        if (!mounted) return
        setDriveStats(stats)
        setProcessing(driveStatus.running)
        if (!driveStatus.running) {
          setProcessProgress(null)
        } else if (driveStatus.process_total != null && driveStatus.process_total > 0) {
          // Pick up processing progress from polling so the UI updates
          // even if the WebSocket missed messages during the transition.
          setProcessProgress({
            current: driveStatus.process_current ?? 0,
            total: driveStatus.process_total,
          })
        }

        if (driveStatus.phase === "archiving" && driveStatus.total != null) {
          setArchiveProgress({
            current: driveStatus.current ?? 0,
            total: driveStatus.total,
          })
        } else {
          setArchiveProgress(null)
        }
      } catch {
        // non-critical — drive stats may not be available
      }
    }

    async function fetchStorageBreakdown() {
      try {
        const data = await api.getStorageBreakdown()
        if (mounted) setStorageBreakdown(data)
      } catch { /* non-critical */ }
    }

    fetchStatus()
    fetchDriveStats()
    fetchStorageBreakdown()
    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        const entry = cfg.DRIVE_MAP_UNIT
        if (entry) {
          const val = typeof entry === "object"
            ? (entry.active ? entry.value : null)
            : entry
          if (val !== null) setMetric(val === "km")
        }
        const tempEntry = cfg.TEMPERATURE_UNIT
        if (tempEntry) {
          const val = typeof tempEntry === "object"
            ? (tempEntry.active ? tempEntry.value : null)
            : tempEntry
          if (val !== null) setUseFahrenheit(val === "F")
        }
      })
      .catch(() => { })

    // Check RTC battery health (Pi 5 only)
    fetch("/api/system/rtc-status")
      .then((r) => r.json())
      .then((rtc) => {
        if (mounted && rtc.is_pi5 && !rtc.rtc_healthy && rtc.battery_warning) {
          setRtcWarning(rtc.battery_warning)
        }
      })
      .catch(() => { })

    const statusInterval = setInterval(fetchStatus, 4000)
    const statsInterval = setInterval(fetchDriveStats, 5000)
    const storageInterval = setInterval(fetchStorageBreakdown, 10000)

    uptimeInterval = setInterval(() => {
      setUptime((prev) => prev + 1)
    }, 1000)

    // Subscribe to real-time GPS processing progress via WebSocket
    const unsubscribe = wsClient.subscribe("drive_process", (data) => {
      if (!mounted) return
      const msg = data as { status: string; current?: number; total?: number }
      if (msg.status === "started") {
        setProcessing(true)
        setProcessProgress(null)
      } else if (msg.status === "progress" && msg.current !== undefined && msg.total !== undefined) {
        setProcessing(true)
        setProcessProgress({ current: msg.current, total: msg.total })
      } else if (msg.status === "complete" || msg.status === "error") {
        setProcessing(false)
        setProcessProgress(null)
        fetchDriveStats()
      }
    })

    return () => {
      mounted = false
      clearInterval(statusInterval)
      clearInterval(statsInterval)
      clearInterval(storageInterval)
      clearInterval(uptimeInterval)
      unsubscribe()
    }
  }, [])

  useEffect(() => {
    if (archiveProgress && archiveProgress.current > 0) {
      const h = archiveHistoryRef.current
      h.push({ time: Date.now(), current: archiveProgress.current })
      if (h.length > RATE_WINDOW) h.shift()
    } else {
      archiveHistoryRef.current = []
    }
  }, [archiveProgress])

  useEffect(() => {
    if (processProgress && processProgress.current > 0) {
      const h = processHistoryRef.current
      h.push({ time: Date.now(), current: processProgress.current })
      if (h.length > RATE_WINDOW) h.shift()
    } else {
      processHistoryRef.current = []
    }
  }, [processProgress])

  if (error) {
    return (
      <div className="flex flex-col items-center justify-center py-20">
        <Activity className="mb-4 h-12 w-12 text-slate-600" />
        <p className="text-lg font-medium text-slate-400">{error}</p>
        <p className="mt-1 text-sm text-slate-600">
          Make sure the Sentry USB API server is running
        </p>
      </div>
    )
  }

  if (!status) {
    return (
      <div className="space-y-4">
        <h1 className="text-2xl font-bold text-slate-100">Dashboard</h1>
        <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
          {[...Array(6)].map((_, i) => (
            <div key={i} className="glass-card h-24 animate-pulse" />
          ))}
        </div>
      </div>
    )
  }

  const cpuTemp = parseInt(status.cpu_temp)
  const totalSpace = parseInt(status.total_space)
  const freeSpace = parseInt(status.free_space)
  const usedSpace = totalSpace - freeSpace
  const usedPercent = totalSpace > 0 ? ((usedSpace / totalSpace) * 100).toFixed(0) : "0"
  const wifiBars = getWifiStrengthBars(status.wifi_strength)
  const snapshotCount = parseInt(status.num_snapshots)

  return (
    <div className="space-y-4">
      <div>
        <h1 className="text-2xl font-bold text-slate-100">Dashboard</h1>
        <p className="mt-1 text-sm text-slate-500">
          System overview and status
        </p>
      </div>

      {updateInfo.available && (
        <Link
          to="/settings"
          className="glass-card flex items-center gap-3 border border-amber-500/20 bg-amber-500/5 p-3 transition-colors hover:bg-amber-500/10"
        >
          <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-amber-500/20">
            <Download className="h-4 w-4 text-amber-400" />
          </div>
          <div className="flex-1">
            <span className="text-sm font-semibold text-amber-200">
              Update Available{updateInfo.latestVersion ? `: ${updateInfo.latestVersion}` : ""}
            </span>
            <p className="text-xs text-slate-500">Go to Settings to install</p>
          </div>
          <ChevronRight className="h-4 w-4 text-slate-600" />
        </Link>
      )}

      {rtcWarning && (
        <div className="glass-card flex items-center gap-3 border border-amber-500/20 bg-amber-500/5 p-3">
          <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-lg bg-amber-500/20">
            <AlertTriangle className="h-4 w-4 text-amber-400" />
          </div>
          <div className="flex-1">
            <span className="text-sm font-semibold text-amber-200">RTC Battery Warning</span>
            <p className="text-xs text-slate-500">{rtcWarning}</p>
          </div>
        </div>
      )}

      {/* Row 1: Status tiles */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {/* System tile: Uptime + CPU Temp + USB Drives */}
        <div className="glass-card p-4">
          <div className="flex items-start gap-3">
            <div
              className={`flex h-10 w-10 shrink-0 items-center justify-center rounded-lg ${
                cpuTemp > 0
                  ? { emerald: "text-emerald-400 bg-emerald-500/15", amber: "text-amber-400 bg-amber-500/15", red: "text-red-400 bg-red-500/15" }[getTempColor(cpuTemp)]
                  : "text-blue-400 bg-blue-500/15"
              }`}
            >
              <Activity className="h-5 w-5" />
            </div>
            <div className="min-w-0 flex-1">
              <p className="text-xs font-medium uppercase tracking-wider text-slate-500">
                System
              </p>
              <div className="mt-2 space-y-1.5">
                <div className="flex items-center gap-2">
                  <Clock className="h-3.5 w-3.5 text-slate-500" />
                  <span className="text-xs text-slate-400">Uptime</span>
                  <span className="ml-auto text-sm font-semibold text-slate-100">{formatUptime(uptime)}</span>
                </div>
                <div className="flex items-center gap-2">
                  <Thermometer className="h-3.5 w-3.5 text-slate-500" />
                  <span className="text-xs text-slate-400">CPU</span>
                  <span className={`ml-auto text-sm font-semibold ${
                    cpuTemp > 0
                      ? { emerald: "text-emerald-400", amber: "text-amber-400", red: "text-red-400" }[getTempColor(cpuTemp)]
                      : "text-slate-100"
                  }`}>
                    {cpuTemp > 0 ? formatTemp(cpuTemp, useFahrenheit) : "N/A"}
                  </span>
                </div>
                {status.fan_speed && (
                  <div className="flex items-center gap-2">
                    <Wind className="h-3.5 w-3.5 text-slate-500" />
                    <span className="text-xs text-slate-400">Fan Speed</span>
                    <span className="ml-auto text-sm font-semibold text-slate-100">
                      {status.fan_speed} RPM
                    </span>
                  </div>
                )}
                <div className="flex items-center gap-2">
                  <HardDrive className="h-3.5 w-3.5 text-slate-500" />
                  <span className="text-xs text-slate-400">USB Drives</span>
                  <span className={`ml-auto text-xs font-medium ${status.drives_active === "yes" ? "text-emerald-400" : "text-amber-400"}`}>
                    {status.drives_active === "yes" ? "Connected" : "Disconnected"}
                  </span>
                </div>
              </div>
            </div>
          </div>
        </div>

        {/* Storage tile: Storage + Snapshots */}
        <div className="glass-card p-4">
          <div className="flex items-start gap-3">
            <div
              className={`flex h-10 w-10 shrink-0 items-center justify-center rounded-lg ${
                parseInt(usedPercent) > 90
                  ? "text-red-400 bg-red-500/15"
                  : parseInt(usedPercent) > 75
                    ? "text-amber-400 bg-amber-500/15"
                    : "text-emerald-400 bg-emerald-500/15"
              }`}
            >
              <HardDrive className="h-5 w-5" />
            </div>
            <div className="min-w-0 flex-1">
              <div className="flex items-center gap-1.5">
                <p className="text-xs font-medium uppercase tracking-wider text-slate-500">
                  Storage
                </p>
                <div className="group relative">
                  <Info className="h-3 w-3 cursor-help text-slate-600 transition-colors hover:text-slate-400" />
                  <div className="pointer-events-none absolute left-0 top-full z-50 mt-2 w-64 rounded-xl border border-white/10 bg-slate-900 p-3 text-[11px] leading-relaxed text-slate-400 opacity-0 shadow-xl transition-opacity group-hover:pointer-events-auto group-hover:opacity-100">
                    <div className="absolute bottom-full left-4 border-4 border-transparent border-b-slate-900" />
                    Sentry USB automatically manages your storage. Old snapshots are deleted when space is needed — you don't need to manually free up space. Low remaining space is normal and expected, especially with dashcam footage being continuously saved.
                  </div>
                </div>
              </div>
              <p className="mt-1 text-sm font-semibold text-slate-100">
                {formatBytes(usedSpace)} / {formatBytes(totalSpace)}
              </p>
              <p className="text-xs text-slate-500">{usedPercent}% used</p>
              <div className="mt-2 border-t border-white/5 pt-2">
                <div className="flex items-center gap-2">
                  <Camera className="h-3.5 w-3.5 text-purple-400" />
                  <span className="text-xs text-slate-400">{snapshotCount} snapshots</span>
                  {snapshotCount > 0 && (
                    <span className="ml-auto text-[10px] text-slate-600">
                      {new Date(parseInt(status.snapshot_oldest) * 1000).toLocaleDateString()} — {new Date(parseInt(status.snapshot_newest) * 1000).toLocaleDateString()}
                    </span>
                  )}
                </div>
              </div>
            </div>
          </div>
        </div>

        {/* Network tile: WiFi + Ethernet */}
        <div className="glass-card p-4">
          <div className="flex items-start gap-3">
            <div
              className={`flex h-10 w-10 shrink-0 items-center justify-center rounded-lg ${
                status.wifi_ssid || (status.ether_speed && status.ether_speed !== "Unknown!")
                  ? "text-emerald-400 bg-emerald-500/15"
                  : "text-red-400 bg-red-500/15"
              }`}
            >
              <Wifi className="h-5 w-5" />
            </div>
            <div className="min-w-0 flex-1">
              <p className="text-xs font-medium uppercase tracking-wider text-slate-500">
                Network
              </p>
              <div className="mt-2 space-y-1.5">
                <div className="flex items-center gap-2">
                  <Wifi className="h-3.5 w-3.5 text-slate-500" />
                  {status.wifi_ssid ? (
                    <>
                      <span className="truncate text-xs font-medium text-slate-200">{status.wifi_ssid}</span>
                      <span className="ml-auto text-[10px] text-slate-500">{status.wifi_ip || "No IP"} · {wifiBars}/4</span>
                    </>
                  ) : (
                    <span className="text-xs text-slate-500">WiFi not connected</span>
                  )}
                </div>
                {status.ether_speed && (
                  <div className="flex items-center gap-2">
                    <Cable className="h-3.5 w-3.5 text-slate-500" />
                    {status.ether_speed !== "Unknown!" ? (
                      <>
                        <span className="text-xs font-medium text-slate-200">{status.ether_speed}</span>
                        {status.ether_ip && (
                          <span className="ml-auto text-[10px] text-slate-500">{status.ether_ip}</span>
                        )}
                      </>
                    ) : (
                      <span className="text-xs text-slate-500">Ethernet not connected</span>
                    )}
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>

        <KeepAwakeTile />
      </div>

      {/* Row 2: Storage Usage + Archive Progress side by side */}
      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        {/* Storage Usage */}
        <div className="glass-card p-4">
          <div className="mb-2 flex items-center justify-between">
            <span className="text-sm font-medium text-slate-300">Storage Usage</span>
            <span className="text-xs text-slate-500">
              {formatBytes(usedSpace)} / {formatBytes(totalSpace)} · {usedPercent}% used
            </span>
          </div>
          {storageBreakdown && storageBreakdown.total_space > 0 ? (() => {
            const segments = [
              { label: "Dashcam", size: storageBreakdown.cam_size, color: "#3b82f6" },
              { label: "Music", size: storageBreakdown.music_size, color: "#a855f7" },
              { label: "Lightshow", size: storageBreakdown.lightshow_size, color: "#f59e0b" },
              { label: "Boombox", size: storageBreakdown.boombox_size, color: "#ec4899" },
              { label: "Wraps", size: storageBreakdown.wraps_size, color: "#14b8a6" },
              { label: "Snapshots", size: storageBreakdown.snapshots_size, color: "#6366f1" },
            ].filter(s => s.size > 0)
            const total = storageBreakdown.total_space
            return (
              <>
                <div className="h-2.5 w-full overflow-hidden rounded-full bg-slate-800 flex">
                  {segments.map((s) => (
                    <div
                      key={s.label}
                      className="h-full transition-all duration-500 first:rounded-l-full last:rounded-r-full"
                      style={{
                        width: `${Math.max((s.size / total) * 100, 0.5)}%`,
                        backgroundColor: s.color,
                      }}
                      title={`${s.label}: ${formatBytes(s.size)}`}
                    />
                  ))}
                </div>
                <div className="mt-2 flex flex-wrap gap-x-3 gap-y-1">
                  {segments.map((s) => (
                    <div key={s.label} className="flex items-center gap-1.5 text-[10px]">
                      <span className="inline-block h-1.5 w-1.5 rounded-full" style={{ backgroundColor: s.color }} />
                      <span className="text-slate-400">{s.label}</span>
                      <span className="font-medium text-slate-300">{formatBytes(s.size)}</span>
                    </div>
                  ))}
                  <div className="flex items-center gap-1.5 text-[10px]">
                    <span className="inline-block h-1.5 w-1.5 rounded-full bg-slate-700" />
                    <span className="text-slate-400">Free</span>
                    <span className="font-medium text-slate-300">{formatBytes(storageBreakdown.free_space)}</span>
                  </div>
                </div>
              </>
            )
          })() : (
            <div className="h-2.5 w-full overflow-hidden rounded-full bg-slate-800">
              <div
                className="h-full rounded-full bg-gradient-to-r from-blue-500 to-blue-400 transition-all duration-500"
                style={{ width: `${usedPercent}%` }}
              />
            </div>
          )}
        </div>

        {/* Clip Archive Progress */}
        <div className="glass-card p-4">
          <div className="mb-2 flex items-center justify-between">
            <span className="text-sm font-medium text-slate-300">Clip Archive</span>
            {active && (
              <span className={`flex items-center gap-1.5 text-xs ${archiveProgress ? "text-emerald-400" : "text-blue-400"}`}>
                <span className={`inline-block h-1.5 w-1.5 animate-pulse rounded-full ${archiveProgress ? "bg-emerald-400" : "bg-blue-400"}`} />
                {archiveProgress ? "Archiving" : "Processing"}
              </span>
            )}
          </div>
          {driveStats ? (
            <>
              <div className="flex items-baseline gap-3 text-xs">
                <span className="font-semibold text-slate-100">{driveStats.processed_count.toLocaleString()}</span>
                <span className="text-slate-500">clips</span>
                <span className="font-semibold text-slate-100">{driveStats.drives_count.toLocaleString()}</span>
                <span className="text-slate-500">drives</span>
                <span className="font-semibold text-slate-100">
                  {metric ? driveStats.total_distance_km.toFixed(0) : driveStats.total_distance_mi.toFixed(0)}
                </span>
                <span className="text-slate-500">{metric ? "km" : "mi"}</span>
                {driveStats.fsd_engaged_ms > 0 && (
                  <Link to="/fsd" className="ml-auto flex items-center gap-1 text-[10px] text-emerald-400 hover:text-emerald-300 transition-colors">
                    <Zap className="h-3 w-3" />
                    FSD {driveStats.fsd_percent}%
                    <ChevronRight className="h-3 w-3 text-slate-600" />
                  </Link>
                )}
              </div>

              {archiveProgress && archiveProgress.total > 0 ? (
                <>
                  <div className="mt-2 mb-1 flex items-center justify-between text-[10px] text-slate-500">
                    <span>
                      {archiveProgress.current.toLocaleString()} / {archiveProgress.total.toLocaleString()}
                      {(() => {
                        const eta = computeETA(archiveProgress.current, archiveProgress.total, archiveHistoryRef.current)
                        return eta ? <span className="ml-1.5 text-emerald-400/70">{eta}</span> : null
                      })()}
                    </span>
                    <span>{Math.round((archiveProgress.current / archiveProgress.total) * 100)}%</span>
                  </div>
                  <div className="h-1.5 w-full overflow-hidden rounded-full bg-slate-800">
                    <div className="h-full rounded-full bg-gradient-to-r from-emerald-500 to-emerald-400 transition-all duration-500" style={{ width: `${(archiveProgress.current / archiveProgress.total) * 100}%` }} />
                  </div>
                </>
              ) : processing && processProgress && processProgress.total > 0 ? (
                <>
                  <div className="mt-2 mb-1 flex items-center justify-between text-[10px] text-slate-500">
                    <span>
                      {processProgress.current.toLocaleString()} / {processProgress.total.toLocaleString()}
                      {(() => {
                        const eta = computeETA(processProgress.current, processProgress.total, processHistoryRef.current)
                        return eta ? <span className="ml-1.5 text-blue-400/70">{eta}</span> : null
                      })()}
                    </span>
                    <span>{Math.round((processProgress.current / processProgress.total) * 100)}%</span>
                  </div>
                  <div className="h-1.5 w-full overflow-hidden rounded-full bg-slate-800">
                    <div className="h-full rounded-full bg-gradient-to-r from-blue-500 to-blue-400 transition-all duration-500" style={{ width: `${(processProgress.current / processProgress.total) * 100}%` }} />
                  </div>
                </>
              ) : processing ? (
                <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-slate-800">
                  <div className="h-full w-2/5 animate-pulse rounded-full bg-gradient-to-r from-blue-500 to-blue-400" />
                </div>
              ) : archiveProgress ? (
                <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-slate-800">
                  <div className="h-full w-2/5 animate-pulse rounded-full bg-gradient-to-r from-emerald-500 to-emerald-400" />
                </div>
              ) : (
                <div className="mt-2 h-1.5 w-full overflow-hidden rounded-full bg-slate-800">
                  <div className="h-full rounded-full bg-gradient-to-r from-emerald-500/60 to-emerald-400/60" style={{ width: driveStats.processed_count > 0 ? "100%" : "0%" }} />
                </div>
              )}
            </>
          ) : (
            <div className="space-y-1.5">
              <div className="h-3 w-1/2 animate-pulse rounded bg-slate-800" />
              <div className="h-1.5 w-full animate-pulse rounded-full bg-slate-800" />
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

const DURATION_OPTIONS = [
  { label: "15m", value: 15 },
  { label: "30m", value: 30 },
  { label: "1h", value: 60 },
  { label: "2h", value: 120 },
]

function KeepAwakeTile() {
  const { status, mode, start, stop } = useKeepAwake()
  const [showDurations, setShowDurations] = useState(false)

  if (!mode) return null

  const isActive = status.state === "active"
  const isPending = status.state === "pending"
  const isIdle = status.state === "idle"
  const remainingMin = status.remaining_sec ? Math.ceil(status.remaining_sec / 60) : 0

  const color = isActive ? "red" : isPending ? "amber" : "blue"
  const colorMap = {
    blue: "text-blue-400 bg-blue-500/15",
    amber: "text-amber-400 bg-amber-500/15",
    red: "text-rose-400 bg-rose-500/15",
  }

  const value = isActive
    ? `${remainingMin}m`
    : isPending
      ? "Pending"
      : mode === "auto"
        ? "Auto"
        : "Idle"

  const sub = isActive
    ? "Keeping car awake"
    : isPending
      ? "Waiting for archive..."
      : mode === "auto"
        ? "Activates on interaction"
        : "Tap to start"

  return (
    <div className="glass-card relative p-4">
      <div className="flex items-start gap-3">
        <div className={`flex h-10 w-10 shrink-0 items-center justify-center rounded-lg ${colorMap[color]}`}>
          {isActive ? (
            <HeartPulse className="h-5 w-5 animate-pulse" />
          ) : isPending ? (
            <Timer className="h-5 w-5 animate-pulse" />
          ) : (
            <HeartPulse className="h-5 w-5" />
          )}
        </div>
        <div className="min-w-0 flex-1">
          <p className="text-xs font-medium uppercase tracking-wider text-slate-500">
            Keep Awake
          </p>
          <div className="mt-1 flex items-center gap-2">
            <p className="text-lg font-semibold text-slate-100">{value}</p>
            {mode === "manual" && isIdle && (
              <div className="relative ml-auto">
                <button
                  onClick={() => setShowDurations(!showDurations)}
                  className="rounded-lg bg-blue-500/20 px-2.5 py-1 text-[11px] font-medium text-blue-400 transition-colors hover:bg-blue-500/30"
                >
                  Start
                </button>
                {showDurations && (
                  <div className="absolute right-0 top-full z-10 mt-1 w-28 rounded-lg border border-white/10 bg-slate-900 p-1 shadow-xl">
                    {DURATION_OPTIONS.map((opt) => (
                      <button
                        key={opt.value}
                        onClick={() => { start(opt.value); setShowDurations(false) }}
                        className="w-full rounded-md px-3 py-1.5 text-left text-xs text-slate-300 hover:bg-white/5"
                      >
                        {opt.label}
                      </button>
                    ))}
                  </div>
                )}
              </div>
            )}
            {(isActive || isPending) && (
              <button
                onClick={stop}
                className="ml-auto rounded-lg bg-red-500/15 px-2.5 py-1 text-[11px] font-medium text-red-400 transition-colors hover:bg-red-500/25"
              >
                Stop
              </button>
            )}
          </div>
          <p className="mt-0.5 text-xs text-slate-500">{sub}</p>
        </div>
      </div>
    </div>
  )
}
