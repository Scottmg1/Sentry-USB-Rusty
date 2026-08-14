import { useEffect, useState } from "react"
import { useNavigate } from "react-router-dom"
import { ArrowBackIcon, ExpandMoreIcon, ConversionPathIcon, DataUsageIcon } from "@/components/icons"
import { api } from "@/lib/api"
import type { SafetyAnalytics as SafetyAnalyticsData, SafetyDayStats, SafetyScoreBreakdown } from "@/lib/api"
import { fetchDrives } from "@/api/drives"
import type { DriveSummary } from "@/types/drives"
import { cn } from "@/lib/utils"
import { formatDistance, formatDuration } from "@/lib/drive-format"

type Period = "day" | "week" | "month" | "all"

// Tesla-app blue, used literally — the app theme remaps Tailwind's blue
// scale to green, and this page deliberately matches the official app.
const TESLA_BLUE = "#3e6ae1"

// Rate caps (percent of the factor's denominator) at which a factor's
// penalty saturates — MUST mirror the CAP_* constants in
// crates/drives/src/safety.rs. Braking/turning are Tesla's published
// caps for the same conditional ratios. Used to bucket the severity
// segments and scale the daily factor charts.
const FACTOR_CAPS = { hardBrake: 5.2, aggrTurn: 17.1, speeding: 5.0, night: 30.0 }

// Conditional-ratio denominator floor (ms) — mirrors MIN_RATE_DENOM_MS
// in safety.rs so the daily charts match the backend's rate math.
const MIN_RATE_DENOM_MS = 60_000

// Adapts untyped backend JSON (camelCase from serde, snake_case from any
// older cache) with every field defaulted, like normalizeFsdAnalytics.
// eslint-disable-next-line @typescript-eslint/no-explicit-any
function normalizeSafety(raw: any): SafetyAnalyticsData | null {
  if (!raw || typeof raw !== "object") return null
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  const score = (s: any): SafetyScoreBreakdown | null =>
    s && typeof s === "object"
      ? {
          score: s.score ?? 0,
          hardBrakePct: s.hardBrakePct ?? s.hard_brake_pct ?? 0,
          aggrTurnPct: s.aggrTurnPct ?? s.aggr_turn_pct ?? 0,
          speedingPct: s.speedingPct ?? s.speeding_pct ?? 0,
          nightPct: s.nightPct ?? s.night_pct ?? 0,
          hardBrakePenalty: s.hardBrakePenalty ?? s.hard_brake_penalty ?? 0,
          aggrTurnPenalty: s.aggrTurnPenalty ?? s.aggr_turn_penalty ?? 0,
          speedingPenalty: s.speedingPenalty ?? s.speeding_penalty ?? 0,
          nightPenalty: s.nightPenalty ?? s.night_penalty ?? 0,
          fsdSharePct: s.fsdSharePct ?? s.fsd_share_pct ?? 0,
          fsdReliefPct: s.fsdReliefPct ?? s.fsd_relief_pct ?? 0,
        }
      : null
  const daily = Array.isArray(raw.daily)
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    ? raw.daily.map((d: any): SafetyDayStats => ({
        date: d?.date ?? "",
        dayName: d?.dayName ?? d?.day_name ?? "",
        score: d?.score ?? null,
        drives: d?.drives ?? 0,
        distanceMi: d?.distanceMi ?? d?.distance_mi ?? 0,
        distanceKm: d?.distanceKm ?? d?.distance_km ?? 0,
        hardBrakeEvents: d?.hardBrakeEvents ?? d?.hard_brake_events ?? 0,
        aggrTurnEvents: d?.aggrTurnEvents ?? d?.aggr_turn_events ?? 0,
        speedingMs: d?.speedingMs ?? d?.speeding_ms ?? 0,
        nightMi: d?.nightMi ?? d?.night_mi ?? 0,
        movingMs: d?.movingMs ?? d?.moving_ms ?? 0,
        manualMovingMs: d?.manualMovingMs ?? d?.manual_moving_ms ?? 0,
        hardBrakeMs: d?.hardBrakeMs ?? d?.hard_brake_ms ?? 0,
        aggrTurnMs: d?.aggrTurnMs ?? d?.aggr_turn_ms ?? 0,
        brakeAnyMs: d?.brakeAnyMs ?? d?.brake_any_ms ?? 0,
        turnAnyMs: d?.turnAnyMs ?? d?.turn_any_ms ?? 0,
      }))
    : []
  return {
    period: raw.period ?? "",
    periodStart: raw.periodStart ?? raw.period_start ?? "",
    totalDrives: raw.totalDrives ?? raw.total_drives ?? 0,
    scoredDrives: raw.scoredDrives ?? raw.scored_drives ?? 0,
    score: score(raw.score),
    totalDistanceMi: raw.totalDistanceMi ?? raw.total_distance_mi ?? 0,
    totalDistanceKm: raw.totalDistanceKm ?? raw.total_distance_km ?? 0,
    movingMs: raw.movingMs ?? raw.moving_ms ?? 0,
    manualMovingMs: raw.manualMovingMs ?? raw.manual_moving_ms ?? 0,
    hardBrakeEvents: raw.hardBrakeEvents ?? raw.hard_brake_events ?? 0,
    hardBrakeMs: raw.hardBrakeMs ?? raw.hard_brake_ms ?? 0,
    aggrTurnEvents: raw.aggrTurnEvents ?? raw.aggr_turn_events ?? 0,
    aggrTurnMs: raw.aggrTurnMs ?? raw.aggr_turn_ms ?? 0,
    speedingMs: raw.speedingMs ?? raw.speeding_ms ?? 0,
    brakeAnyMs: raw.brakeAnyMs ?? raw.brake_any_ms ?? 0,
    turnAnyMs: raw.turnAnyMs ?? raw.turn_any_ms ?? 0,
    nightMi: raw.nightMi ?? raw.night_mi ?? 0,
    nightKm: raw.nightKm ?? raw.night_km ?? 0,
    assistedPercent: raw.assistedPercent ?? raw.assisted_percent ?? 0,
    fsdDisengagements: raw.fsdDisengagements ?? raw.fsd_disengagements ?? 0,
    daily,
    bestDay: raw.bestDay ?? raw.best_day ?? "",
    bestDayScore: raw.bestDayScore ?? raw.best_day_score ?? null,
  }
}

