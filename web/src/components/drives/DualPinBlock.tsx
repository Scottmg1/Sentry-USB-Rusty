import { cn } from "@/lib/utils"
import { formatRelativeTime } from "@/lib/drive-format"
import { BatteryLevelIcon } from "./BatteryLevelIcon"

interface DualPinBlockProps {
  origin: { label: string; batteryPct?: number; timestamp: string }
  destination: { label: string; batteryPct?: number; timestamp: string }
  size?: "row" | "detail"
}

export function DualPinBlock({ origin, destination, size = "row" }: DualPinBlockProps) {
  const labelClass =
    size === "detail" ? "text-base font-semibold" : "text-sm font-semibold"
  return (
    <div className="flex min-w-0 gap-3">
      <div className="flex flex-col items-center pt-1">
        <Pin variant="origin" />
        <div className="my-1 flex-1 border-l border-dashed border-white/15" />
        <Pin variant="destination" />
      </div>
      <div className="flex min-w-0 flex-1 flex-col justify-between gap-3">
        <Row label={origin.label} batteryPct={origin.batteryPct} timestamp={origin.timestamp} labelClass={labelClass} />
        <Row label={destination.label} batteryPct={destination.batteryPct} timestamp={destination.timestamp} labelClass={labelClass} />
      </div>
    </div>
  )
}

function Pin({ variant }: { variant: "origin" | "destination" }) {
  const ring = variant === "origin" ? "ring-white/20" : "ring-emerald-400/40"
  const dot = variant === "origin" ? "bg-slate-500" : "bg-emerald-400"
  return (
    <span
      className={cn(
        "flex h-3.5 w-3.5 items-center justify-center rounded-full ring-2",
        ring,
      )}
      aria-label={variant === "origin" ? "Origin" : "Destination"}
    >
      <span className={cn("h-1.5 w-1.5 rounded-full", dot)} />
    </span>
  )
}

interface RowProps {
  label: string
  batteryPct?: number
  timestamp: string
  labelClass: string
}

function Row({ label, batteryPct, timestamp, labelClass }: RowProps) {
  return (
    <div className="min-w-0">
      <div className={cn("truncate text-slate-100", labelClass)} title={label}>
        {label}
      </div>
      <div className="mt-0.5 flex items-center gap-1.5 text-xs text-slate-500">
        {batteryPct !== undefined && (
          <span className="flex items-center gap-1">
            <BatteryLevelIcon pct={batteryPct} className="h-3 w-3" />
            {Math.round(batteryPct)}%
          </span>
        )}
        {batteryPct !== undefined && <span aria-hidden>·</span>}
        <span>{formatRelativeTime(timestamp)}</span>
      </div>
    </div>
  )
}
