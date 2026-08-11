import { Suspense, lazy, useCallback, useEffect, useMemo, useState } from "react"
import { Link, useParams } from "react-router-dom"
import {
  AirIcon,
  AlbumIcon,
  ArrowBackIcon,
  AutoAwesomeIcon,
  DataUsageIcon,
  DeviceThermostatIcon,
  LineEndIcon,
  LineStartIcon,
  ProgressActivityIcon,
  ScheduleIcon,
  SensorsIcon,
  SpeedIcon,
} from "@/components/icons"
import { cn } from "@/lib/utils"
import { monotonicTrack } from "@/lib/drive-track"
import { useDriveDetail } from "@/hooks/useDriveDetail"
import { useUnits } from "@/lib/units"
import { ScrubberProvider, useScrubberActions } from "@/hooks/useScrubberSync"
import {
  formatDistance,
  formatDuration,
  formatHvacRuntime,
  formatOdometer,
  formatPercent,
  formatPsi,
  formatSpeed,
  formatTempC,
} from "@/lib/drive-format"
import type { DriveDetail as DriveDetailType } from "@/types/drives"
import { DriveMap } from "@/components/drives/DriveMap"
import { DriveScrubber } from "@/components/drives/DriveScrubber"
import { BatteryLevelIcon } from "@/components/drives/BatteryLevelIcon"
import { DualPinBlock } from "@/components/drives/DualPinBlock"
import { SectionHeading, StatTile } from "@/components/drives/StatTile"
import { TagPopover } from "@/components/drives/TagPopover"
import type { TemperaturePoint } from "@/components/drives/TemperatureChart"
import type { BatteryPoint } from "@/components/drives/DriveMap"

const DriveChart = lazy(() => import("@/components/drives/DriveChart"))
const TemperatureChart = lazy(
  () => import("@/components/drives/TemperatureChart"),
)

export default function DriveDetail() {
  const { id } = useParams<{ id: string }>()
  const { drive, loading, error, saveTags } = useDriveDetail(id)

  return (
    <div className="mx-auto w-full max-w-6xl px-4 py-6 sm:px-6 sm:py-8">
      <Link
        to="/drives"
        className="mb-4 inline-flex items-center gap-2 text-sm text-slate-400 hover:text-slate-200"
      >
        <ArrowBackIcon className="h-4 w-4" />
        Back to drives
      </Link>

      {loading && (
        <div className="flex items-center justify-center gap-2 rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-sm text-slate-400">
          <ProgressActivityIcon className="h-4 w-4 animate-spin" /> Loading drive…
        </div>
      )}
      {error && !loading && (
        <div className="rounded-2xl border border-rose-400/30 bg-rose-500/5 p-6 text-sm text-rose-200">
          Failed to load drive: {error}
        </div>
      )}
      {drive && (
        <ScrubberProvider>
          <DriveDetailContent drive={drive} onSaveTags={saveTags} />
        </ScrubberProvider>
      )}
    </div>
  )
}

interface DriveDetailContentProps {
  drive: DriveDetailType
  onSaveTags: (tags: string[]) => Promise<void>
}

