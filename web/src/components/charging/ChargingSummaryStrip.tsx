import {
  AttachMoneyIcon,
  BatteryAndroidFrameBoltIcon,
  BoltIcon,
  NestEcoLeafIcon,
  ScheduleIcon,
  SpeedIcon,
} from "@/components/icons"
import { fmtDuration, fmtMoney, fmtPercent } from "@/lib/charge-format"

export interface ChargingStats {
  count: number
  totalEnergyKwh: number
  totalDurationSecs: number
  // Null means no rate-derived cost or computable efficiency is available.
  totalCost: number | null
  currency: string
  avgEfficiency: number | null
}

// Aggregates the current charging filter set.
export function ChargingSummaryStrip({
  stats,
  loading,
}: {
  stats: ChargingStats
  loading: boolean
}) {
  if (loading && stats.count === 0) {
    return (
      <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
        <div className="h-8 w-24 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-8 w-20 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-8 w-20 animate-pulse rounded-md bg-white/[0.04]" />
      </div>
    )
  }

  const avgKwh = stats.count > 0 ? stats.totalEnergyKwh / stats.count : 0

  return (
    <div className="grid grid-cols-2 gap-x-4 gap-y-3 sm:flex sm:flex-wrap sm:items-center sm:gap-x-5 sm:gap-y-2">
      <StatCell
        icon={<BatteryAndroidFrameBoltIcon className="h-3.5 w-3.5" />}
        label="Sessions"
        value={stats.count.toLocaleString()}
      />
      <Divider />
      <StatCell
        icon={<BoltIcon className="h-3.5 w-3.5 text-emerald-300" />}
        label="Energy added"
        value={`${stats.totalEnergyKwh.toFixed(1)} kWh`}
      />
      <Divider />
      <StatCell
        icon={<ScheduleIcon className="h-3.5 w-3.5" />}
        label="Time charging"
        value={fmtDuration(stats.totalDurationSecs)}
      />
      {stats.count > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<SpeedIcon className="h-3.5 w-3.5" />}
            label="Avg / session"
            value={`${avgKwh.toFixed(1)} kWh`}
          />
        </>
      )}
      {stats.avgEfficiency != null && (
        <>
          <Divider />
          <StatCell
            icon={<NestEcoLeafIcon className="h-3.5 w-3.5 text-emerald-300" />}
            label="Avg efficiency"
            value={fmtPercent(stats.avgEfficiency)}
          />
        </>
      )}
      {stats.totalCost != null && (
        <>
          <Divider />
          <StatCell
            icon={<AttachMoneyIcon className="h-3.5 w-3.5 text-emerald-300" />}
            label="Total cost"
            value={fmtMoney(stats.totalCost, stats.currency)}
          />
        </>
      )}
    </div>
  )
}

function StatCell({
  icon,
  label,
  value,
}: {
  icon: React.ReactNode
  label: string
  value: React.ReactNode
}) {
  return (
    <div className="flex min-w-0 items-center gap-2">
      <span
        className="flex h-6 w-6 shrink-0 items-center justify-center rounded-full bg-white/[0.04] ring-1 ring-inset ring-white/10 text-slate-300"
        aria-hidden
      >
        {icon}
      </span>
      <div className="min-w-0">
        <div className="text-[9px] font-semibold uppercase tracking-wider text-slate-500">
          {label}
        </div>
        <div className="text-sm font-semibold tabular-nums leading-tight text-slate-100">
          {value}
        </div>
      </div>
    </div>
  )
}

function Divider() {
  return <span aria-hidden className="hidden h-7 w-px bg-white/[0.06] sm:block" />
}
