import { Suspense, lazy, useEffect, useMemo, useState } from "react"
import { Link } from "react-router-dom"
import {
  AlbumIcon,
  BatteryAndroidFrameBoltIcon,
  ChevronRightIcon,
  DeviceThermostatIcon,
  DirectionsCarIcon,
  ExpandLessIcon,
  ExpandMoreIcon,
  MusicNoteIcon,
  WarningIcon,
} from "@/components/icons"
import { BatteryLevelIcon } from "@/components/drives/BatteryLevelIcon"
import { sendChargingAction } from "@/api/charging"
import type { TireHistoryResponse } from "./TirePressureCard"
import type { CurrentCharge } from "@/types/charging"
import {
  fmtChargeRateUnit,
  fmtCurrent,
  fmtEnergy,
  fmtRangeUnit,
  fmtToFull,
  fmtVoltage,
} from "@/lib/charge-format"
import {
  deriveVehicleStatusLabel,
  presentBleHealth,
  shouldShowChargingControls,
  type BleHealth,
} from "@/lib/bleHealth"

// Lazy-load the chart only when the user expands the Tires chip —
// recharts (380 KB) stays out of the dashboard's initial bundle for
// users who only glance at the summary.
const TirePressureCard = lazy(() =>
  import("./TirePressureCard").then((m) => ({ default: m.TirePressureCard })),
)

export interface CarStatusSample {
  ts: number | null
  // Age of the envelope (most recent state poll), in seconds.
  seconds_ago?: number | null
  // Live gear: "Park" / "Drive" / "Reverse" / "Neutral" / "Unknown".
  shift_state?: string | null
  battery_pct?: number | null
  interior_temp_c?: number | null
  exterior_temp_c?: number | null
  tire_fl_psi?: number | null
  tire_fr_psi?: number | null
  tire_rl_psi?: number | null
  tire_rr_psi?: number | null
  // Per-field age (seconds) of the shown value. A field can be far older
  // than `seconds_ago` when its poll has been failing while other polls
  // keep the envelope fresh — that's what made a stale temp read as
  // "updated 10s ago". Surfaced so the chip can flag it.
  field_secs_ago?: {
    battery_pct?: number | null
    interior_temp_c?: number | null
    exterior_temp_c?: number | null
    tires?: number | null
  } | null
}

interface CarStatusCardProps {
  sample: CarStatusSample | null
  // Shared backend health keeps the dashboard aligned with Settings.
  // A rejected key is red; sleep, contention, and stale data are yellow.
  bleHealth?: BleHealth | null
  // ISO end-time of the most recent drive — used to derive
  // "Parked Xh Ym". When the value is null the duration row is
  // hidden (no drives recorded yet).
  latestDriveEnd: string | null
  // Tire history for the expandable chart. Pass undefined to hide
  // the Tires chip's expand affordance entirely (e.g. no telemetry).
  tireHistory?: TireHistoryResponse
  useFahrenheit: boolean
  // Distance unit for the battery drop-down's range row (true = km).
  metric: boolean
  // Live charge status. When the car is charging the Battery chip turns
  // green and pulses; expanding it shows range, time-to-full and power.
  // null/undefined hides the chip's expand affordance.
  currentCharge?: CurrentCharge | null
  // Name of the currently-active lock-chime sound, if the feature
  // is configured. null/undefined hides the indicator entirely so
  // users who don't use lock chimes don't see a confusing chip.
  lockChimeName?: string | null
}

type TireStatus =
  | { kind: "optimal"; label: string; color: string }
  | { kind: "check"; label: string; color: string }
  | { kind: "unsafe"; label: string; color: string }
  | { kind: "none"; label: string; color: string }

function deriveTireStatus(sample: CarStatusSample | null): TireStatus {
  if (!sample) return { kind: "none", label: "—", color: "text-slate-500" }
  const values = [
    sample.tire_fl_psi,
    sample.tire_fr_psi,
    sample.tire_rl_psi,
    sample.tire_rr_psi,
  ].filter((v): v is number => typeof v === "number")
  if (values.length === 0) {
    return { kind: "none", label: "—", color: "text-slate-500" }
  }
  // Mirrors the zone thresholds the chart uses: optimal 36–45,
  // warning bands 28–36 and 45–50, unsafe outside that.
  const anyUnsafe = values.some((v) => v < 28 || v > 50)
  if (anyUnsafe) {
    return { kind: "unsafe", label: "Unsafe", color: "text-rose-400" }
  }
  const anyWarn = values.some((v) => v < 36 || v > 45)
  if (anyWarn) {
    return { kind: "check", label: "Check tires", color: "text-amber-400" }
  }
  return { kind: "optimal", label: "Optimal", color: "text-emerald-400" }
}