function DriveDetailContent({ drive, onSaveTags }: DriveDetailContentProps) {
  const { setTotal } = useScrubberActions()
  // Distance/speed/temperature unit, sourced from setup config
  // (DRIVE_MAP_UNIT). Default to imperial — same default as the wizard
  // and as Drives.tsx so the first paint never shows an unintended
  // unit. The pattern mirrors Drives.tsx / Dashboard.tsx.
  const [metric, setMetric] = useState(false)
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
  const [showFsdEvents, setShowFsdEvents] = useState(true)
  const hasFsdEvents = (drive.fsdEvents ?? []).length > 0

  // Per-sample battery time-series for the playback info card on the
  // map. Sourced from /api/drives/{id}/battery-series — the BLE
  // sampler polls every 60s, so a typical drive has dozens of samples.
  // DriveMap looks up the most recent sample at or before the current
  // scrubber's wall-clock time on every tick; that's a step function,
  // not interpolation, because battery moves in discrete 1% increments.
  // Empty array (or skipped fetch) → card omits the battery row.
  const [batterySeries, setBatterySeries] = useState<BatteryPoint[]>([])
  useEffect(() => {
    let cancelled = false
    fetch(`/api/drives/${drive.id}/battery-series`)
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((data: { points?: BatteryPoint[] }) => {
        if (cancelled) return
        setBatterySeries(Array.isArray(data.points) ? data.points : [])
      })
      .catch(() => {
        if (!cancelled) setBatterySeries([])
      })
    return () => {
      cancelled = true
    }
  }, [drive.id])

  // Tesla can stitch overlapping/duplicate same-minute clips into one drive,
  // which makes a sample's synthesized time jump backward at the seam even
  // though the car kept moving. monotonicTrack repairs the clock in place
  // (keeping every real position) so EVERY consumer (map, scrubber, speed
  // chart) shares one clean, geographically continuous, strictly time-
  // increasing, index-aligned sequence. See web/src/lib/drive-track.ts for the
  // why — the old "drop backward samples" approach silently deleted over a mile
  // of real road at the seam, leaving a false gap on the map. A normal
  // contiguous drive passes through unchanged.
  const { monoPoints, monoFsdStates } = useMemo(() => {
    const { points, fsdStates } = monotonicTrack(drive.points, drive.fsdStates)
    return { monoPoints: points, monoFsdStates: fsdStates }
  }, [drive.points, drive.fsdStates])

  useEffect(() => {
    setTotal(monoPoints.length)
  }, [monoPoints.length, setTotal])

  const title = drive.endLocation ?? "Drive"
  // Only the "imported from Tessie" badge is informative — drives from
  // the USB are the default and don't need a redundant label.
  const showTessieBadge = drive.source === "tessie"

  // Speed chart input — must be MONOTONIC in `time` for Recharts'
  // type="monotone" Area, otherwise duplicate or backward-going X
  // values trigger the cubic interpolator to draw little
  // triangle/loop artifacts on the chart (one drive in ~10 has them
  // — typically caused by two GPS samples at the same relative-ms
  // stamp during slow / stopped sections, or by an NTP correction
  // mid-drive nudging clocks backward by a few ms).
  //
  // We:
  //  1) keep the original `index` so chart hover/click still maps
  //     back to the right scrubber position,
  //  2) filter out non-finite values that would NaN-poison the
  //     interpolator,
  //  3) sort by time, then collapse duplicate times to keep the
  //     last value (most recent reading wins — matches what the
  //     scrubber would land on).
  const speedSeries = useMemo(() => {
    const raw = monoPoints
      .map((p, i) => ({
        index: i,
        time: p[2],
        value: metric ? p[3] * 3.6 : p[3] * 2.23694,
      }))
      .filter(
        (pt) => Number.isFinite(pt.time) && Number.isFinite(pt.value),
      )
    raw.sort((a, b) => a.time - b.time)
    // De-dupe consecutive identical times — overwrites with the
    // last sample so the chart line ends at the same point the
    // scrubber would have shown.
    const out: typeof raw = []
    for (const pt of raw) {
      const prev = out[out.length - 1]
      if (prev && prev.time === pt.time) {
        out[out.length - 1] = pt
      } else {
        out.push(pt)
      }
    }
    return out
  }, [monoPoints, metric])

  const speedUnit = metric ? "km/h" : "mph"
  const fsdFull = drive.fsdPercent >= 100

  return (
    <>
      <div className="flex items-start justify-between gap-3">
        <h1 className="text-2xl font-semibold text-slate-100 sm:text-3xl">
          {drive.summon ? "Summon to" : "Drive to"} {title}
        </h1>
        {showTessieBadge && (
          <span className="mt-1 inline-flex shrink-0 items-center rounded-full bg-violet-500/15 px-2 py-0.5 text-xs font-medium text-violet-200 ring-1 ring-inset ring-violet-400/30">
            Tessie
          </span>
        )}
      </div>

      {hasFsdEvents && (
        <div className="mt-4 flex justify-end">
          <button
            type="button"
            onClick={() => setShowFsdEvents((s) => !s)}
            className={cn(
              "inline-flex items-center gap-1.5 rounded-full border px-3 py-1 text-xs font-medium transition-colors",
              showFsdEvents
                ? "border-emerald-400/40 bg-emerald-400/10 text-emerald-200 hover:bg-emerald-400/20"
                : "border-white/10 bg-white/[0.03] text-slate-300 hover:bg-white/[0.06]",
            )}
            aria-pressed={showFsdEvents}
          >
            <AutoAwesomeIcon className="h-3.5 w-3.5" />
            Toggle FSD Markers
          </button>
        </div>
      )}

      <div className={hasFsdEvents ? "mt-2" : "mt-4"}>
        <div className="relative">
          <DriveMap
            points={monoPoints}
            fsdStates={monoFsdStates}
            fsdEvents={drive.fsdEvents}
            showEvents={showFsdEvents}
            source={drive.source}
            startTime={drive.startTime}
            metric={metric}
            batterySeries={batterySeries}
          />
          {/* Drive tag chip floats over the bottom-left of the map.
              Click to open the popover; when no tags, shows just a
              tag icon. */}
          <div className="absolute bottom-3 left-3 z-[400]">
            <TagPopover tags={drive.tags ?? []} onChange={onSaveTags} />
          </div>
        </div>
        {/* DriveScrubber now renders the FSD engagement overlay on its
            own track — the standalone FsdEngagementStripe is retired. */}
        <DriveScrubber
          points={monoPoints}
          startTime={drive.startTime}
          fsdStates={monoFsdStates}
        />
      </div>

      <div className="mt-6">
        <DualPinBlock
          origin={{
            label: drive.startLocation ?? "Unknown origin",
            batteryPct: drive.batteryPctStart,
            timestamp: drive.startTime,
          }}
          destination={{
            label: drive.endLocation ?? "Unknown destination",
            batteryPct: drive.batteryPctEnd,
            timestamp: drive.endTime,
          }}
          size="detail"
        />
      </div>

      <div className="mt-6 grid grid-cols-1 gap-4 sm:grid-cols-3">
        <StatTile
          label="Distance"
          value={formatDistance(drive.distanceMi, drive.distanceKm, metric)}
          icon={<SpeedIcon className="h-4 w-4" />}
          size="headline"
        />
        {drive.summon ? (
          // Summon drives run driverless with autopilot_state unset, so
          // "0% self-driving" would be misleading — mirror the list
          // row's Summon pill instead.
          <StatTile
            label="Self-driving"
            value="Summon"
            icon={<SensorsIcon className="h-4 w-4" />}
            info="The car drove itself via Summon. FSD % doesn't apply — summon telemetry reports no autopilot state."
            size="headline"
          />
        ) : (
          <StatTile
            label="Self-driving"
            value={`${formatPercent(drive.fsdPercent)}%`}
            icon={<AutoAwesomeIcon className="h-4 w-4" />}
            star={fsdFull}
            info="Percentage of the drive's distance with FSD engaged."
            size="headline"
          />
        )}
        <StatTile
          label="Duration"
          value={formatDuration(drive.durationMs)}
          icon={<ScheduleIcon className="h-4 w-4" />}
          size="headline"
        />
      </div>

      <SpeedSection
        drive={drive}
        speedSeries={speedSeries}
        metric={metric}
        speedUnit={speedUnit}
      />
      <AssistedSection drive={drive} metric={metric} />
      <OdometerSection drive={drive} metric={metric} />
      <BatterySection drive={drive} />
      <ClimateSection drive={drive} metric={metric} />
      <TirePressureSection drive={drive} />
    </>
  )
}

