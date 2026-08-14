import { useMemo, useRef } from "react"
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
import { useScrubberActions } from "@/hooks/useScrubberSync"

// Keep these constants aligned with chart props; seeking uses explicit pixel
// bounds because Recharts 3 exposes click state before it settles.
const LEFT_MARGIN = 4
const RIGHT_MARGIN = 16
const YAXIS_WIDTH = 44
const X_PADDING_LEFT = 10
const X_PADDING_RIGHT = 4

export interface TemperaturePoint {
  // Unix milliseconds.
  ts: number
  // Undefined temperatures render as gaps rather than zero.
  interiorC?: number
  exteriorC?: number
}

interface TemperatureChartProps {
  points: TemperaturePoint[]
  // Distance-system preference also selects Celsius versus Fahrenheit.
  metric: boolean
  // Optional drive timing enables click-to-seek.
  drivePoints?: [number, number, number, number][]
  startTime?: string
}

const INTERIOR_COLOR = "#f97316"
const EXTERIOR_COLOR = "#38bdf8"

export default function TemperatureChart({
  points,
  metric,
  drivePoints,
  startTime,
}: TemperatureChartProps) {
  const { setIndex } = useScrubberActions()
  // Convert once at the boundary while preserving undefined gaps.
  const converted = useMemo(() => {
    if (metric) return points
    return points.map((p) => ({
      ts: p.ts,
      interiorC: p.interiorC === undefined ? undefined : (p.interiorC * 9) / 5 + 32,
      exteriorC: p.exteriorC === undefined ? undefined : (p.exteriorC * 9) / 5 + 32,
    }))
  }, [points, metric])

  // Convert absolute sample time to a relative offset for binary search.
  const baseMs = useMemo(
    () => (startTime ? new Date(startTime).getTime() : NaN),
    [startTime],
  )

  const canSeek = !!drivePoints && drivePoints.length > 0 && Number.isFinite(baseMs)
  const containerRef = useRef<HTMLDivElement>(null)

  const seekToTs = (ts: number) => {
    if (!drivePoints || drivePoints.length === 0) return
    const targetRel = ts - baseMs
    let lo = 0
    let hi = drivePoints.length - 1
    while (lo < hi) {
      const mid = (lo + hi) >> 1
      if (drivePoints[mid][2] < targetRel) lo = mid + 1
      else hi = mid
    }
    let best = lo
    if (lo > 0) {
      const a = Math.abs(drivePoints[lo - 1][2] - targetRel)
      const b = Math.abs(drivePoints[lo][2] - targetRel)
      if (a < b) best = lo - 1
    }
    setIndex(best)
  }

  // Sparse samples require time-axis mapping rather than direct index mapping.
  const handleClick = (e: React.MouseEvent<HTMLDivElement>) => {
    if (!canSeek || converted.length < 2) return
    const container = containerRef.current
    if (!container) return
    const rect = container.getBoundingClientRect()
    const plotLeft = LEFT_MARGIN + YAXIS_WIDTH + X_PADDING_LEFT
    const plotRight = rect.width - RIGHT_MARGIN - X_PADDING_RIGHT
    const plotWidth = plotRight - plotLeft
    if (plotWidth <= 0) return
    const x = e.clientX - rect.left
    const clamped = Math.max(plotLeft, Math.min(plotRight, x))
    const frac = (clamped - plotLeft) / plotWidth
    const tMin = converted[0].ts
    const tMax = converted[converted.length - 1].ts
    if (!(tMax > tMin)) return
    seekToTs(tMin + (tMax - tMin) * frac)
  }

  const unit = metric ? "°C" : "°F"

  return (
    <div
      ref={containerRef}
      className={
        canSeek ? "h-56 w-full cursor-crosshair select-none" : "h-56 w-full"
      }
      onClick={canSeek ? handleClick : undefined}
      aria-label="Temperature chart"
    >
      <ResponsiveContainer minHeight={0} minWidth={0}>
        <LineChart
          data={converted}
          margin={{ top: 10, right: RIGHT_MARGIN, bottom: 24, left: LEFT_MARGIN }}
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
            padding={{ left: X_PADDING_LEFT, right: X_PADDING_RIGHT }}
          />
          <YAxis
            stroke="#475569"
            tick={{ fill: "#64748b", fontSize: 11 }}
            tickFormatter={(n: number) => `${Math.round(n)}${unit}`}
            tickLine={false}
            axisLine={false}
            tickMargin={4}
            width={YAXIS_WIDTH}
            domain={["dataMin - 2", "dataMax + 2"]}
          />
          <Tooltip
            content={({ active, payload }) => {
              if (!active || !payload || payload.length === 0) return null
              const p = payload[0].payload as TemperaturePoint
              return (
                <div className="rounded-md border border-white/10 bg-slate-900/95 px-2 py-1.5 text-xs text-slate-200 shadow-xl">
                  <div className="mb-1 text-[10px] text-slate-500 tabular-nums">
                    {formatTooltipTime(p.ts)}
                  </div>
                  {p.interiorC !== undefined && (
                    <div className="flex items-center gap-2 tabular-nums">
                      <span
                        className="inline-block h-2 w-2 rounded-full"
                        style={{ background: INTERIOR_COLOR }}
                        aria-hidden
                      />
                      <span className="text-slate-400">Interior</span>
                      <span className="ml-auto font-medium">
                        {Math.round(p.interiorC)}
                        {unit}
                      </span>
                    </div>
                  )}
                  {p.exteriorC !== undefined && (
                    <div className="flex items-center gap-2 tabular-nums">
                      <span
                        className="inline-block h-2 w-2 rounded-full"
                        style={{ background: EXTERIOR_COLOR }}
                        aria-hidden
                      />
                      <span className="text-slate-400">Exterior</span>
                      <span className="ml-auto font-medium">
                        {Math.round(p.exteriorC)}
                        {unit}
                      </span>
                    </div>
                  )}
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
            name="Interior"
            dataKey="interiorC"
            stroke={INTERIOR_COLOR}
            strokeWidth={2}
            dot={false}
            isAnimationActive={false}
            connectNulls
          />
          <Line
            type="monotone"
            name="Exterior"
            dataKey="exteriorC"
            stroke={EXTERIOR_COLOR}
            strokeWidth={2}
            dot={false}
            isAnimationActive={false}
            connectNulls
          />
        </LineChart>
      </ResponsiveContainer>
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