function formatDuration(ms: number): string {
  const totalMin = Math.max(0, Math.floor(ms / 60_000))
  const d = Math.floor(totalMin / (60 * 24))
  const h = Math.floor((totalMin - d * 60 * 24) / 60)
  const m = totalMin - d * 24 * 60 - h * 60
  if (d > 0) return `${d}d ${h}h`
  if (h > 0) return `${h}h ${m}m`
  return `${m}m`
}

function formatTemp(c: number | null | undefined, useFahrenheit: boolean): string {
  if (c === null || c === undefined) return "—"
  const value = useFahrenheit ? (c * 9) / 5 + 32 : c
  const unit = useFahrenheit ? "°F" : "°C"
  return `${Math.round(value)}${unit}`
}

// A shown value older than this is flagged as stale next to the chip, so
// a last-known reading (car asleep, or a field's poll failing while the
// envelope stays fresh) can't masquerade as current. 10 min is well past
// the ~15-30s active poll cadence, so live values are never flagged.
const STALE_AFTER_SECS = 600

// Compact relative age ("4m", "2h", "3d") for a stale-value hint.
function formatAge(secs: number): string {
  if (secs < 90) return `${Math.max(1, Math.round(secs))}s`
  const m = Math.round(secs / 60)
  if (m < 90) return `${m}m`
  const h = Math.round(m / 60)
  if (h < 36) return `${h}h`
  return `${Math.round(h / 24)}d`
}

// Returns "· 2h ago" when the field is older than the threshold, else null.
function staleHint(secs: number | null | undefined): string | null {
  if (secs == null || secs < STALE_AFTER_SECS) return null
  return `${formatAge(secs)} ago`
}

/**
 * Top-of-dashboard car-status overview. Replaces the old
 * stand-alone tire-pressure card with a single tile that shows the
 * last-known summary (parked duration, battery, cabin/ambient
 * temps, tire-health verdict) and reveals the tire-pressure history
 * chart inline when the user clicks the Tires chip.
 *
 * The chart bundle is lazy-loaded — clicking Tires is what pulls it
 * in, so users who never expand it pay zero recharts cost.
 */