interface SpeedSectionProps {
  drive: DriveDetailType
  speedSeries: { index: number; time: number; value: number }[]
  metric: boolean
  speedUnit: string
}

function SpeedSection({ drive, speedSeries, metric, speedUnit }: SpeedSectionProps) {
  // Stable formatter ref so the memoized DriveChart skips re-rendering on
  // scrubber/playback ticks that don't change the speed data.
  const formatSpeedValue = useCallback(
    (n: number) => `${Math.round(n)} ${speedUnit}`,
    [speedUnit],
  )
  if (drive.maxSpeedMph === 0 && drive.avgSpeedMph === 0) return null
  return (
    <>
      <SectionHeading>Speed</SectionHeading>
      <div className="grid grid-cols-2 gap-4">
        <StatTile
          label="Avg speed"
          value={formatSpeed(drive.avgSpeedMph, drive.avgSpeedKmh, metric)}
          icon={<SpeedIcon className="h-4 w-4" />}
        />
        <StatTile
          label="Max speed"
          value={formatSpeed(drive.maxSpeedMph, drive.maxSpeedKmh, metric)}
          icon={<SpeedIcon className="h-4 w-4" />}
        />
      </div>
      {speedSeries.length > 1 && (
        <div className="mt-4 rounded-2xl border border-white/[0.06] bg-white/[0.02] p-3">
          <Suspense fallback={<ChartFallback />}>
            <DriveChart
              series={speedSeries}
              valueLabel="Speed"
              valueFormatter={formatSpeedValue}
              startTime={drive.startTime}
            />
          </Suspense>
        </div>
      )}
    </>
  )
}

function ChartFallback() {
  return (
    <div className="flex h-44 items-center justify-center text-xs text-slate-500">
      <ProgressActivityIcon className="mr-2 h-3 w-3 animate-spin" /> Loading chart…
    </div>
  )
}

