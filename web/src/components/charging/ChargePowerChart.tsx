import { memo, useMemo } from "react"
import {
  CartesianGrid,
  Legend,
  Line,
  LineChart,
  ResponsiveContainer,
  Tooltip,
  XAxis,
  YAxis,
} from "recharts"
import type { ChargePoint } from "@/types/charging"

// Power and state of charge use independent axes; missed samples remain gaps.
const POWER_COLOR = "#34d399"
const SOC_COLOR = "#60a5fa"

const LEFT_MARGIN = 4
const RIGHT_MARGIN = 8
const YAXIS_WIDTH = 40
const AXIS_LABEL_STYLE = {
  fill: "#94a3b8",
  fontSize: 11,
  fontWeight: 700,
  textAnchor: "middle" as const,
}

type ChartPoint = ChargePoint & { socProjected?: number | null }

function ChargePowerChart({
  points,
  projection,
}: {
  points: ChargePoint[]
  // Project state of charge only for an active session.
  projection?: { ts: number; soc: number }[]
}) {
  const hasProjection = !!projection && projection.length > 0
  // Seed the projected series at the last actual point to join both lines.
  const data: ChartPoint[] = useMemo(
    () => [
      ...points.map((p, i) => ({
        ...p,
        socProjected: hasProjection && i === points.length - 1 ? p.soc : null,
      })),
      ...(projection ?? []).map((pp) => ({
        ts: pp.ts,
        powerKw: null,
        currentA: null,
        voltageV: null,
        rateMph: null,
        soc: null,
        rangeMi: null,
        energyAddedKwh: null,
        socProjected: pp.soc,
      })),
    ],
    [points, projection, hasProjection],
  )
  return (
    <div className="h-56 w-full" aria-label="Charging power and state-of-charge chart">
      <ResponsiveContainer minHeight={0} minWidth={0}>
        <LineChart
          data={data}
          margin={{ top: 24, right: RIGHT_MARGIN, bottom: 24, left: LEFT_MARGIN }}
        >
          <CartesianGrid stroke="#1e242f" strokeDasharray="3 3" vertical={false} />
          <XAxis
            dataKey="ts"
            type="number"
            domain={["dataMin", "dataMax"]}
            tickFormatter={formatTick}
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickLine={false}
            axisLine={false}
            tickMargin={10}
            minTickGap={56}
            padding={{ left: 6, right: 6 }}
          />
          <YAxis
            yAxisId="power"
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickFormatter={(n: number) => `${Math.round(n)}`}
            tickLine={false}
            axisLine={false}
            tickMargin={4}
            width={YAXIS_WIDTH}
            domain={[0, "dataMax + 2"]}
            label={{ value: "kW", position: "top", offset: 14, style: AXIS_LABEL_STYLE }}
          />
          <YAxis
            yAxisId="soc"
            orientation="right"
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickFormatter={(n: number) => `${Math.round(n)}%`}
            tickLine={false}
            axisLine={false}
            tickMargin={4}
            width={YAXIS_WIDTH}
            domain={[0, 100]}
            ticks={[0, 25, 50, 75, 100]}
            label={{ value: "SoC", position: "top", offset: 14, style: AXIS_LABEL_STYLE }}
          />
          <Tooltip
            content={({ active, payload }) => {
              if (!active || !payload || payload.length === 0) return null
              const p = payload[0].payload as ChartPoint
              return (
                <div className="rounded-md border border-white/10 bg-slate-900/95 px-2 py-1.5 text-xs text-slate-200 shadow-xl">
                  <div className="mb-1 text-[10px] text-slate-500 tabular-nums">
                    {formatTooltipTime(p.ts)}
                  </div>
                  {p.powerKw != null && (
                    <Row color={POWER_COLOR} label="Power" value={`${p.powerKw} kW`} />
                  )}
                  {p.soc != null && (
                    <Row color={SOC_COLOR} label="Battery" value={`${Math.round(p.soc)}%`} />
                  )}
                  {p.soc == null && p.socProjected != null && (
                    <Row
                      color={SOC_COLOR}
                      label="Projected"
                      value={`${Math.round(p.socProjected)}%`}
                    />
                  )}
                  {/* Range has a separate chart below. */}
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
            yAxisId="power"
            type="monotone"
            name="Power (kW)"
            dataKey="powerKw"
            stroke={POWER_COLOR}
            strokeWidth={2}
            dot={false}
            isAnimationActive={false}
            connectNulls
          />
          <Line
            yAxisId="soc"
            type="monotone"
            name="Battery (%)"
            dataKey="soc"
            stroke={SOC_COLOR}
            strokeWidth={2}
            dot={false}
            isAnimationActive={false}
            connectNulls
          />
          {hasProjection && (
            <Line
              yAxisId="soc"
              type="monotone"
              name="Projected"
              dataKey="socProjected"
              stroke={SOC_COLOR}
              strokeWidth={2}
              strokeDasharray="4 4"
              dot={false}
              isAnimationActive={false}
              connectNulls
            />
          )}
        </LineChart>
      </ResponsiveContainer>
    </div>
  )
}

export default memo(ChargePowerChart)

function Row({ color, label, value }: { color: string; label: string; value: string }) {
  return (
    <div className="flex items-center gap-2 tabular-nums">
      <span
        className="inline-block h-2 w-2 rounded-full"
        style={{ background: color }}
        aria-hidden
      />
      <span className="text-slate-400">{label}</span>
      <span className="ml-auto font-medium">{value}</span>
    </div>
  )
}

function formatTick(ms: number): string {
  const t = new Date(ms)
  if (Number.isNaN(t.getTime())) return ""
  return t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
}

function formatTooltipTime(ms: number): string {
  const t = new Date(ms)
  if (Number.isNaN(t.getTime())) return ""
  return t.toLocaleTimeString([], {
    hour: "numeric",
    minute: "2-digit",
    second: "2-digit",
  })
}