export function CarStatusCard({
  sample,
  bleHealth,
  latestDriveEnd,
  tireHistory,
  useFahrenheit,
  metric,
  currentCharge,
  lockChimeName,
}: CarStatusCardProps) {
  const [tiresOpen, setTiresOpen] = useState(false)
  const [batteryOpen, setBatteryOpen] = useState(false)
  // Now tick — drives the parked-duration counter forward without
  // Fifteen-second cadence also expires vehicle controls promptly when
  // the browser cannot reach the Pi. Date.now() stays outside render.
  const [nowMs, setNowMs] = useState(() => Date.now())
  useEffect(() => {
    const id = setInterval(() => setNowMs(Date.now()), 15_000)
    return () => clearInterval(id)
  }, [])

  // Live gear decides Parked vs Driving. Gate on freshness so a stale
  // Drive sample from before the car slept doesn't read "Driving".
  const isDriving =
    (sample?.shift_state === "Drive" ||
      sample?.shift_state === "Reverse" ||
      sample?.shift_state === "Neutral") &&
    sample?.seconds_ago != null &&
    sample.seconds_ago >= 0 &&
    sample.seconds_ago <= 120
  const healthPresentation = presentBleHealth(
    bleHealth,
    sample?.seconds_ago ?? null,
  )
  const showHealthWarning = healthPresentation.severity !== "green"
  // A persisted charge phase may outlive the Pi's connection to the car.
  // Only present it as live charging while authenticated BLE health is green.
  const charging =
    healthPresentation.severity === "green" && !!currentCharge?.charging
  const showChargingControls = shouldShowChargingControls(
    healthPresentation,
    currentCharge?.controlsAvailable === true,
    currentCharge?.controlsValidUntilTs ?? null,
    Math.floor(nowMs / 1_000),
  )
  const statusLabel = deriveVehicleStatusLabel(
    healthPresentation,
    isDriving,
    charging,
  )
  const statusHalo = healthPresentation.severity === "red"
    ? "halo-red"
    : healthPresentation.severity === "yellow"
      ? "halo-amber"
      : "halo-accent"
  const statusColor = healthPresentation.severity === "red"
    ? "text-rose-300"
    : healthPresentation.severity === "yellow"
      ? "text-amber-300"
      : "text-slate-100"

  // Derived parked duration. We treat "latest drive ended in the
  // past" as the parked-since timestamp; if there's no recorded
  // drive yet we just show the state badge without a duration.
  const parkedDuration = useMemo(() => {
    if (isDriving) return null
    if (!latestDriveEnd) return null
    const t = new Date(latestDriveEnd).getTime()
    if (!Number.isFinite(t)) return null
    const delta = nowMs - t
    if (delta < 60_000) return null
    return formatDuration(delta)
  }, [isDriving, latestDriveEnd, nowMs])

  const tireStatus = deriveTireStatus(sample)
  const haveTireData =
    !!tireHistory && tireHistory.points.length > 0 && tireStatus.kind !== "none"

  // Prefer the live charge SoC over the last BLE sample's battery_pct.
  const batterySoc = currentCharge?.soc ?? sample?.battery_pct
  const haveChargeDetail =
    currentCharge != null &&
    (currentCharge.charging ||
      currentCharge.rangeMi != null ||
      currentCharge.controlsAvailable)

  return (
    <div className="glass-card relative p-4">
      {/* Lock-chime chip pinned to the card's actual top-right
          corner via absolute positioning, so it sits in the corner
          regardless of the Parked row's height. Only renders when
          a chime is active so users without the feature don't see
          an empty placeholder. Click → /community?view=chimes
          which lands directly on the lock-chime tab inside
          Community (the LockChime page is mounted as a sub-view
          of Community, not its own route). */}
      {lockChimeName && (
        <Link
          to="/community?view=chimes"
          title={`Active lock chime: ${lockChimeName}`}
          className="absolute right-3 top-3 inline-flex max-w-[120px] items-center gap-1.5 rounded-full border border-emerald-400/25 bg-emerald-500/10 px-2.5 py-1 text-[11px] font-medium text-emerald-300 transition-colors hover:bg-emerald-500/15 sm:max-w-[180px]"
        >
          <MusicNoteIcon className="h-3 w-3 shrink-0" />
          <span className="truncate">{lockChimeName}</span>
          <ChevronRightIcon className="h-3 w-3 shrink-0 text-emerald-400/60" />
        </Link>
      )}

      {/* Top row — car state + duration. Right padding reserves room
          for the absolutely-positioned chime chip when present so
          long durations / labels can't slide under it. */}
      <div className={"flex items-center gap-3 " + (lockChimeName ? "pr-32 sm:pr-48" : "")}>
        <span className={`tile-icon ${statusHalo}`}>
          {showHealthWarning ? (
            <WarningIcon className="h-4 w-4" />
          ) : (
            <DirectionsCarIcon className="h-4 w-4" />
          )}
        </span>
        <div className="min-w-0 flex-1">
          <div className={`text-sm font-semibold ${statusColor}`}>{statusLabel}</div>
          {showHealthWarning ? (
            <div className="mt-0.5 text-[11px] leading-relaxed text-slate-400">
              {healthPresentation.guidance}
              {healthPresentation.repairRequired && (
                <Link
                  to="/settings?tab=Car%20%26%20Network"
                  className="ml-1 whitespace-nowrap font-medium text-rose-300 hover:text-rose-200"
                >
                  Open settings <ChevronRightIcon className="inline h-3 w-3" />
                </Link>
              )}
            </div>
          ) : parkedDuration ? (
            <div className="text-[11px] text-slate-500">{parkedDuration}</div>
          ) : null}
        </div>
      </div>

      {/* Chip row — battery / interior / exterior / tires */}
      <div className="mt-4 flex flex-wrap items-stretch gap-3">
        <StatusChip
          icon={
            charging ? (
              <BatteryAndroidFrameBoltIcon className="h-3.5 w-3.5 animate-pulse" />
            ) : (
              <BatteryLevelIcon pct={batterySoc ?? undefined} className="h-3.5 w-3.5" />
            )
          }
          label={charging ? "Charging" : "Battery"}
          value={batterySoc != null ? `${Math.round(batterySoc)}%` : "—"}
          accent={charging}
          valueClass={charging ? "text-emerald-300" : undefined}
          onClick={haveChargeDetail ? () => setBatteryOpen((o) => !o) : undefined}
          // Live charge SoC (currentCharge) is always fresh; only the
          // last-BLE-sample battery_pct can be stale.
          stale={
            currentCharge?.soc != null
              ? null
              : staleHint(sample?.field_secs_ago?.battery_pct)
          }
          trailing={
            haveChargeDetail ? (
              batteryOpen ? (
                <ExpandLessIcon className="h-3.5 w-3.5 text-slate-500" />
              ) : (
                <ExpandMoreIcon className="h-3.5 w-3.5 text-slate-500" />
              )
            ) : null
          }
        />
        <StatusChip
          icon={<DeviceThermostatIcon className="h-3.5 w-3.5" />}
          label="Interior"
          value={formatTemp(sample?.interior_temp_c, useFahrenheit)}
          stale={staleHint(sample?.field_secs_ago?.interior_temp_c)}
        />
        <StatusChip
          icon={<DeviceThermostatIcon className="h-3.5 w-3.5" />}
          label="Exterior"
          value={formatTemp(sample?.exterior_temp_c, useFahrenheit)}
          stale={staleHint(sample?.field_secs_ago?.exterior_temp_c)}
        />
        <StatusChip
          icon={<AlbumIcon className="h-3.5 w-3.5" />}
          label="Tires"
          value={tireStatus.label}
          valueClass={tireStatus.color}
          stale={
            tireStatus.kind === "none"
              ? null
              : staleHint(sample?.field_secs_ago?.tires)
          }
          onClick={haveTireData ? () => setTiresOpen((o) => !o) : undefined}
          trailing={
            haveTireData ? (
              tiresOpen ? (
                <ExpandLessIcon className="h-3.5 w-3.5 text-slate-500" />
              ) : (
                <ExpandMoreIcon className="h-3.5 w-3.5 text-slate-500" />
              )
            ) : null
          }
        />
      </div>

      {/* Battery drop-down — range / time-to-full / power, shown when the
          chip is expanded. Only the range row appears when idle. */}
      {batteryOpen && haveChargeDetail && currentCharge && (
        <div className="mt-4 border-t border-white/[0.06] pt-4">
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
            <MiniStat label="Range" value={fmtRangeUnit(currentCharge.rangeMi, metric)} />
            {charging && (
              <MiniStat
                label="Time to full"
                value={fmtToFull(currentCharge.minutesToFull) ?? "—"}
              />
            )}
            {charging && currentCharge.powerKw != null && (
              <MiniStat label="Power" value={`${currentCharge.powerKw} kW`} />
            )}
            {charging && currentCharge.currentA != null && (
              <MiniStat label="Current" value={fmtCurrent(currentCharge.currentA)} />
            )}
            {charging && currentCharge.voltageV != null && (
              <MiniStat label="Voltage" value={fmtVoltage(currentCharge.voltageV)} />
            )}
            {charging && currentCharge.rateMph != null && (
              <MiniStat
                label="Charge rate"
                value={fmtChargeRateUnit(currentCharge.rateMph, metric)}
              />
            )}
            {charging && currentCharge.energyAddedKwh != null && (
              <MiniStat
                label="Energy added"
                value={fmtEnergy(currentCharge.energyAddedKwh)}
              />
            )}
            {charging && currentCharge.limitSoc != null && (
              <MiniStat label="Charge limit" value={`${currentCharge.limitSoc}%`} />
            )}
          </div>
          {showChargingControls && (
            <ChargingControls
              key={`${currentCharge.charging}-${currentCharge.chargingAmps}-${currentCharge.maxChargingAmps}-${currentCharge.limitSoc}`}
              charging={currentCharge.charging}
              chargingAmps={currentCharge.chargingAmps}
              maxChargingAmps={currentCharge.maxChargingAmps}
              limitSoc={currentCharge.limitSoc}
            />
          )}
          <Link
            to="/charging"
            className="mt-3 inline-flex items-center gap-1 text-xs font-medium text-emerald-300 hover:text-emerald-200"
          >
            View charging history
            <ChevronRightIcon className="h-3.5 w-3.5" />
          </Link>
        </div>
      )}

      {/* Expandable chart — only mounts when the user clicks Tires.
          Lazy-loaded so users who don't expand never pull recharts. */}
      {tiresOpen && haveTireData && tireHistory && (
        <div className="mt-4 border-t border-white/[0.06] pt-4">
          <div className="mb-2 text-[11px] uppercase tracking-wider text-slate-500">
            Tire pressure · Last {tireHistory.days} days
          </div>
          <Suspense
            fallback={
              <div className="flex h-72 items-center justify-center text-sm text-slate-500">
                Loading tire history…
              </div>
            }
          >
            <TirePressureCard data={tireHistory} chartOnly />
          </Suspense>
        </div>
      )}
    </div>
  )
}