function fmtDateLong(iso: string): string {
  const d = new Date(iso + "T00:00:00")
  return isNaN(d.getTime()) ? iso : d.toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" })
}

export default function SafetyScore() {
  const navigate = useNavigate()
  const [data, setData] = useState<SafetyAnalyticsData | null>(null)
  const [drives, setDrives] = useState<DriveSummary[]>([])
  const [period, setPeriod] = useState<Period>("month")
  const [loading, setLoading] = useState(true)
  const [metric, setMetric] = useState(false)
  const [expanded, setExpanded] = useState<string | null>(null)
  const [showLearnMore, setShowLearnMore] = useState(false)
  const [showDaily, setShowDaily] = useState(false)
  const [reliefTipFor, setReliefTipFor] = useState<string | null>(null)

  useEffect(() => {
    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        const entry = cfg.DRIVE_MAP_UNIT
        if (entry) {
          const val = typeof entry === "object" ? (entry.active ? entry.value : null) : entry
          if (val !== null) setMetric(val === "km")
        }
      })
      .catch(() => {})
    fetchDrives()
      .then((list) => setDrives(Array.isArray(list) ? list : []))
      .catch(() => setDrives([]))
  }, [])

  useEffect(() => {
    setLoading(true)
    api.getSafetyAnalytics(period)
      .then((resp) => setData(normalizeSafety(resp)))
      .catch(() => setData(null))
      .finally(() => setLoading(false))
  }, [period])

  if (loading) {
    return (
      <div className="mx-auto w-full max-w-2xl space-y-6 p-4 sm:p-6">
        <div className="mx-auto h-6 w-40 animate-pulse rounded bg-white/5" />
        <div className="mx-auto h-52 w-72 animate-pulse rounded-xl bg-white/5" />
        <div className="h-72 animate-pulse rounded-xl bg-white/5" />
      </div>
    )
  }

  if (!data) {
    return (
      <div className="flex h-full items-center justify-center p-6">
        <p className="text-slate-500">No drive data available yet. Safety Score appears after your first processed drives.</p>
      </div>
    )
  }

  const score = data.score
  const distUnit = metric ? "km" : "mi"
  const nightDist = metric ? data.nightKm : data.nightMi
  const daily = (data.daily || []).filter((d) => typeof d.date === "string" && d.date.length >= 10)

  const lastDrive = drives
    .filter((d) => !d.summon)
    .sort((a, b) => (a.startTime < b.startTime ? 1 : -1))[0]

  const periodLabel =
    period === "day"
      ? "today"
      : data.periodStart
        ? `${fmtDateLong(data.periodStart)} - ${new Date().toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" })}`
        : "all recorded driving"

  // Severity bucket per factor: which of the three segments lights up.
  // green = clean, yellow = present but mid-range, red = near/at the cap.
  const bucket = (frac: number): "green" | "yellow" | "red" =>
    frac <= 0.02 ? "green" : frac < 0.5 ? "yellow" : "red"

  interface Factor {
    key: string
    label: string
    value: string
    frac: number
    description: string
    avg: number | null
    series: number[] | null
    relieved: boolean
    forceBucket?: "green" | "yellow" | "red"
  }

  const factors: Factor[] = score
    ? [
        {
          key: "hardBrake",
          label: "Hard Braking",
          value: `${score.hardBrakePct}%`,
          frac: Math.min(score.hardBrakePct / FACTOR_CAPS.hardBrake, 1),
          description: "Proportion of braking time (deceleration above 0.1g) spent braking harder than 0.3g — the same conditional measure Tesla uses. Braking while FSD or Autopilot is engaged is not counted.",
          avg: score.hardBrakePct,
          series: daily.map((d) => (d.hardBrakeMs / Math.max(d.brakeAnyMs, MIN_RATE_DENOM_MS)) * 100),
          relieved: score.fsdReliefPct > 0,
        },
        {
          key: "aggrTurn",
          label: "Aggressive Turning",
          value: `${score.aggrTurnPct}%`,
          frac: Math.min(score.aggrTurnPct / FACTOR_CAPS.aggrTurn, 1),
          description: "Proportion of turning time (lateral force above 0.2g) done with force above 0.4g — the same conditional measure Tesla uses. Turns under FSD or Autopilot are not counted.",
          avg: score.aggrTurnPct,
          series: daily.map((d) => (d.aggrTurnMs / Math.max(d.turnAnyMs, MIN_RATE_DENOM_MS)) * 100),
          relieved: score.fsdReliefPct > 0,
        },
        {
          key: "speeding",
          label: "Excessive Speeding",
          value: `${score.speedingPct}%`,
          frac: Math.min(score.speedingPct / FACTOR_CAPS.speeding, 1),
          description: `Proportion of moving time driven above 85 mph (${formatDuration(data.speedingMs)} in this period).`,
          avg: score.speedingPct,
          series: daily.map((d) => (d.movingMs > 0 ? (d.speedingMs / d.movingMs) * 100 : 0)),
          relieved: false,
        },
        {
          key: "night",
          label: "Late Night Driving",
          value: `${score.nightPct}%`,
          frac: Math.min(score.nightPct / FACTOR_CAPS.night, 1),
          description: `Proportion of miles driven between 10pm and 4am (${nightDist.toFixed(1)} ${distUnit} in this period). Risk-weighted by hour like Tesla's: driving at 3am counts three times as much as 10pm.`,
          avg: score.nightPct,
          series: daily.map((d) => (d.distanceMi > 0 ? (d.nightMi / d.distanceMi) * 100 : 0)),
          relieved: false,
        },
        {
          key: "fsd",
          label: "FSD (Supervised) Usage",
          value: `${Math.round(score.fsdSharePct)}%`,
          frac: 0,
          forceBucket: "green",
          description: `Share of miles driven with FSD, Autosteer or TACC engaged. Events during assisted driving never count against you${score.fsdReliefPct > 0 ? `, and this share trims the braking and turning penalties by ${Math.round(score.fsdReliefPct)}%` : ""}.`,
          avg: null,
          series: null,
          relieved: false,
        },
        {
          key: "disengage",
          label: "Forced Autosteer or FSD (Supervised) Disengagements",
          value: `${data.fsdDisengagements} total`,
          frac: 0,
          forceBucket: data.fsdDisengagements === 0 ? "green" : "yellow",
          description: "Times FSD (Supervised) or Autosteer was disengaged in this period. The dashcam stream can't distinguish forced disengagements (strikeouts) from normal takeovers, so every disengagement is counted. Informational only — this does not affect the score.",
          avg: null,
          series: null,
          relieved: false,
        },
      ]
    : []

  return (
    <div className="mx-auto w-full max-w-2xl p-4 pb-10 sm:p-6">
      {/* Header — centered title, Tesla-app style */}
      <div className="relative flex items-center justify-center py-2">
        <button
          onClick={() => navigate("/drives")}
          className="absolute left-0 rounded-lg p-2 text-slate-400 transition-colors hover:bg-white/5 hover:text-slate-200"
        >
          <ArrowBackIcon className="h-5 w-5" />
        </button>
        <h1 className="text-xl font-semibold text-slate-100">Safety Score</h1>
        <div className="absolute right-0 flex items-center gap-1 rounded-full border border-white/10 bg-white/5 p-0.5">
          {(["day", "week", "month", "all"] as Period[]).map((p) => (
            <button
              key={p}
              onClick={() => setPeriod(p)}
              className={cn(
                "rounded-full px-2 py-1 text-[10px] font-medium transition-colors",
                period === p ? "bg-white/10 text-slate-100" : "text-slate-500 hover:text-slate-300"
              )}
            >
              {p === "day" ? "1d" : p === "week" ? "7d" : p === "month" ? "30d" : "All"}
            </button>
          ))}
        </div>
      </div>

      {data.totalDrives === 0 || !score ? (
        <div className="mt-6 rounded-xl border border-white/10 bg-white/[0.03] p-10 text-center">
          <p className="text-sm font-semibold text-slate-200">
            {data.totalDrives === 0
              ? period === "day" ? "No drives yet today" : "No drives in this period"
              : "Not enough driving to score yet"}
          </p>
          <p className="mt-1 text-xs text-slate-500">
            The score needs at least half a mile of SEI-recorded driving in the period.
          </p>
        </div>
      ) : (<>
      {/* Subtitle — scoring window */}
      <p className="mt-2 text-center text-sm text-slate-500">
        Based on driving behavior for
        <br />
        <span className="text-slate-300">{periodLabel}</span>
      </p>

      {/* Gauge */}
      <ScoreGauge score={score.score} />

      {/* Scoring Factors */}
      <h2 className="mt-2 text-2xl font-semibold text-slate-100">Scoring Factors</h2>
      <button
        onClick={() => setShowLearnMore((v) => !v)}
        className="mt-1 text-sm font-medium"
        style={{ color: TESLA_BLUE }}
      >
        Learn More
      </button>
      {showLearnMore && (
        <p className="mt-2 rounded-lg bg-white/[0.03] p-3 text-xs leading-relaxed text-slate-400">
          This score is estimated from your dashcam SEI telemetry. Braking and turning G-forces
          are derived from speed and GPS data - as such events are read conservatively. Braking
          and turning factors are tuned to be similar to Tesla's conditional measures and caps.
          <br />
          <br />
          Driving on FSD reduces what can count against you.
        </p>
      )}

      <div className="mt-2">
        {factors.map((f) => {
          const isOpen = expanded === f.key
          const b = f.forceBucket ?? bucket(f.frac)
          return (
            <div key={f.key} className="border-b border-white/[0.06] py-4 last:border-b-0">
              <button
                onClick={() => setExpanded(isOpen ? null : f.key)}
                className="flex w-full items-center justify-between text-left"
              >
                <span className="flex items-center gap-2 pr-3 text-[15px] text-slate-100">
                  {f.label}
                  {f.relieved && (
                    <span
                      className="relative inline-block"
                      onClick={(e) => {
                        // Don't let a tap on the pill expand/collapse the row.
                        e.stopPropagation()
                        setReliefTipFor(reliefTipFor === f.key ? null : f.key)
                      }}
                      onMouseEnter={() => setReliefTipFor(f.key)}
                      onMouseLeave={() => setReliefTipFor(null)}
                    >
                      <span className="rounded-full bg-emerald-500/10 px-1.5 py-px text-[9px] font-medium text-emerald-400">
                        FSD relief
                      </span>
                      {reliefTipFor === f.key && (
                        <span className="absolute left-1/2 top-full z-50 mt-1.5 w-60 -translate-x-1/2 rounded-md border border-white/10 bg-slate-900/95 px-2.5 py-1.5 text-[11px] font-normal normal-case leading-relaxed text-slate-300 shadow-2xl">
                          This penalty is reduced by {Math.round(score.fsdReliefPct)}% because{" "}
                          {Math.round(score.fsdSharePct)}% of your miles are driven on FSD/Autopilot.
                          Braking and turning while assisted never count at all.
                        </span>
                      )}
                    </span>
                  )}
                </span>
                <span className="flex shrink-0 items-center gap-2">
                  <span className="text-[15px] font-medium text-slate-100 tabular-nums">{f.value}</span>
                  <ExpandMoreIcon
                    className={cn("h-4 w-4 text-slate-400 transition-transform", isOpen && "rotate-180")}
                  />
                </span>
              </button>

              {/* Three-segment severity indicator */}
              <div className="mt-2.5 flex w-56 max-w-full gap-1.5">
                <span className={cn("h-1.5 flex-1 rounded-full", b === "red" ? "bg-red-500" : "bg-white/10")} />
                <span className={cn("h-1.5 flex-1 rounded-full", b === "yellow" ? "bg-amber-400" : "bg-white/10")} />
                <span className={cn("h-1.5 flex-1 rounded-full", b === "green" ? "bg-emerald-500" : "bg-white/10")} />
              </div>

              {isOpen && (
                <div className="mt-3">
                  <p className="text-sm text-slate-400">{f.description}</p>
                  {f.series && f.series.length >= 2 && (
                    <>
                      <p className="mt-3 flex items-center gap-2 text-xs text-slate-500">
                        Period Average
                        <span className="inline-block h-px w-6 bg-slate-500" />
                      </p>
                      <FactorChart
                        dates={daily.map((d) => d.date)}
                        series={f.series}
                        avg={f.avg}
                      />
                    </>
                  )}
                </div>
              )}
            </div>
          )
        })}
      </div>

      {/* Links section */}
      <div className="mt-6 border-t border-white/[0.08] pt-2">
        {lastDrive && (
          <button
            onClick={() => navigate(`/drives/${lastDrive.id}`)}
            className="flex w-full items-center gap-4 py-4 text-left transition-colors hover:bg-white/[0.02]"
          >
            <ConversionPathIcon className="h-6 w-6 shrink-0 text-slate-300" />
            <span className="flex-1">
              <span className="block text-[15px] text-slate-100">View Last Trip</span>
              <span className="block text-sm text-slate-500">
                {new Date(lastDrive.startTime).toLocaleDateString([], { month: "short", day: "numeric", year: "numeric" })} at{" "}
                {new Date(lastDrive.startTime).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })}
                {typeof lastDrive.safetyScore === "number" && ` · scored ${Math.round(lastDrive.safetyScore)}`}
              </span>
            </span>
            <ExpandMoreIcon className="h-4 w-4 -rotate-90 text-slate-500" />
          </button>
        )}
        <button
          onClick={() => setShowDaily((v) => !v)}
          className="flex w-full items-center gap-4 border-t border-white/[0.06] py-4 text-left transition-colors hover:bg-white/[0.02]"
        >
          <DataUsageIcon className="h-6 w-6 shrink-0 text-slate-300" />
          <span className="flex-1">
            <span className="block text-[15px] text-slate-100">Daily Breakdown</span>
            <span className="block text-sm text-slate-500">View details of each day</span>
          </span>
          <ExpandMoreIcon className={cn("h-4 w-4 text-slate-500 transition-transform", showDaily ? "rotate-180" : "-rotate-90")} />
        </button>
        {showDaily && (
          <div className="pb-2">
            {[...daily].reverse().map((d) => (
              <div key={d.date} className="flex items-center justify-between border-t border-white/[0.04] py-2.5 pl-10">
                <span className="text-sm text-slate-300">
                  {new Date(d.date + "T00:00:00").toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" })}
                </span>
                <span className="text-xs text-slate-500 tabular-nums">
                  {formatDistance(d.distanceMi, d.distanceKm, metric)} · {d.drives} drive{d.drives === 1 ? "" : "s"}
                </span>
                <span className="w-10 text-right text-sm font-semibold text-slate-100 tabular-nums">
                  {d.score !== null ? Math.round(d.score) : "—"}
                </span>
              </div>
            ))}
          </div>
        )}
      </div>

      <p className="mt-6 text-xs leading-relaxed text-slate-600">
        Estimated from dashcam SEI telemetry data extracted by Sentry USB - not Tesla's official
        Safety Score.
      </p>
      </>)}
    </div>
  )
}

