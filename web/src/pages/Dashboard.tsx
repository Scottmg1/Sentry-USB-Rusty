import { useEffect, useRef, useState } from "react"
import { Link } from "react-router-dom"
import {
  AirIcon,
  BoltIcon,
  CardiologyIcon,
  ChevronRightIcon,
  ConversionPathIcon,
  DeviceThermostatIcon,
  DownloadIcon,
  HardDriveIcon,
  InfoIcon,
  LanIcon,
  LocationOnIcon,
  MovieIcon,
  PhotoCameraIcon,
  ScheduleIcon,
  TimerIcon,
  VitalSignsIcon,
  WarningIcon,
  WifiIcon,
  WifiOffIcon,
} from "@/components/icons"
import { api } from "@/lib/api"
import { useKeepAwake } from "@/hooks/useKeepAwake"
import { useAwayMode } from "@/hooks/useAwayMode"
import { useUpdateAvailable } from "@/hooks/useUpdateAvailable"
import { useWifiFirmware } from "@/hooks/useWifiFirmware"
import { WifiFirmwareModal } from "@/components/dashboard/WifiFirmwareModal"
import { fetchCurrentCharge } from "@/api/charging"
import type { CurrentCharge } from "@/types/charging"
import type { PiStatus, DriveStats, StorageBreakdown } from "@/lib/api"
import { wsClient } from "@/lib/ws"
import { formatUptime, formatBytes, formatTemp } from "@/lib/utils"
import { useUnits } from "@/lib/units"
import { CloudStatusBar } from "@/components/CloudStatusBar"
import {
  CarStatusCard,
  type CarStatusSample,
} from "@/components/dashboard/CarStatusCard"
import { StatusTile, Row, TileDivider } from "@/components/ui/StatusTile"
import { BannerStack, type BannerItem } from "@/components/ui/Banner"
import { Pill, LiveDot } from "@/components/ui/Pill"
import type { Halo } from "@/components/ui/StatusTile"
import type { TireHistoryResponse } from "@/components/dashboard/TirePressureCard"
import type { BleHealth } from "@/lib/bleHealth"

function getTempHalo(milliC: number): Halo {
  if (milliC <= 0) return "blue"
  if (milliC < 55000) return "accent"
  if (milliC < 70000) return "amber"
  return "red"
}

function getTempColor(milliC: number): string {
  if (milliC < 55000) return "oklch(0.82 0.18 150)"
  if (milliC < 70000) return "#fbbf24"
  return "#f87171"
}

function getStorageHalo(usedPct: number): Halo {
  if (usedPct > 90) return "red"
  if (usedPct > 75) return "amber"
  return "accent"
}