interface AssistedSectionProps {
  drive: DriveDetailType
  metric: boolean
}

function AssistedSection({ drive, metric }: AssistedSectionProps) {
  // Each assist system contributes a pair of tiles (% and distance) and
  // only shows up when it was actually engaged during the drive. Drives
  // that used only FSD don't waste rows on Autopilot/TACC zeroes.
  const showFsd =
    drive.fsdPercent > 0 ||
    drive.fsdDistanceMi > 0 ||
    drive.fsdDisengagements > 0 ||
    drive.fsdAccelPushes > 0
  const showAutopilot = drive.autosteerPercent > 0 || drive.autosteerDistanceMi > 0
  const showTacc = drive.taccPercent > 0 || drive.taccDistanceMi > 0
  if (!showFsd && !showAutopilot && !showTacc) return null
  return (
    <>
      <SectionHeading>Assisted driving</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
        {showFsd && (
          <>
            <StatTile
              label="FSD"
              value={`${formatPercent(drive.fsdPercent)}%`}
              icon={<AutoAwesomeIcon className="h-4 w-4" />}
              info="Time + distance share with Full Self-Driving (Supervised) engaged."
            />
            <StatTile
              label="FSD distance"
              value={formatDistance(drive.fsdDistanceMi, drive.fsdDistanceKm, metric)}
              icon={<SpeedIcon className="h-4 w-4" />}
            />
            <StatTile
              label="Disengagements"
              value={String(drive.fsdDisengagements)}
              icon={<AlbumIcon className="h-4 w-4" />}
              info="Number of times FSD handed back control (excluding parks within 2s)."
            />
            <StatTile
              label="Accel pushes"
              value={String(drive.fsdAccelPushes)}
              icon={<SpeedIcon className="h-4 w-4" />}
              info="Number of accelerator presses while FSD was engaged."
            />
          </>
        )}
        {showAutopilot && (
          <>
            <StatTile
              label="Autopilot"
              value={`${formatPercent(drive.autosteerPercent)}%`}
              icon={<AutoAwesomeIcon className="h-4 w-4" />}
              info="Autosteer share (lane-keeping without FSD)."
            />
            <StatTile
              label="Autopilot distance"
              value={formatDistance(drive.autosteerDistanceMi, drive.autosteerDistanceKm, metric)}
              icon={<SpeedIcon className="h-4 w-4" />}
            />
          </>
        )}
        {showTacc && (
          <>
            <StatTile
              label="TACC"
              value={`${formatPercent(drive.taccPercent)}%`}
              icon={<AutoAwesomeIcon className="h-4 w-4" />}
              info="Traffic-Aware Cruise Control share (speed regulation only)."
            />
            <StatTile
              label="TACC distance"
              value={formatDistance(drive.taccDistanceMi, drive.taccDistanceKm, metric)}
              icon={<SpeedIcon className="h-4 w-4" />}
            />
          </>
        )}
      </div>
    </>
  )
}

function OdometerSection({
  drive,
  metric,
}: {
  drive: DriveDetailType
  metric: boolean
}) {
  if (drive.odometerMiStart === undefined && drive.odometerMiEnd === undefined) return null
  return (
    <>
      <SectionHeading>Odometer</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
        <StatTile
          label="Start"
          value={
            drive.odometerMiStart !== undefined
              ? formatOdometer(drive.odometerMiStart, metric)
              : "—"
          }
          icon={<LineStartIcon className="h-4 w-4" />}
        />
        <StatTile
          label="End"
          value={
            drive.odometerMiEnd !== undefined
              ? formatOdometer(drive.odometerMiEnd, metric)
              : "—"
          }
          icon={<LineEndIcon className="h-4 w-4" />}
        />
        <StatTile
          label="Driven"
          value={
            drive.odometerMiDriven !== undefined
              ? formatOdometer(drive.odometerMiDriven, metric)
              : "—"
          }
          icon={<SpeedIcon className="h-4 w-4" />}
        />
      </div>
    </>
  )
}

function BatterySection({ drive }: { drive: DriveDetailType }) {
  if (drive.batteryPctStart === undefined && drive.batteryPctEnd === undefined) return null
  return (
    <>
      <SectionHeading>Battery</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
        <StatTile
          label="Start"
          value={drive.batteryPctStart !== undefined ? `${Math.round(drive.batteryPctStart)}%` : "—"}
          icon={<BatteryLevelIcon pct={drive.batteryPctStart} />}
        />
        <StatTile
          label="End"
          value={drive.batteryPctEnd !== undefined ? `${Math.round(drive.batteryPctEnd)}%` : "—"}
          icon={<BatteryLevelIcon pct={drive.batteryPctEnd} />}
        />
        <StatTile
          label="Used"
          value={drive.batteryPctUsed !== undefined ? `${drive.batteryPctUsed.toFixed(1)}%` : "—"}
          icon={<DataUsageIcon className="h-4 w-4" />}
        />
      </div>
    </>
  )
}