/** Semicircular gauge with the score centered, Tesla-app style. */
function ScoreGauge({ score }: { score: number }) {
  const W = 320
  const H = 180
  const r = 130
  const cx = W / 2
  const cy = 160
  const stroke = 14
  const arcLen = Math.PI * r
  const frac = Math.min(Math.max(score / 100, 0), 1)
  const d = `M ${cx - r} ${cy} A ${r} ${r} 0 0 1 ${cx + r} ${cy}`
  return (
    <div className="mx-auto mt-4 w-full max-w-xs">
      <svg width="100%" viewBox={`0 0 ${W} ${H + 10}`}>
        <path d={d} fill="none" stroke="#ffffff" strokeOpacity="0.08" strokeWidth={stroke} strokeLinecap="round" />
        <path
          d={d}
          fill="none"
          stroke={TESLA_BLUE}
          strokeWidth={stroke}
          strokeLinecap="round"
          strokeDasharray={`${arcLen * frac} ${arcLen}`}
          style={{ transition: "stroke-dasharray 700ms ease" }}
        />
        <text
          x={cx}
          y={cy - 20}
          textAnchor="middle"
          fill="#f1f5f9"
          fontSize="72"
          fontWeight="600"
          fontFamily="Inter, system-ui, sans-serif"
        >
          {Math.round(score)}
        </text>
        <text x={cx - r} y={cy + 24} textAnchor="middle" fill="#64748b" fontSize="14" fontFamily="Inter, system-ui, sans-serif">
          Unsafe
        </text>
        <text x={cx + r} y={cy + 24} textAnchor="middle" fill="#64748b" fontSize="14" fontFamily="Inter, system-ui, sans-serif">
          Safe
        </text>
      </svg>
    </div>
  )
}