interface ChargingControlsProps {
  charging: boolean
  chargingAmps: number | null
  maxChargingAmps: number | null
  limitSoc: number | null
}

function ChargingControls({
  charging,
  chargingAmps,
  maxChargingAmps,
  limitSoc,
}: ChargingControlsProps) {
  const safeMaxAmps =
    maxChargingAmps != null && maxChargingAmps >= 1 && maxChargingAmps <= 80
      ? Math.round(maxChargingAmps)
      : null
  const initialAmps =
    chargingAmps == null || safeMaxAmps == null
      ? null
      : Math.max(1, Math.min(safeMaxAmps, Math.round(chargingAmps)))
  const initialLimit =
    limitSoc == null ? null : Math.max(50, Math.min(100, Math.round(limitSoc)))
  const [amps, setAmps] = useState(initialAmps)
  const [limit, setLimit] = useState(initialLimit)
  const [pending, setPending] = useState<"toggle" | "amps" | "limit" | null>(null)
  const [feedback, setFeedback] = useState<string | null>(null)
  const [failed, setFailed] = useState(false)

  async function send(
    kind: "toggle" | "amps" | "limit",
    request: Parameters<typeof sendChargingAction>[0],
  ) {
    setPending(kind)
    setFeedback(null)
    setFailed(false)
    try {
      await sendChargingAction(request)
      setFeedback("Command sent. Waiting for fresh vehicle data.")
    } catch (error) {
      setFailed(true)
      setFeedback(error instanceof Error ? error.message : "Charging command failed")
    } finally {
      setPending(null)
    }
  }

  return (
    <div className="mt-4 rounded-xl border border-emerald-400/20 bg-emerald-500/[0.06] p-3">
      <div className="flex items-center justify-between gap-3">
        <div>
          <div className="text-[9px] font-semibold uppercase tracking-wider text-emerald-400/80">
            Charging controls
          </div>
          <div className="mt-0.5 text-[11px] text-slate-400">
            Available while the charge port is open or charging.
          </div>
        </div>
        <button
          type="button"
          disabled={pending !== null}
          onClick={() =>
            void send("toggle", { action: charging ? "stop" : "start" })
          }
          className="shrink-0 rounded-lg border border-emerald-400/30 bg-emerald-500/15 px-3 py-1.5 text-xs font-semibold text-emerald-200 transition-colors hover:bg-emerald-500/25 disabled:cursor-wait disabled:opacity-50"
        >
          {pending === "toggle"
            ? "Sending…"
            : charging
              ? "Stop charging"
              : "Start charging"}
        </button>
      </div>

      {amps != null && safeMaxAmps != null && (
        <div className="mt-4">
          <div className="flex items-center justify-between text-xs">
            <label htmlFor="charging-amps" className="font-medium text-slate-300">
              Charging current
            </label>
            <span className="font-semibold tabular-nums text-slate-100">{amps} A</span>
          </div>
          <div className="mt-2 flex items-center gap-3">
            <input
              id="charging-amps"
              type="range"
              min={1}
              max={safeMaxAmps}
              step={1}
              value={amps}
              disabled={pending !== null}
              onChange={(event) => setAmps(Number(event.target.value))}
              className="min-w-0 flex-1 accent-emerald-400"
            />
            <button
              type="button"
              disabled={pending !== null || amps === initialAmps}
              onClick={() => void send("amps", { action: "setAmps", value: amps })}
              className="rounded-lg border border-white/10 bg-white/[0.04] px-2.5 py-1 text-[11px] font-semibold text-slate-200 transition-colors hover:bg-white/[0.08] disabled:cursor-not-allowed disabled:opacity-40"
            >
              {pending === "amps" ? "Applying…" : "Apply"}
            </button>
          </div>
        </div>
      )}

      {limit != null && (
        <div className="mt-4">
          <div className="flex items-center justify-between text-xs">
            <label htmlFor="charge-limit" className="font-medium text-slate-300">
              Charge limit
            </label>
            <span className="font-semibold tabular-nums text-slate-100">{limit}%</span>
          </div>
          <div className="mt-2 flex items-center gap-3">
            <input
              id="charge-limit"
              type="range"
              min={50}
              max={100}
              step={1}
              value={limit}
              disabled={pending !== null}
              onChange={(event) => setLimit(Number(event.target.value))}
              className="min-w-0 flex-1 accent-emerald-400"
            />
            <button
              type="button"
              disabled={pending !== null || limit === initialLimit}
              onClick={() => void send("limit", { action: "setLimit", value: limit })}
              className="rounded-lg border border-white/10 bg-white/[0.04] px-2.5 py-1 text-[11px] font-semibold text-slate-200 transition-colors hover:bg-white/[0.08] disabled:cursor-not-allowed disabled:opacity-40"
            >
              {pending === "limit" ? "Applying…" : "Apply"}
            </button>
          </div>
        </div>
      )}

      {feedback && (
        <div className={`mt-3 text-[11px] ${failed ? "text-rose-300" : "text-emerald-300"}`}>
          {feedback}
        </div>
      )}
    </div>
  )
}