interface ClimateSectionProps {
  drive: DriveDetailType
  metric: boolean
}

function ClimateSection({ drive, metric }: ClimateSectionProps) {
  const anyClimate =
    drive.interiorTempMinC !== undefined ||
    drive.interiorTempMaxC !== undefined ||
    drive.exteriorTempAvgC !== undefined ||
    drive.hvacRuntimeS !== undefined

  // Per-sample temperature series for the chart. Fetched only when
  // the section will actually render, so drives without climate data
  // never trigger the request. The endpoint is cheap (one indexed
  // SELECT bounded by the drive's clip window) but skipping the
  // round-trip when there's nothing to show keeps the network panel
  // clean.
  const [tempPoints, setTempPoints] = useState<TemperaturePoint[] | null>(null)
  useEffect(() => {
    if (!anyClimate) return
    let cancelled = false
    fetch(`/api/drives/${drive.id}/temperature-series`)
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`HTTP ${r.status}`))))
      .then((data: { points?: TemperaturePoint[] }) => {
        if (cancelled) return
        setTempPoints(Array.isArray(data.points) ? data.points : [])
      })
      .catch(() => {
        if (!cancelled) setTempPoints([])
      })
    return () => {
      cancelled = true
    }
  }, [drive.id, anyClimate])

  if (!anyClimate) return null

  // Decide whether the chart has enough material to render. A single
  // sample produces a degenerate flat line that's worse than just the
  // min/max/avg tiles above it.
  const chartReady = tempPoints !== null && tempPoints.length >= 2

  return (
    <>
      <SectionHeading>Climate</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-4">
        <StatTile
          label="Interior min"
          value={formatTempC(drive.interiorTempMinC, metric)}
          icon={<DeviceThermostatIcon className="h-4 w-4" />}
        />
        <StatTile
          label="Interior max"
          value={formatTempC(drive.interiorTempMaxC, metric)}
          icon={<DeviceThermostatIcon className="h-4 w-4" />}
        />
        <StatTile
          label="Exterior avg"
          value={formatTempC(drive.exteriorTempAvgC, metric)}
          icon={<DeviceThermostatIcon className="h-4 w-4" />}
        />
        <StatTile
          label="HVAC runtime"
          value={
            drive.hvacRuntimeS !== undefined
              ? formatHvacRuntime(drive.hvacRuntimeS, drive.durationMs)
              : "—"
          }
          icon={<AirIcon className="h-4 w-4" />}
        />
      </div>
      {chartReady && (
        <div className="mt-4 rounded-2xl border border-white/[0.06] bg-white/[0.02] p-3">
          <Suspense fallback={<ChartFallback />}>
            <TemperatureChart
              points={tempPoints!}
              metric={metric}
              drivePoints={drive.points}
              startTime={drive.startTime}
            />
          </Suspense>
        </div>
      )}
    </>
  )
}

function TirePressureSection({ drive }: { drive: DriveDetailType }) {
  // Pressure has its own unit key (PRESSURE_UNIT) independent of the page's
  // DRIVE_MAP_UNIT-derived `metric` — read it from the shared units store so
  // this section stays in sync with the dashboard's TirePressureCard.
  const { pressureBar } = useUnits()
  const any =
    drive.tireFlPsi !== undefined ||
    drive.tireFrPsi !== undefined ||
    drive.tireRlPsi !== undefined ||
    drive.tireRrPsi !== undefined
  if (!any) return null
  return (
    <>
      <SectionHeading>Tire pressure</SectionHeading>
      <div className="grid grid-cols-2 gap-4">
        <StatTile label="FL" value={formatPsi(drive.tireFlPsi, pressureBar)} icon={<AlbumIcon className="h-4 w-4" />} />
        <StatTile label="FR" value={formatPsi(drive.tireFrPsi, pressureBar)} icon={<AlbumIcon className="h-4 w-4" />} />
        <StatTile label="RL" value={formatPsi(drive.tireRlPsi, pressureBar)} icon={<AlbumIcon className="h-4 w-4" />} />
        <StatTile label="RR" value={formatPsi(drive.tireRrPsi, pressureBar)} icon={<AlbumIcon className="h-4 w-4" />} />
      </div>
    </>
  )
}

