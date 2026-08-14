import { DangerousIcon, Robot2Icon, ScheduleIcon, SpeedIcon, VitalSignsIcon } from "@/components/icons"
import { formatDuration, formatPercent } from "@/lib/drive-format"
import type { DrivesFilteredStats } from "@/hooks/useDrivesList"

/** Aggregate distance with grouping and one decimal in the preferred unit. */
function formatAggregateDistance(
  mi: number,
  km: number,
  metric: boolean,
): string {
  const value = metric ? km : mi
  const unit = metric ? "km" : "mi"
  return `${value.toLocaleString(undefined, {
    minimumFractionDigits: 1,
    maximumFractionDigits: 1,
  })} ${unit}`
}

interface DrivesSummaryStripProps {
  stats: DrivesFilteredStats
  loading: boolean
  metric: boolean
}

/** Aggregate statistics for the current filter selection. */
export function DrivesSummaryStrip({
  stats,
  loading,
  metric,
}: DrivesSummaryStripProps) {
  // Preserve prior values during refreshes; only cold loads need a skeleton.
  if (loading && stats.count === 0) {
    return (
      <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
        <div className="h-8 w-24 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-8 w-20 animate-pulse rounded-md bg-white/[0.04]" />
        <div className="h-8 w-20 animate-pulse rounded-md bg-white/[0.04]" />
      </div>
    )
  }

  return (
    <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
      <StatCell
        icon={<SpeedIcon className="h-3.5 w-3.5" />}
        label="Distance"
        value={formatAggregateDistance(
          stats.totalDistanceMi,
          stats.totalDistanceKm,
          metric,
        )}
      />
      <Divider />
      <StatCell
        icon={<ScheduleIcon className="h-3.5 w-3.5" />}
        label="Time"
        value={formatDuration(stats.totalDurationMs)}
      />
      {stats.fsdEngagedMs > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<Robot2Icon className="h-3.5 w-3.5 text-emerald-300" />}
            label="FSD"
            value={`${formatPercent(stats.fsdPercent)}%`}
            highlight={stats.fsdPercent >= 99}
          />
        </>
      )}
      {stats.autopilotEngagedMs > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<VitalSignsIcon className="h-3.5 w-3.5" />}
            label="Autopilot"
            value={`${formatPercent(stats.autopilotPercent)}%`}
          />
        </>
      )}
      {stats.fsdDisengagements > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<DangerousIcon className="h-3.5 w-3.5 text-rose-300" />}
            label="Disengagements"
            value={stats.fsdDisengagements.toLocaleString()}
          />
        </>
      )}
      {stats.tessieCount > 0 && (
        <>
          <Divider />
          <StatCell
            icon={<Robot2Icon className="h-3.5 w-3.5 text-violet-300" />}
            label="Tessie"
            value={stats.tessieCount.toLocaleString()}
          />
        </>
      )}
    </div>
  )
}

interface StatCellProps {
  icon: React.ReactNode
  label: string
  value: React.ReactNode
  highlight?: boolean
}

function StatCell({ icon, label, value, highlight }: StatCellProps) {
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
        <div
          className={
            "text-sm font-semibold tabular-nums leading-tight " +
            (highlight ? "text-emerald-300" : "text-slate-100")
          }
        >
          {value}
        </div>
      </div>
    </div>
  )
}

function Divider() {
  return <span aria-hidden className="hidden h-7 w-px bg-white/[0.06] sm:block" />
}