interface StatusChipProps {
  icon: React.ReactNode
  label: string
  value: string
  valueClass?: string
  // Green-tinted chip + icon ring, used for the charging state.
  accent?: boolean
  onClick?: () => void
  trailing?: React.ReactNode
  // When set (e.g. "2h ago"), the value is older than it looks: render a
  // muted age suffix and dim the value so a stale reading isn't mistaken
  // for a live one.
  stale?: string | null
}

function StatusChip({
  icon,
  label,
  value,
  valueClass,
  accent,
  onClick,
  trailing,
  stale,
}: StatusChipProps) {
  const isButton = !!onClick
  const Wrapper = (isButton ? "button" : "div") as "button" | "div"
  return (
    <Wrapper
      {...(isButton ? { type: "button", onClick } : {})}
      className={
        "flex flex-1 min-w-[140px] items-center gap-2.5 rounded-xl border px-3 py-2 text-left transition-colors " +
        (accent
          ? "border-emerald-400/30 bg-emerald-500/10 "
          : "border-white/[0.06] bg-white/[0.025] ") +
        (isButton
          ? accent
            ? "hover:bg-emerald-500/15 cursor-pointer"
            : "hover:bg-white/[0.05] cursor-pointer"
          : "")
      }
    >
      <span
        className={
          "flex h-7 w-7 shrink-0 items-center justify-center rounded-full ring-1 ring-inset " +
          (accent
            ? "bg-emerald-500/15 ring-emerald-400/30 text-emerald-300"
            : "bg-white/[0.04] ring-white/10 text-slate-300")
        }
        aria-hidden
      >
        {icon}
      </span>
      <div className="min-w-0 flex-1">
        <div className="text-[9px] font-semibold uppercase tracking-wider text-slate-500">
          {label}
        </div>
        <div
          className={
            "mt-0.5 text-sm font-semibold tabular-nums leading-tight " +
            (stale ? "text-slate-400" : (valueClass ?? "text-slate-100"))
          }
        >
          {value}
          {stale && (
            <span
              className="ml-1.5 text-[10px] font-medium tabular-nums text-amber-400/80"
              title="Last-known value — not a live reading"
            >
              · {stale}
            </span>
          )}
        </div>
      </div>
      {trailing}
    </Wrapper>
  )
}

function MiniStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <div className="text-[9px] font-semibold uppercase tracking-wider text-slate-500">
        {label}
      </div>
      <div className="mt-0.5 text-sm font-semibold tabular-nums leading-tight text-slate-100">
        {value}
      </div>
    </div>
  )
}
