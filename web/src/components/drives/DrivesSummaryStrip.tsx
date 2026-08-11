import { DangerousIcon, Robot2Icon, ScheduleIcon, SpeedIcon, VitalSignsIcon } from "@/components/icons"
import { formatDuration, formatPercent } from "@/lib/drive-format"
import type { DrivesFilteredStats } from "@/hooks/useDrivesList"

/** Aggregate distance with thousands separators + 1 decimal, honouring
 *  the user's metric preference. Distinct from formatDistance() which
 *  uses 2 decimals — at scale (e.g. 1,234.5 mi over a month) one
 *  decimal reads cleaner. */
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

/**
 * Compact lifetime-of-current-selection stats strip — rendered inline
 * inside DrivesToolbar between the Filter button and the Select
 * button. Numbers recompute live against the current filter set so
 * switching the date preset (or applying a tag/min-distance filter)
 * updates them immediately.
 *
 * Deliberately *does not* render a "drives count" cell — that number
 * is already shown by the pagination row ("1–10 of 26") and a date
 * badge — the active date pill in the toolbar already communicates
 * the active selection. Keeps the strip tight enough to fit on one
 * line next to the filter chrome.
 */
export function DrivesSummaryStrip({
  stats,
  loading,
  metric,
}: DrivesSummaryStripProps) {
  // While the initial fetch is in flight render a skeleton so the
  // toolbar row keeps its height. On subsequent refreshes (post
  // process / import) we keep showing the previous numbers rather
  // than flashing back to a skeleton.
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
