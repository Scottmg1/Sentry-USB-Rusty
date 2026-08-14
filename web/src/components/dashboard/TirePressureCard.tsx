import { memo, useMemo } from "react"
import { AlbumIcon } from "@/components/icons"
import {
  CartesianGrid,
  Legend,
  Line,
  LineChart,
  ReferenceArea,
  ReferenceLine,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts"
import { useUnits } from "@/lib/units"

// Telemetry is stored in PSI and converted only for display.
const PSI_TO_BAR = 0.0689476

// Cap zone bands to the visible domain: Recharts centers labels using literal
// y1/y2 values, including portions outside the viewport. Thresholds stay in PSI.
const ZONES_PSI = [
  { y1: 50, y2: 55, fill: "rgba(127, 29, 29, 0.55)", labelColor: "#fca5a5", gt: 50, text: "UNSAFE" },
  { y1: 45, y2: 50, fill: "rgba(63, 98, 18, 0.55)", labelColor: "#bef264", gt: 45, text: "HARSHER RIDE & WEAR" },
  { y1: 36, y2: 45, fill: "rgba(22, 78, 51, 0.55)", labelColor: "rgba(167, 243, 208, 0.85)", text: "OPTIMAL" },
  { y1: 28, y2: 36, fill: "rgba(124, 45, 18, 0.55)", labelColor: "#fcd34d", lt: 36, text: "REDUCED HANDLING & EFFICIENCY" },
  { y1: 20, y2: 28, fill: "rgba(127, 29, 29, 0.55)", labelColor: "#fca5a5", lt: 28, text: "UNSAFE" },
] as const

// Interior zone boundaries in PSI.
const ZONE_BOUNDARIES_PSI = [
  { y: 50, color: "rgba(252, 165, 165, 0.7)" },
  { y: 45, color: "rgba(190, 242, 100, 0.7)" },
  { y: 36, color: "rgba(252, 211, 77, 0.7)" },
  { y: 28, color: "rgba(252, 165, 165, 0.7)" },
] as const

// The PSI domain keeps the lower unsafe band visible without unused range.
const Y_DOMAIN_PSI: [number, number] = [20, 55]

// Distinct line colors remain legible against the zone bands.
const TIRE_COLORS = {
  fl: "#34d399",
  fr: "#a3e635",
  rl: "#5eead4",
  rr: "#facc15",
} as const

export interface TirePoint {
  ts: number
  fl?: number
  fr?: number
  rl?: number
  rr?: number
}

export interface TireHistoryResponse {
  points: TirePoint[]
  days: number
}

interface TirePressureCardProps {
  // The parent controls mounting so Recharts is absent without tire data.
  data: TireHistoryResponse
  days?: number
  // Chart-only mode lets CarStatusCard provide the surrounding chrome.
  chartOnly?: boolean
}

export const TirePressureCard = memo(function TirePressureCard({
  data,
  days = 30,
  chartOnly = false,
}: TirePressureCardProps) {
  const { pressureBar } = useUnits()

  // Convert the full series once so axes, chips, and tooltips share a unit.
  const points = useMemo(
    () =>
      pressureBar
        ? data.points.map((p) => ({
            ts: p.ts,
            fl: p.fl !== undefined ? p.fl * PSI_TO_BAR : undefined,
            fr: p.fr !== undefined ? p.fr * PSI_TO_BAR : undefined,
            rl: p.rl !== undefined ? p.rl * PSI_TO_BAR : undefined,
            rr: p.rr !== undefined ? p.rr * PSI_TO_BAR : undefined,
          }))
        : data.points,
    [data.points, pressureBar],
  )

  // Latest reading per tire for the header strip.
  const latest = useMemo(() => {
    const out: Partial<Record<"fl" | "fr" | "rl" | "rr", number>> = {}
    for (let i = points.length - 1; i >= 0; i--) {
      const p = points[i]
      if (out.fl === undefined && p.fl !== undefined) out.fl = p.fl
      if (out.fr === undefined && p.fr !== undefined) out.fr = p.fr
      if (out.rl === undefined && p.rl !== undefined) out.rl = p.rl
      if (out.rr === undefined && p.rr !== undefined) out.rr = p.rr
      if (
        out.fl !== undefined &&
        out.fr !== undefined &&
        out.rl !== undefined &&
        out.rr !== undefined
      )
        break
    }
    return out
  }, [points])

  const conv = (psi: number) => (pressureBar ? psi * PSI_TO_BAR : psi)
  const unitUpper = pressureBar ? "BAR" : "PSI"
  const fmtThreshold = (psi: number) =>
    pressureBar ? (psi * PSI_TO_BAR).toFixed(1) : String(psi)
  const fmtTick = (n: number) => (pressureBar ? n.toFixed(1) : String(Math.round(n)))
  const fmtValue = (v: number) => (pressureBar ? `${v.toFixed(2)} bar` : `${v.toFixed(1)} psi`)

  const zones = ZONES_PSI.map((z) => ({
    key: z.y1,
    y1: conv(z.y1),
    y2: conv(z.y2),
    fill: z.fill,
    labelColor: z.labelColor,
    label:
      "gt" in z
        ? `>${fmtThreshold(z.gt)} ${unitUpper} • ${z.text}`
        : "lt" in z
          ? `<${fmtThreshold(z.lt)} ${unitUpper} • ${z.text}`
          : z.text,
  }))
  const boundaries = ZONE_BOUNDARIES_PSI.map((b) => ({ key: b.y, y: conv(b.y), color: b.color }))
  const domain: [number, number] = [conv(Y_DOMAIN_PSI[0]), conv(Y_DOMAIN_PSI[1])]

  const chart = (
    <div className="h-72 w-full" aria-label="Tire pressure chart">
          <ResponsiveContainer minHeight={0} minWidth={0}>
            <LineChart
              data={points}
              margin={{ top: 8, right: 20, bottom: 24, left: 0 }}
            >
              <CartesianGrid
                stroke="#1e242f"
                strokeDasharray="3 3"
                vertical={false}
              />
              {zones.map((z) => (
                <ReferenceArea
                  key={z.key}
                  y1={z.y1}
                  y2={z.y2}
                  fill={z.fill}
                  stroke="transparent"
                  label={{
                    value: z.label,
                    position: "center",
                    fill: z.labelColor,
                    fontSize: 10,
                    fontWeight: 600,
                    letterSpacing: "0.08em",
                  }}
                  ifOverflow="hidden"
                />
              ))}
              {boundaries.map((b) => (
                <ReferenceLine
                  key={b.key}
                  y={b.y}
                  stroke={b.color}
                  strokeWidth={1}
                  strokeDasharray="6 4"
                  ifOverflow="hidden"
                />
              ))}
              <XAxis
                dataKey="ts"
                type="number"
                domain={["dataMin", "dataMax"]}
                tickFormatter={formatXTick}
                stroke="#475569"
                tick={{ fill: "#64748b", fontSize: 11 }}
                tickLine={false}
                axisLine={false}
                tickMargin={10}
                minTickGap={64}
              />
              <YAxis
                domain={domain}
                stroke="#475569"
                tick={{ fill: "#64748b", fontSize: 11 }}
                tickFormatter={(n: number) => fmtTick(n)}
                tickLine={false}
                axisLine={false}
                tickMargin={4}
                width={pressureBar ? 40 : 32}
              />
              <Tooltip
                content={({ active, payload }) => {
                  if (!active || !payload || payload.length === 0) return null
                  const p = payload[0].payload as TirePoint
                  return (
                    <div className="rounded-md border border-white/10 bg-slate-900/95 px-2 py-1.5 text-xs text-slate-200 shadow-xl">
                      <div className="mb-1 text-[10px] text-slate-500 tabular-nums">
                        {formatTooltipTime(p.ts)}
                      </div>
                      <TooltipRow label="FL" value={p.fl} color={TIRE_COLORS.fl} format={fmtValue} />
                      <TooltipRow label="FR" value={p.fr} color={TIRE_COLORS.fr} format={fmtValue} />
                      <TooltipRow label="RL" value={p.rl} color={TIRE_COLORS.rl} format={fmtValue} />
                      <TooltipRow label="RR" value={p.rr} color={TIRE_COLORS.rr} format={fmtValue} />
                    </div>
                  )
                }}
              />
              <Legend
                verticalAlign="bottom"
                height={20}
                iconType="line"
                wrapperStyle={{ fontSize: 11, color: "#94a3b8" }}
              />
              <Line
                type="monotone"
                name="Front L"
                dataKey="fl"
                stroke={TIRE_COLORS.fl}
                strokeWidth={1.75}
                dot={false}
                isAnimationActive={false}
                connectNulls
              />
              <Line
                type="monotone"
                name="Front R"
                dataKey="fr"
                stroke={TIRE_COLORS.fr}
                strokeWidth={1.75}
                dot={false}
                isAnimationActive={false}
                connectNulls
              />
              <Line
                type="monotone"
                name="Rear L"
                dataKey="rl"
                stroke={TIRE_COLORS.rl}
                strokeWidth={1.75}
                dot={false}
                isAnimationActive={false}
                connectNulls
              />
              <Line
                type="monotone"
                name="Rear R"
                dataKey="rr"
                stroke={TIRE_COLORS.rr}
                strokeWidth={1.75}
                dot={false}
                isAnimationActive={false}
                connectNulls
              />
            </LineChart>
          </ResponsiveContainer>
      </div>
  )

  if (chartOnly) {
    return chart
  }

  return (
    <div className="glass-card p-4">
      <div className="mb-3 flex flex-wrap items-center gap-3">
        <span className="tile-icon halo-blue">
          <AlbumIcon className="h-4 w-4" />
        </span>
        <div className="min-w-0">
          <div className="text-sm font-semibold text-slate-100">
            Tire pressure
          </div>
          <div className="text-[11px] uppercase tracking-wider text-slate-500">
            Last {days} days
          </div>
        </div>
        <div className="ml-auto flex flex-wrap gap-3 text-xs tabular-nums text-slate-300">
          <LatestChip label="FL" value={latest.fl} color={TIRE_COLORS.fl} format={fmtValue} />
          <LatestChip label="FR" value={latest.fr} color={TIRE_COLORS.fr} format={fmtValue} />
          <LatestChip label="RL" value={latest.rl} color={TIRE_COLORS.rl} format={fmtValue} />
          <LatestChip label="RR" value={latest.rr} color={TIRE_COLORS.rr} format={fmtValue} />
        </div>
      </div>
      {chart}
    </div>
  )
})

function LatestChip({
  label,
  value,
  color,
  format,
}: {
  label: string
  value: number | undefined
  color: string
  format: (v: number) => string
}) {
  return (
    <span className="inline-flex items-center gap-1.5">
      <span
        className="inline-block h-2 w-2 rounded-full"
        style={{ background: color }}
        aria-hidden
      />
      <span className="text-slate-500">{label}</span>
      <span className="text-slate-100">
        {value !== undefined ? format(value) : "—"}
      </span>
    </span>
  )
}

function TooltipRow({
  label,
  value,
  color,
  format,
}: {
  label: string
  value: number | undefined
  color: string
  format: (v: number) => string
}) {
  return (
    <div className="flex items-center gap-2 tabular-nums">
      <span
        className="inline-block h-2 w-2 rounded-full"
        style={{ background: color }}
        aria-hidden
      />
      <span className="text-slate-400">{label}</span>
      <span className="ml-auto font-medium">
        {value !== undefined ? format(value) : "—"}
      </span>
    </div>
  )
}

function formatXTick(ms: number): string {
  const t = new Date(ms)
  if (Number.isNaN(t.getTime())) return ""
  return t.toLocaleDateString([], { month: "short", day: "numeric" })
}

function formatTooltipTime(ms: number): string {
  const t = new Date(ms)
  if (Number.isNaN(t.getTime())) return ""
  return t.toLocaleString([], {
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  })
}
