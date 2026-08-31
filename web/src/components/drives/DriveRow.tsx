import { Robot2Icon, ScheduleIcon, SettingsRemoteIcon, SpeedIcon, VerifiedUserIcon } from "@/components/icons"
import { useNavigate } from "react-router-dom"
import { cn } from "@/lib/utils"
import { formatDistance, formatDuration, formatPercent } from "@/lib/drive-format"
import type { DriveSummary } from "@/types/drives"
import { DualPinBlock } from "./DualPinBlock"
import { MiniRouteMap } from "./MiniRouteMap"
import { TagPopover } from "./TagPopover"

interface DriveRowProps {
  drive: DriveSummary
  routePoints: [number, number][]
  metric: boolean
  selectMode: boolean
  selected: boolean
  onToggleSelected: (id: number) => void
  onTagsChange: (id: number, tags: string[]) => Promise<void>
}

export function DriveRow({
  drive,
  routePoints,
  metric,
  selectMode,
  selected,
  onToggleSelected,
  onTagsChange,
}: DriveRowProps) {
  const navigate = useNavigate()
  const onCardClick = () => {
    if (selectMode) {
      onToggleSelected(drive.id)
      return
    }
    navigate(`/drives/${drive.id}`)
  }

  const originLabel = drive.startLocation ?? formatGps(drive.startPoint) ?? "Unknown origin"
  const destinationLabel = drive.endLocation ?? formatGps(drive.endPoint) ?? "Unknown destination"
  const fsdDisplay = formatPercent(drive.fsdPercent)
  const fsdFull = drive.fsdPercent >= 100

  return (
    <div
      role="button"
      tabIndex={0}
      onClick={onCardClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault()
          onCardClick()
        }
      }}
      className={cn(
        "group relative flex cursor-pointer items-stretch gap-4 rounded-2xl border bg-white/[0.025] p-4 transition-all",
        selected
          ? "border-emerald-400/40 bg-emerald-400/[0.05]"
          : "border-white/[0.06] hover:border-emerald-400/30 hover:bg-white/[0.04]",
      )}
    >
      {selectMode && (
        <div className="flex items-center pr-1">
          <span
            aria-hidden
            className={cn(
              "flex h-5 w-5 items-center justify-center rounded border-2 transition-colors",
              selected
                ? "border-emerald-400 bg-emerald-400"
                : "border-white/30 bg-transparent",
            )}
          >
            {selected && (
              <svg viewBox="0 0 12 12" className="h-3 w-3 text-slate-950">
                <path
                  d="M2 6.5 L5 9.5 L10 3.5"
                  stroke="currentColor"
                  strokeWidth="2"
                  fill="none"
                />
              </svg>
            )}
          </span>
        </div>
      )}

      <div className="min-w-0 flex-1">
        <DualPinBlock
          origin={{
            label: originLabel,
            batteryPct: drive.batteryPctStart,
            timestamp: drive.startTime,
          }}
          destination={{
            label: destinationLabel,
            batteryPct: drive.batteryPctEnd,
            timestamp: drive.endTime,
          }}
        />
      </div>

      <div className="flex flex-col items-end justify-between gap-2">
        <div className="flex flex-wrap items-center justify-end gap-1.5">
          <Chip>
            <SpeedIcon className="h-3.5 w-3.5" />
            {formatDistance(drive.distanceMi, drive.distanceKm, metric)}
          </Chip>
          <Chip>
            <ScheduleIcon className="h-3.5 w-3.5" />
            {formatDuration(drive.durationMs)}
          </Chip>
          {drive.summon ? (
            <Chip summon>
              <SettingsRemoteIcon className="h-3.5 w-3.5" />
              Summon
            </Chip>
          ) : (
            <Chip emphasis>
              <Robot2Icon className="h-3.5 w-3.5" />
              FSD {fsdDisplay}%
              {fsdFull && (
                <span className="ml-0.5 text-amber-300" aria-hidden>
                  ★
                </span>
              )}
            </Chip>
          )}
          {typeof drive.safetyScore === "number" && (
            <Chip
              title="Tesla v2.2 Estimate for this drive"
              className={
                drive.safetyScore >= 90
                  ? "bg-emerald-400/15 text-emerald-200 ring-emerald-400/20"
                  : drive.safetyScore >= 70
                    ? "bg-blue-400/15 text-blue-200 ring-blue-400/20"
                    : drive.safetyScore >= 40
                      ? "bg-amber-400/15 text-amber-200 ring-amber-400/20"
                      : "bg-red-400/15 text-red-200 ring-red-400/20"
              }
            >
              <VerifiedUserIcon className="h-3.5 w-3.5" />
              {Math.round(drive.safetyScore)}
            </Chip>
          )}
        </div>
        <div className="flex items-end gap-2">
          <div
            onClick={(e) => e.stopPropagation()}
            onKeyDown={(e) => e.stopPropagation()}
          >
            <TagPopover
              tags={drive.tags ?? []}
              onChange={(tags) => onTagsChange(drive.id, tags)}
            />
          </div>
          <MiniRouteMap points={routePoints} source={drive.source} />
        </div>
      </div>
    </div>
  )
}

interface ChipProps {
  children: React.ReactNode
  title?: string
  emphasis?: boolean
  summon?: boolean
  /** Explicit color classes (bg/text/ring); overrides the variant flags. */
  className?: string
}

function Chip({ children, title, emphasis, summon, className }: ChipProps) {
  return (
    <span
      title={title}
      className={cn(
        "inline-flex items-center gap-1 rounded-full px-2 py-0.5 text-xs font-medium tabular-nums ring-1 ring-inset",
        className ??
          (summon
            ? "bg-violet-400/15 text-violet-200 ring-violet-400/20"
            : emphasis
              ? "bg-emerald-400/15 text-emerald-200 ring-emerald-400/20"
              : "bg-white/5 text-slate-300 ring-white/10"),
      )}
    >
      {children}
    </span>
  )
}

function formatGps(point: [number, number] | null): string | null {
  if (!point) return null
  return `${point[0].toFixed(4)}, ${point[1].toFixed(4)}`
}