function formatThroughput(bps: number): string {
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(1)} Mbps`
  if (bps >= 1_000) return `${Math.round(bps / 1_000)} Kbps`
  return bps > 0 ? "< 1 Kbps" : "—"
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

// Mini 4-bar signal indicator. Filled bars get the tile's accent colour;
// the rest are a muted slate so the gauge reads at a glance.
function WifiBars({ bars }: { bars: number }) {
  return (
    <span className="inline-flex items-end gap-[2px] align-middle" aria-label={`${bars}/4 bars`}>
      {[1, 2, 3, 4].map((n) => (
        <span
          key={n}
          className={n <= bars ? "bg-emerald-400" : "bg-slate-700"}
          style={{ width: 3, height: 3 + n * 2, borderRadius: 1 }}
        />
      ))}
    </span>
  )
}

interface ProcessProgress {
  current: number
  total: number
}
interface ProgressSample {
  time: number
  current: number
}
const RATE_WINDOW = 6

function computeETA(
  current: number,
  total: number,
  history: ProgressSample[]
): string | null {
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
  const [storageBreakdown, setStorageBreakdown] =
    useState<StorageBreakdown | null>(null)
  const [archiveProgress, setArchiveProgress] = useState<ProcessProgress | null>(null)
  const [processing, setProcessing] = useState(false)
  const [processProgress, setProcessProgress] = useState<ProcessProgress | null>(null)
  // Units come from the shared store — coherent defaults and live-synced with
  // the Settings → Display & Units controls. systemTempF is the independent
  // System-tile CPU unit; tempF/km drive the dashboard temps and distances.
  const { tempF: useFahrenheit, systemTempF: systemUseFahrenheit, km: metric } = useUnits()
  const [rtcWarning, setRtcWarning] = useState<string | null>(null)
  // null = still probing, then either the response or `{points: []}`.
  // The card stays unmounted until points.length > 0, so vendor-charts
  // never loads for users without Tesla BLE telemetry.
  const [tireHistory, setTireHistory] = useState<TireHistoryResponse | null>(null)
  // Latest BLE-derived car-state snapshot for the CarStatusCard.
  // Polled at 30s — the BLE sampler itself runs once a minute while
  // parked + awake, so anything faster on the UI side is wasted.
  const [carStatusSample, setCarStatusSample] = useState<CarStatusSample | null>(null)
  const [bleHealth, setBleHealth] = useState<BleHealth | null>(null)
  const [bleHealthConfigured, setBleHealthConfigured] = useState(false)
  // Live charge status for the CarStatusCard battery chip.
  const [currentCharge, setCurrentCharge] = useState<CurrentCharge | null>(null)
  // ISO end-time of the latest drive on record — used to derive the
  // "Parked Xh Ym" duration. One-shot fetch on mount + a refresh
  // when a drive-process WebSocket completion comes in.
  const [latestDriveEnd, setLatestDriveEnd] = useState<string | null>(null)
  // Active lock-chime sound name (e.g. "Star Wars Theme") when the
  // feature is configured. null means "no active chime", which hides
  // the indicator entirely. Fetched once on mount + refreshed every
  // 5 minutes (the active chime rarely changes — manual user action
  // on the LockChime page is the only source).
  const [activeChimeName, setActiveChimeName] = useState<string | null>(null)

  const archiveHistoryRef = useRef<ProgressSample[]>([])
  const processHistoryRef = useRef<ProgressSample[]>([])
  const updateInfo = useUpdateAvailable()
  const wifiFirmware = useWifiFirmware()
  const [wifiFwOpen, setWifiFwOpen] = useState(false)
  const { status: awayStatus } = useAwayMode()
  const { mode: keepAwakeMode } = useKeepAwake()

  useEffect(() => {
    let mounted = true

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
        /* non-critical */
      }
    }

    async function fetchStorageBreakdown() {
      try {
        const data = await api.getStorageBreakdown()
        if (mounted) setStorageBreakdown(data)
      } catch {
        /* non-critical */
      }
    }

    fetchStatus()
    fetchDriveStats()
    fetchStorageBreakdown()

    fetch("/api/system/rtc-status")
      .then((r) => r.json())
      .then((rtc) => {
        if (mounted && rtc.is_pi5 && !rtc.rtc_healthy && rtc.battery_warning) {
          setRtcWarning(rtc.battery_warning)
        }
      })
      .catch(() => {})

    // Tire history: probe once at mount. The card only mounts (and
    // pulls in recharts) when the response has samples. Empty
    // response = the user hasn't paired BLE telemetry; we just hide
    // the card to keep the dashboard clean.
    fetch("/api/telemetry/tire-history?days=30")
      .then((r) => (r.ok ? r.json() : { points: [], days: 30 }))
      .then((d: TireHistoryResponse) => { if (mounted) setTireHistory(d) })
      .catch(() => { if (mounted) setTireHistory({ points: [], days: 30 }) })

    // Latest BLE sample drives the CarStatusCard's battery + temps +
    // tire-health summary. Hide-on-error since this is purely an
    // overview tile; the user can still pair BLE from Settings.
    async function fetchCarStatusSample() {
      try {
        const [sampleRes, healthRes] = await Promise.all([
          fetch("/api/system/ble-latest-sample"),
          fetch("/api/system/ble-connected"),
        ])
        if (!mounted) return
        if (healthRes.ok) {
          const d = (await healthRes.json()) as {
            configured?: boolean
            health?: BleHealth
          }
          if (mounted) setBleHealthConfigured(Boolean(d.configured))
          if (mounted) setBleHealth(d.health ?? null)
        } else if (mounted) {
          setBleHealthConfigured(false)
          setBleHealth(null)
        }
        if (sampleRes.ok) {
          const d = (await sampleRes.json()) as CarStatusSample
          if (mounted) setCarStatusSample(d)
        } else if (mounted) {
          setCarStatusSample(null)
        }
      } catch {
        if (mounted) {
          setBleHealthConfigured(false)
          setBleHealth(null)
          setCarStatusSample(null)
        }
      }
    }

    // Most recent drive's end-time → used by CarStatusCard to render
    // "Parked Xh Ym". /api/drives returns the cached list in
    // insertion order (NOT newest-first), so we have to find the
    // entry with the latest endTime ourselves — `drives[0]` would
    // give the oldest drive and produce a "Parked 600d 9h"-style
    // bogus duration.
    async function fetchLatestDrive() {
      try {
        const res = await fetch("/api/drives")
        if (!res.ok) return
        const drives = (await res.json()) as Array<{ endTime?: string }>
        if (!mounted) return
        if (!Array.isArray(drives) || drives.length === 0) return
        let latest: string | null = null
        let latestMs = -Infinity
        for (const d of drives) {
          if (!d.endTime) continue
          const ms = new Date(d.endTime).getTime()
          if (Number.isFinite(ms) && ms > latestMs) {
            latestMs = ms
            latest = d.endTime
          }
        }
        if (latest) setLatestDriveEnd(latest)
      } catch {
        /* non-critical */
      }
    }

    // Active lock-chime probe. Endpoint is /api/lockchime/list — it
    // returns the full sound directory, but we only need
    // active_name/active_set. The list is small (filename + size per
    // sound) so the extra payload is negligible.
    async function fetchActiveChime() {
      try {
        const res = await fetch("/api/lockchime/list")
        if (!res.ok) return
        const d = (await res.json()) as {
          active_set?: boolean
          active_name?: string
        }
        if (!mounted) return
        setActiveChimeName(d.active_set && d.active_name ? d.active_name : null)
      } catch {
        /* non-critical */
      }
    }

    async function fetchChargeStatus() {
      try {
        const c = await fetchCurrentCharge()
        if (mounted) setCurrentCharge(c)
      } catch {
        if (mounted) setCurrentCharge(null)
      }
    }

    fetchCarStatusSample()
    fetchLatestDrive()
    fetchActiveChime()
    fetchChargeStatus()
    // Pause every poller while the tab is hidden (phone in a pocket, a
    // backgrounded tab) so the dashboard stops hitting the Pi every 1-2s
    // and draining the phone battery for data nobody's looking at. The
    // `visibilitychange` handler below refreshes immediately on return so
    // the tiles don't sit stale waiting for the next tick.
    const carStatusInterval = setInterval(() => {
      if (!document.hidden) fetchCarStatusSample()
    }, 30_000)
    const chargeInterval = setInterval(() => {
      if (!document.hidden) fetchChargeStatus()
    }, 30_000)
    const chimeInterval = setInterval(() => {
      if (!document.hidden) fetchActiveChime()
    }, 300_000)

    // Status drives the live-tile values (CPU, mem, temp). 2s is fast
    // enough that a glance still feels real-time and halves the
    // server hits vs the previous 1s cadence. The uptime tile uses a
    // separate local 1s interval below so the seconds counter still
    // advances smoothly between server polls.
    const statusInterval = setInterval(() => {
      if (!document.hidden) fetchStatus()
    }, 2000)
    const statsInterval = setInterval(() => {
      if (!document.hidden) fetchDriveStats()
    }, 5000)
    const storageInterval = setInterval(() => {
      if (!document.hidden) fetchStorageBreakdown()
    }, 10000)
    // Local-only counter, but still skip it while hidden so React isn't
    // re-rendering the dashboard once a second for a backgrounded tab.
    const uptimeInterval = setInterval(() => {
      if (!document.hidden) setUptime((p) => p + 1)
    }, 1000)

    // Snap the live tiles back to current the moment the tab is shown
    // again, rather than waiting up to 30s for the slow intervals.
    const onVisible = () => {
      if (document.hidden) return
      fetchStatus()
      fetchDriveStats()
      fetchStorageBreakdown()
      fetchCarStatusSample()
      fetchChargeStatus()
    }
    document.addEventListener("visibilitychange", onVisible)

    const unsubscribe = wsClient.subscribe("drive_process", (data) => {
      if (!mounted) return
      const msg = data as { status: string; current?: number; total?: number }
      if (msg.status === "started") {
        setProcessing(true)
        setProcessProgress(null)
      } else if (
        msg.status === "progress" &&
        msg.current !== undefined &&
        msg.total !== undefined
      ) {
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
      clearInterval(carStatusInterval)
      clearInterval(chargeInterval)
      clearInterval(chimeInterval)
      document.removeEventListener("visibilitychange", onVisible)
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
        <VitalSignsIcon className="mb-4 h-12 w-12 text-slate-600" />
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
        <div className="tile-grid">
          {[...Array(4)].map((_, i) => (
            <div key={i} className="glass-card h-32 animate-pulse" />
          ))}
        </div>
      </div>
    )
  }

  // Build banner stack — priority sorted (warn > update).
  const banners: BannerItem[] = []
  if (rtcWarning) {
    banners.push({
      id: "rtc",
      kind: "warn",
      icon: <WarningIcon className="h-4 w-4" />,
      title: "RTC Battery Warning",
      sub: rtcWarning,
    })
  }
  if (wifiFirmware.installing) {
    // Progress has to be reachable even if the modal was closed, otherwise a
    // user who dismissed it assumes the update finished.
    banners.push({
      id: "wifi-firmware-running",
      kind: "info",
      icon: <WifiIcon className="h-4 w-4" />,
      title: `Updating Wi-Fi firmware… ${wifiFirmware.status?.install?.progress ?? 0}%`,
      sub: wifiFirmware.status?.install?.message || "Working…",
      action: (
        <button
          onClick={() => setWifiFwOpen(true)}
          className="action-chip action-chip--accent shrink-0"
        >
          View <ChevronRightIcon className="h-3.5 w-3.5" />
        </button>
      ),
    })
  }
  if (wifiFirmware.offerRevert) {
    banners.push({
      id: "wifi-firmware-revert",
      kind: "info",
      icon: <WifiIcon className="h-4 w-4" />,
      title: `Wi-Fi firmware updated to ${wifiFirmware.status?.target_version ?? ""}`,
      // Only ask for a restart while one is genuinely outstanding.
      sub: wifiFirmware.status?.reboot_pending
        ? "Reboot to finish. Reloading the radio in place can leave Wi-Fi slower than normal until the Pi restarts."
        : "You can put the previous firmware back if anything looks wrong.",
      action: (
        <div className="flex shrink-0 gap-2">
          <button onClick={() => setWifiFwOpen(true)} className="action-chip">
            {wifiFirmware.status?.reboot_pending ? "Finish" : "Revert"}
          </button>
          <button onClick={wifiFirmware.dismissRevert} className="action-chip">
            Dismiss
          </button>
        </div>
      ),
    })
  }
  if (wifiFirmware.show) {
    // Stronger wording when the fault's fingerprint is actually in this
    // device's kernel log, rather than a generic "you might hit this".
    const seen = wifiFirmware.status?.symptom_detected
    banners.push({
      id: "wifi-firmware",
      kind: "warn",
      icon: <WifiIcon className="h-4 w-4" />,
      title: seen
        ? "Wi-Fi firmware issue detected"
        : "Wi-Fi firmware update available",
      sub: seen
        ? "This Pi hit the Wi-Fi fault that slows archiving and breaks Bluetooth keep-awake."
        : "A newer Broadcom firmware fixes slow archiving and Bluetooth drop-outs.",
      action: (
        <button
          onClick={() => setWifiFwOpen(true)}
          className="action-chip action-chip--accent shrink-0"
        >
          Review <ChevronRightIcon className="h-3.5 w-3.5" />
        </button>
      ),
    })
  }
  if (updateInfo.available) {
    banners.push({
      id: "update",
      kind: "update",
      icon: <DownloadIcon className="h-4 w-4" />,
      title: `Update Available${
        updateInfo.latestVersion ? `: ${updateInfo.latestVersion}` : ""
      }`,
      sub: "Go to Settings to install",
      action: (
        <Link
          to="/settings?tab=Device"
          className="action-chip action-chip--accent shrink-0"
        >
          Install <ChevronRightIcon className="h-3.5 w-3.5" />
        </Link>
      ),
    })
  }

  const isAwayActive = awayStatus.state === "active"

  return (
    <div className="space-y-3">
      <div>
        <h1 className="text-2xl font-bold text-slate-100">Dashboard</h1>
        <p className="mt-0.5 text-sm text-slate-500">System overview and status</p>
      </div>

      <BannerStack banners={banners} />

      {wifiFwOpen && wifiFirmware.status && (
        <WifiFirmwareModal
          status={wifiFirmware.status}
          onClose={() => setWifiFwOpen(false)}
          onRefresh={wifiFirmware.refresh}
        />
      )}

      <CloudStatusBar />

      <div className="tile-grid">
        <SystemTile
          status={status}
          uptime={uptime}
          useFahrenheit={systemUseFahrenheit}
          keepAwakeIdle={keepAwakeMode == null}
        />
        <NetworkTile status={status} />
        <StorageTile
          status={status}
          breakdown={storageBreakdown}
        />
        <ActivityTile
          driveStats={driveStats}
          archiveProgress={archiveProgress}
          processProgress={processProgress}
          processing={processing}
          metric={metric}
          // eslint-disable-next-line react-hooks/refs -- ETA history is intentionally a ref (push-only, no re-render needed) and the original Dashboard read .current the same way.
          archiveEta={archiveProgress ? computeETA(archiveProgress.current, archiveProgress.total, archiveHistoryRef.current) : null}
          // eslint-disable-next-line react-hooks/refs -- same as above
          processEta={processProgress ? computeETA(processProgress.current, processProgress.total, processHistoryRef.current) : null}
        />
        {isAwayActive && <AwayModeTile />}
      </div>

      {/* Car status overview — last-known battery / cabin temps / tire
          health as chips, with the tire-pressure history chart behind an
          expand toggle (recharts stays unloaded until expanded). Spans the
          full content width so its flex-1 chips line up under the status
          tiles above; the page-level max-width keeps it from over-stretching
          on ultrawide. */}
      {(carStatusSample?.ts != null ||
        currentCharge?.soc != null ||
        (bleHealthConfigured && bleHealth != null)) && (
        <CarStatusCard
          sample={carStatusSample}
          bleHealth={bleHealth}
          latestDriveEnd={latestDriveEnd}
          tireHistory={tireHistory ?? undefined}
          useFahrenheit={useFahrenheit}
          metric={metric}
          currentCharge={currentCharge}
          lockChimeName={activeChimeName}
        />
      )}
    </div>
  )
}

// ─── Tiles ──────────────────────────────────────────────────────────────────

function SystemTile({
  status,
  uptime,
  useFahrenheit,
  keepAwakeIdle,
}: {
  status: PiStatus
  uptime: number
  useFahrenheit: boolean
  keepAwakeIdle: boolean
}) {
  const cpuTemp = parseInt(status.cpu_temp)
  return (
    <StatusTile
      icon={<VitalSignsIcon className="h-4 w-4" />}
      halo={getTempHalo(cpuTemp)}
      title="System"
    >
      <Row
        icon={<ScheduleIcon className="h-3.5 w-3.5" />}
        label="Uptime"
        value={formatUptime(uptime)}
      />
      <Row
        icon={<DeviceThermostatIcon className="h-3.5 w-3.5" />}
        label="CPU"
        value={cpuTemp > 0 ? formatTemp(cpuTemp, useFahrenheit) : "N/A"}
        valueColor={cpuTemp > 0 ? getTempColor(cpuTemp) : undefined}
      />
      {status.fan_speed && (
        <Row
          icon={<AirIcon className="h-3.5 w-3.5" />}
          label="Fan"
          value={`${status.fan_speed} RPM`}
        />
      )}
      {/* Three-state: "Connected" needs the host link up ("configured"),
          not just the gadget bound in configfs — a bound gadget with a
          dead link is exactly how the car shows an X while the old
          two-state pill stayed green. udc_state is absent on older
          backends; treat absent as link-unknown and keep the old view.
          Label and color derive from ONE state value so they can't drift. */}
      <Row
        icon={<HardDriveIcon className="h-3.5 w-3.5" />}
        label="USB Drives"
        {...(() => {
          const drivesState =
            status.drives_active !== "yes"
              ? "disconnected"
              : status.udc_state && status.udc_state !== "configured"
                ? "no-link"
                : "connected"
          const pill = {
            disconnected: { value: "Disconnected", valueColor: "#fbbf24" },
            "no-link": { value: "No host link", valueColor: "#f87171" },
            connected: { value: "Connected", valueColor: "oklch(0.82 0.18 150)" },
          } as const
          return pill[drivesState]
        })()}
      />
      {keepAwakeIdle && (
        <Row
          icon={<CardiologyIcon className="h-3.5 w-3.5" />}
          label="Keep Awake"
          value={
            <Link
              to="/settings?tab=Device"
              className="text-blue-400 hover:text-blue-300"
            >
              Off
            </Link>
          }
        />
      )}
    </StatusTile>
  )
}

function NetworkTile({ status }: { status: PiStatus }) {
  const haveWifi = !!status.wifi_ssid
  const haveEth = !!status.ether_speed && status.ether_speed !== "Unknown!"
  const halo: Halo = haveWifi || haveEth ? "accent" : "red"

  return (
    <StatusTile
      icon={haveWifi || haveEth ? <WifiIcon className="h-4 w-4" /> : <WifiOffIcon className="h-4 w-4" />}
      halo={halo}
      title="Network"
    >
      {haveWifi ? (
        <>
          <div className="tile-row">
            <span className="inline-flex text-slate-500">
              <WifiIcon className="h-3.5 w-3.5" />
            </span>
            <span className="text-xs font-medium text-slate-200">
              {status.wifi_ssid}
            </span>
            <span className="ml-auto inline-flex items-center gap-1.5 text-[10px] text-slate-500">
              {status.wifi_signal_dbm != null && (
                <span className="text-slate-400">{status.wifi_signal_dbm} dBm</span>
              )}
              <WifiBars bars={getWifiStrengthBars(status.wifi_strength)} />
            </span>
          </div>
          <div className="tile-row pl-5" style={{ minHeight: 18 }}>
            <span className="text-[10px] text-slate-500">{status.wifi_ip || "No IP"}</span>
            {(status.wifi_rx_bps !== undefined || status.wifi_tx_bps !== undefined) && (
              <>
                <span className="ml-auto text-[10px] text-emerald-400">
                  ↓ {formatThroughput(status.wifi_rx_bps ?? 0)}
                </span>
                <span className="text-[10px] text-slate-500">·</span>
                <span className="text-[10px] text-sky-400">
                  ↑ {formatThroughput(status.wifi_tx_bps ?? 0)}
                </span>
              </>
            )}
          </div>
        </>
      ) : (
        <Row
          icon={<WifiOffIcon className="h-3.5 w-3.5" />}
          label="WiFi"
          sub="Not connected"
        />
      )}

      {haveEth ? (
        <>
          <div className="tile-row">
            <span className="inline-flex text-slate-500">
              <LanIcon className="h-3.5 w-3.5" />
            </span>
            <span className="text-xs font-medium text-slate-200">
              {status.ether_speed}
            </span>
            {status.ether_ip && (
              <span className="ml-auto text-[10px] text-slate-500">
                {status.ether_ip}
              </span>
            )}
          </div>
          {(status.ether_rx_bps !== undefined || status.ether_tx_bps !== undefined) && (
            <div className="tile-row pl-5" style={{ minHeight: 18 }}>
              <span className="text-[10px] text-emerald-400">
                ↓ {formatThroughput(status.ether_rx_bps ?? 0)}
              </span>
              <span className="text-[10px] text-slate-500">·</span>
              <span className="text-[10px] text-sky-400">
                ↑ {formatThroughput(status.ether_tx_bps ?? 0)}
              </span>
            </div>
          )}
        </>
      ) : (
        // Always render an Ethernet row — keeps tile balanced when WiFi is
        // present but ethernet isn't (or vice versa). Muted styling signals
        // disconnected state without taking the tile's halo over.
        <div className="tile-row">
          <span className="inline-flex text-slate-600">
            <LanIcon className="h-3.5 w-3.5" />
          </span>
          <span className="text-xs text-slate-600">Ethernet</span>
          <span className="ml-auto text-[10px] text-slate-600">Not connected</span>
        </div>
      )}
    </StatusTile>
  )
}

function StorageTile({
  status,
  breakdown,
}: {
  status: PiStatus
  breakdown: StorageBreakdown | null
}) {
  const totalSpace = parseInt(status.total_space)
  const freeSpace = parseInt(status.free_space)
  const usedSpace = totalSpace - freeSpace
  const usedPct = totalSpace > 0 ? (usedSpace / totalSpace) * 100 : 0
  const usedPctStr = totalSpace > 0 ? `${Math.round(usedPct)}%` : "0%"
  const snaps = parseInt(status.num_snapshots)

  const segments = breakdown
    ? [
        { label: "Dashcam", size: breakdown.cam_size, color: "#3b82f6" },
        { label: "Music", size: breakdown.music_size, color: "#a855f7" },
        { label: "Lightshow", size: breakdown.lightshow_size, color: "#f59e0b" },
        { label: "Boombox", size: breakdown.boombox_size, color: "#ec4899" },
        { label: "Snapshots", size: breakdown.snapshots_size, color: "#6366f1" },
      ].filter((s) => s.size > 0)
    : []

  return (
    <StatusTile
      icon={<HardDriveIcon className="h-4 w-4" />}
      halo={getStorageHalo(usedPct)}
      title="Storage"
    >
      <div className="flex items-baseline gap-1.5">
        <span className="text-sm font-semibold text-slate-100">
          {formatBytes(usedSpace)}
        </span>
        <span className="text-[11px] text-slate-500">
          / {formatBytes(totalSpace)} · {usedPctStr} used
        </span>
        {/* Reassurance tooltip — high storage usage triggers panic
            for new users ("96% used!"), but Sentry USB rotates
            snapshots automatically as space gets tight. CSS-only
            group-hover so we don't need React state for it.
            Anchored right-0 so the 256px tooltip extends LEFT into
            the card body rather than overflowing off the right
            edge on narrow grid columns. */}
        <span className="group relative inline-flex items-center self-center">
          <InfoIcon
            aria-label="About storage management"
            className="h-3 w-3 cursor-help text-slate-600 transition-colors hover:text-slate-400"
          />
          <span className="pointer-events-none absolute right-0 top-full z-50 mt-2 w-64 rounded-xl border border-white/10 bg-slate-900 p-3 text-[11px] leading-relaxed text-slate-400 opacity-0 shadow-xl transition-opacity group-hover:pointer-events-auto group-hover:opacity-100">
            <span className="absolute bottom-full right-3 block border-4 border-transparent border-b-slate-900" />
            Sentry USB automatically manages your storage. Old
            snapshots are deleted when space is needed — you don't
            need to manually free up space. Low remaining space is
            normal and expected, especially with dashcam footage
            being continuously saved.
          </span>
        </span>
      </div>
      {breakdown && segments.length > 0 ? (
        <>
          <div className="seg-bar">
            {segments.map((s) => (
              <div
                key={s.label}
                style={{
                  width: `${Math.max((s.size / breakdown.total_space) * 100, 0.5)}%`,
                  backgroundColor: s.color,
                }}
                title={`${s.label}: ${formatBytes(s.size)}`}
              />
            ))}
          </div>
          <div className="mt-1 flex flex-wrap gap-x-3 gap-y-1">
            {segments.map((s) => (
              <div key={s.label} className="flex items-center gap-1.5 text-[10px]">
                <span
                  className="inline-block h-1.5 w-1.5 rounded-full"
                  style={{ backgroundColor: s.color }}
                />
                <span className="text-slate-400">{s.label}</span>
                <span className="font-medium text-slate-300">
                  {formatBytes(s.size)}
                </span>
              </div>
            ))}
            <div className="flex items-center gap-1.5 text-[10px]">
              <span className="inline-block h-1.5 w-1.5 rounded-full bg-slate-700" />
              <span className="text-slate-400">Free</span>
              <span className="font-medium text-slate-300">
                {formatBytes(breakdown.free_space)}
              </span>
            </div>
          </div>
        </>
      ) : (
        <div className="bar">
          <div
            className="bg-gradient-to-r from-blue-500 to-blue-400"
            style={{ width: `${usedPct}%` }}
          />
        </div>
      )}
      <TileDivider />
      <Row
        icon={<PhotoCameraIcon className="h-3.5 w-3.5" />}
        label={`${snaps.toLocaleString()} snapshots`}
        sub={
          snaps > 0 &&
          Number.isFinite(parseInt(status.snapshot_oldest)) &&
          Number.isFinite(parseInt(status.snapshot_newest))
            ? `${new Date(
                parseInt(status.snapshot_oldest) * 1000
              ).toLocaleDateString()} → ${new Date(
                parseInt(status.snapshot_newest) * 1000
              ).toLocaleDateString()}`
            : "—"
        }
      />
    </StatusTile>
  )
}

function ActivityTile({
  driveStats,
  archiveProgress,
  processProgress,
  processing,
  metric,
  archiveEta,
  processEta,
}: {
  driveStats: DriveStats | null
  archiveProgress: ProcessProgress | null
  processProgress: ProcessProgress | null
  processing: boolean
  metric: boolean
  archiveEta: string | null
  processEta: string | null
}) {
  // Keep-Awake is rendered as a sub-section inside the Activity card
  // (used to be its own tile next door, but the dead space below
  // Activity made the grid look unbalanced). The hook is only
  // consumed here now.
  //
  // Mirror the old standalone card's behaviour: the inline UI only
  // exposes start/stop affordances, not the Off/Manual/Auto picker
  // (that lives in Settings → Device), so showing it when the user
  // has chosen Off would be a useless dead section. `mode` from the
  // hook is `null` while the preference is still loading and `""`
  // when the user explicitly picked Off — collapse the section in
  // both cases by gating on the active modes only. When mode flips
  // to manual/auto in Settings, this re-renders and the section
  // appears on its own.
  const keepAwake = useKeepAwake()
  const keepAwakeVisible =
    keepAwake.mode === "manual" || keepAwake.mode === "auto"

  const phase = archiveProgress
    ? ("archiving" as const)
    : processing
    ? ("processing" as const)
    : null

  return (
    <div className="relative flex flex-col">
      {/* Phase notification — pinned to the card's top-right corner
          as an absolutely-positioned pill so it doesn't crowd the
          ⚡ ACTIVITY title or the inline FSD link. Only renders
          during an actual archive/process run. */}
      {phase && (
        <div className="pointer-events-none absolute right-2 top-2 z-10">
          <Pill kind={phase === "archiving" ? "accent" : "sky"}>
            <LiveDot /> {phase}
          </Pill>
        </div>
      )}
      <StatusTile
        icon={<BoltIcon className="h-4 w-4" />}
        halo="violet"
        title="Activity"
        className="flex-1"
      >
      {driveStats ? (
        driveStats.processed_count === 0 && driveStats.drives_count === 0 ? (
          <p className="t-xs">
            No drives processed yet. Plug a Sentry USB to ingest dashcam footage.
          </p>
        ) : (
          <>
            <Row
              icon={<MovieIcon className="h-3.5 w-3.5" />}
              label="Clips"
              value={driveStats.processed_count.toLocaleString()}
            />
            <Row
              icon={<LocationOnIcon className="h-3.5 w-3.5" />}
              label="Drives"
              value={driveStats.drives_count.toLocaleString()}
            />
            <Row
              icon={<ConversionPathIcon className="h-3.5 w-3.5" />}
              label="Distance"
              value={`${
                metric
                  ? driveStats.total_distance_km.toFixed(0)
                  : driveStats.total_distance_mi.toFixed(0)
              } ${metric ? "km" : "mi"}`}
            />
            {driveStats.fsd_engaged_ms > 0 && (
              <Row
                icon={<BoltIcon className="h-3.5 w-3.5" />}
                label="FSD"
                value={
                  <Link to="/fsd" className="text-emerald-400 hover:text-emerald-300">
                    {driveStats.fsd_percent}%
                  </Link>
                }
              />
            )}

            {archiveProgress && archiveProgress.total > 0 ? (
              <ProgressBlock
                current={archiveProgress.current}
                total={archiveProgress.total}
                eta={archiveEta}
                color="emerald"
              />
            ) : processProgress && processProgress.total > 0 ? (
              <ProgressBlock
                current={processProgress.current}
                total={processProgress.total}
                eta={processEta}
                color="blue"
              />
            ) : processing ? (
              <div className="bar">
                <div
                  className="w-2/5 animate-pulse bg-gradient-to-r from-blue-500 to-blue-400"
                />
              </div>
            ) : null}
          </>
        )
      ) : (
        <>
          <div className="h-3 w-1/2 animate-pulse rounded bg-slate-800" />
          <div className="h-1.5 w-full animate-pulse rounded-full bg-slate-800" />
        </>
      )}

      {keepAwakeVisible && (
        <>
          <TileDivider />
          <KeepAwakeInline keepAwake={keepAwake} />
        </>
      )}
      </StatusTile>
    </div>
  )
}

function ProgressBlock({
  current,
  total,
  eta,
  color,
}: {
  current: number
  total: number
  eta: string | null
  color: "emerald" | "blue"
}) {
  const pct = (current / total) * 100
  const grad =
    color === "emerald"
      ? "bg-gradient-to-r from-emerald-500 to-emerald-400"
      : "bg-gradient-to-r from-blue-500 to-blue-400"
  return (
    <>
      <div className="flex items-center justify-between text-[10px] text-slate-500 t-num">
        <span>
          {current.toLocaleString()} / {total.toLocaleString()}
          {eta && (
            <span
              className={`ml-1.5 ${
                color === "emerald" ? "text-emerald-400/70" : "text-blue-400/70"
              }`}
            >
              {eta}
            </span>
          )}
        </span>
        <span>{Math.round(pct)}%</span>
      </div>
      <div className="bar">
        <div className={grad} style={{ width: `${pct}%` }} />
      </div>
    </>
  )
}

const KEEP_AWAKE_DURATIONS = [
  { label: "15m", value: 15 },
  { label: "30m", value: 30 },
  { label: "1h", value: 60 },
  { label: "2h", value: 120 },
]

/**
 * Keep-Awake sub-section rendered inline inside the Activity tile
 * (below the clips/drives/distance stats row and a tile divider).
 * Same visual + behavioural state machine as the old standalone
 * KeepAwakeTile: animated icon when active/pending, value reflects
 * remaining time or mode state, action button is Start (with
 * duration dropdown) when manual + idle, Stop when active/pending,
 * nothing otherwise.
 *
 * Caller (ActivityTile) gates rendering on `mode != null` so this
 * component can assume the feature is configured.
 */
function KeepAwakeInline({ keepAwake }: { keepAwake: ReturnType<typeof useKeepAwake> }) {
  const { status, mode, start, stop } = keepAwake
  const [showDurations, setShowDurations] = useState(false)

  const isActive = status.state === "active"
  const isPending = status.state === "pending"
  const isIdle = status.state === "idle"
  const remainingMin = status.remaining_sec ? Math.ceil(status.remaining_sec / 60) : 0

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

  const iconColor = isActive
    ? "text-rose-400"
    : isPending
    ? "text-amber-400"
    : "text-blue-400"

  const actionBtn =
    mode === "manual" && isIdle ? (
      <div className="relative">
        <button
          onClick={() => setShowDurations(!showDurations)}
          className="rounded-lg bg-blue-500/20 px-2.5 py-1 text-[11px] font-medium text-blue-400 transition-colors hover:bg-blue-500/30"
        >
          Start
        </button>
        {showDurations && (
          <div className="absolute right-0 top-full z-10 mt-1 w-28 rounded-lg border border-white/10 bg-slate-900 p-1 shadow-xl">
            {KEEP_AWAKE_DURATIONS.map((opt) => (
              <button
                key={opt.value}
                onClick={() => {
                  start(opt.value)
                  setShowDurations(false)
                }}
                className="w-full rounded-md px-3 py-1.5 text-left text-xs text-slate-300 hover:bg-white/5"
              >
                {opt.label}
              </button>
            ))}
          </div>
        )}
      </div>
    ) : isActive || isPending ? (
      <button
        onClick={stop}
        className="rounded-lg bg-red-500/15 px-2.5 py-1 text-[11px] font-medium text-red-400 transition-colors hover:bg-red-500/25"
      >
        Stop
      </button>
    ) : null

  return (
    <div>
      <div className="flex items-center gap-2">
        <span className={`inline-flex ${iconColor}`}>
          {isActive ? (
            <CardiologyIcon className="h-3.5 w-3.5 animate-pulse" />
          ) : isPending ? (
            <TimerIcon className="h-3.5 w-3.5 animate-pulse" />
          ) : (
            <CardiologyIcon className="h-3.5 w-3.5" />
          )}
        </span>
        <span className="text-[10px] font-semibold uppercase tracking-wider text-slate-500">
          Keep Awake
        </span>
        {actionBtn && <span className="ml-auto">{actionBtn}</span>}
      </div>
      <div className="mt-1 flex items-baseline gap-2">
        <span className="text-base font-semibold text-slate-100">{value}</span>
      </div>
      <p className="t-xs">{sub}</p>
    </div>
  )
}

function AwayModeTile() {
  const { status } = useAwayMode()
  const remaining = status.remaining_sec ?? 0
  const h = Math.floor(remaining / 3600)
  const m = Math.floor((remaining % 3600) / 60)

  let totalSec = 0
  if (status.enabled_at && status.expires_at) {
    totalSec =
      (new Date(status.expires_at).getTime() -
        new Date(status.enabled_at).getTime()) /
      1000
  }
  const pct = totalSec > 0 ? ((totalSec - remaining) / totalSec) * 100 : 0

  return (
    <StatusTile
      icon={<WifiIcon className="h-4 w-4" />}
      halo="blue"
      title="Away Mode"
      badge={
        <Pill kind="sky">
          <LiveDot /> Active
        </Pill>
      }
    >
      <div className="flex items-baseline gap-1.5">
        <span className="text-lg font-semibold text-slate-100">
          {h}h {m}m
        </span>
        <span className="t-xs">remaining</span>
      </div>
      <div className="bar">
        <div className="bg-sky-400" style={{ width: `${pct}%` }} />
      </div>
      {status.ap_ssid && (
        <p className="t-xs">
          AP <span className="t-mono text-slate-300">{status.ap_ssid}</span>
        </p>
      )}
    </StatusTile>
  )
}