/** Per-factor daily proportion line, Tesla-app style: blue line, dashed
 *  gridlines, gray period-average line, sparse date labels. */
function FactorChart({ dates, series, avg }: { dates: string[]; series: number[]; avg: number | null }) {
  const W = 600
  const H = 170
  const PAD_L = 34
  const PAD_R = 10
  const PAD_T = 12
  const PAD_B = 24

  const maxVal = Math.max(...series, avg ?? 0, 0.1)
  // Round the axis top up to a friendly value.
  const yTop = maxVal <= 1 ? Math.ceil(maxVal * 10) / 10 : Math.ceil(maxVal * 1.15)
  const x = (i: number) => (series.length === 1 ? W / 2 : PAD_L + (i / (series.length - 1)) * (W - PAD_L - PAD_R))
  const y = (v: number) => PAD_T + (1 - v / yTop) * (H - PAD_T - PAD_B)

  const path = series.map((v, i) => `${i === 0 ? "M" : "L"}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join(" ")
  const grid = [yTop, yTop / 2, 0]
  const fmt = (v: number) => (v >= 10 ? `${Math.round(v)}%` : `${Math.round(v * 10) / 10}%`)

  // Sparse x labels: first, ~third, ~two-thirds, last.
  const labelIdx = Array.from(
    new Set([0, Math.floor((series.length - 1) / 3), Math.floor(((series.length - 1) * 2) / 3), series.length - 1])
  )

  return (
    <svg width="100%" height={H} viewBox={`0 0 ${W} ${H}`} className="mt-1">
      {grid.map((g) => (
        <g key={g}>
          <line
            x1={PAD_L}
            x2={W - PAD_R}
            y1={y(g)}
            y2={y(g)}
            stroke="#ffffff"
            strokeOpacity="0.12"
            strokeDasharray="4 5"
          />
          <text x={PAD_L - 6} y={y(g) + 3} textAnchor="end" fill="#64748b" fontSize="10" fontFamily="Inter, system-ui, sans-serif">
            {fmt(g)}
          </text>
        </g>
      ))}
      {avg !== null && (
        <line x1={PAD_L} x2={W - PAD_R} y1={y(Math.min(avg, yTop))} y2={y(Math.min(avg, yTop))} stroke="#94a3b8" strokeOpacity="0.7" strokeWidth="1.5" />
      )}
      <path d={path} fill="none" stroke={TESLA_BLUE} strokeWidth="2.5" strokeLinejoin="round" strokeLinecap="round" />
      {labelIdx.map((i) => (
        <text
          key={i}
          x={x(i)}
          y={H - 6}
          textAnchor={i === 0 ? "start" : i === series.length - 1 ? "end" : "middle"}
          fill="#64748b"
          fontSize="10"
          fontFamily="Inter, system-ui, sans-serif"
        >
          {dates[i]?.slice(5).replace("-", "/")}
        </text>
      ))}
    </svg>
  )
}
