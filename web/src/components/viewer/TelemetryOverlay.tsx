import { memo } from "react"
import type { TelemetryFrame } from "@/lib/api"
import { useDraggable } from "@/hooks/useDraggable"

const GEAR_LABELS: Record<number, { text: string; color: string }> = {
  0: { text: "P", color: "bg-blue-500/80 text-blue-100" },
  1: { text: "D", color: "bg-emerald-500/80 text-emerald-100" },
  2: { text: "R", color: "bg-red-500/80 text-red-100" },
  3: { text: "N", color: "bg-amber-500/80 text-amber-100" },
}

interface TelemetryOverlayProps {
  frame: TelemetryFrame | null
  metric?: boolean
}

export default memo(function TelemetryOverlay({ frame, metric = false }: TelemetryOverlayProps) {
  const { ref, dragProps } = useDraggable({ initialAnchor: "bottom-center" })

  if (!frame) return null

  const speedVal = frame.speed_mps * (metric ? 3.6 : 2.237)
  const speed = Math.round(Math.abs(speedVal))
  const unit = metric ? "km/h" : "mph"
  const gear = GEAR_LABELS[frame.gear] || GEAR_LABELS[0]
  const apState = frame.autopilot
  const isAssisted = apState > 0
  const apLabel = apState === 1 ? "FSD" : apState === 2 ? "Autopilot" : apState === 3 ? "TACC" : ""
  const accelPct = Math.round(Math.min(Math.max(frame.accel_pos * 100, 0), 100))

  return (
    <div
      ref={ref}
      {...dragProps}
      className="z-10 flex items-center gap-3 rounded-xl border border-white/10 bg-black/60 px-4 py-2 backdrop-blur-md select-none"
    >
      {/* Speed */}
      <div className="text-center">
        <span className="text-2xl font-bold tabular-nums text-white">{speed}</span>
        <span className="ml-1 text-[10px] text-slate-400">{unit}</span>
      </div>

      {/* Divider */}
      <div className="h-6 w-px bg-white/10" />

      {/* Gear pill */}
      <span className={`rounded-md px-2 py-0.5 text-xs font-bold ${gear.color}`}>
        {gear.text}
      </span>

      {/* Autopilot mode indicator */}
      {isAssisted && (
        <>
          <div className="h-6 w-px bg-white/10" />
          <div className="flex items-center gap-1">
            <span className="h-2 w-2 animate-pulse rounded-full bg-emerald-400" />
            <span className="text-[10px] font-semibold text-emerald-400">{apLabel}</span>
          </div>
        </>
      )}

      {/* Accel bar */}
      {accelPct > 0 && (
        <>
          <div className="h-6 w-px bg-white/10" />
          <div className="flex items-center gap-1.5">
            <div className="h-1 w-12 overflow-hidden rounded-full bg-white/10">
              <div
                className="h-full rounded-full bg-emerald-400 transition-all duration-100"
                style={{ width: `${accelPct}%` }}
              />
            </div>
            <span className="text-[9px] tabular-nums text-slate-500">{accelPct}%</span>
          </div>
        </>
      )}
    </div>
  )
})
