import { CheckBoxIcon } from "@/components/icons"
import type {
  DateRange,
  DrivesFilteredStats,
  DrivesFilters,
} from "@/hooks/useDrivesList"
import type { DriveSummary } from "@/types/drives"
import { DatePopover } from "./DatePopover"
import { DrivesSummaryStrip } from "./DrivesSummaryStrip"
import { FilterPopover } from "./FilterPopover"
import { SelectModeBar } from "./SelectModeBar"

interface DrivesToolbarProps {
  drives: DriveSummary[]
  range: DateRange
  filters: DrivesFilters
  onRangeChange: (r: DateRange) => void
  onFiltersChange: (f: DrivesFilters) => void
  selectMode: boolean
  onToggleSelectMode: () => void
  selectedCount: number
  totalCount: number
  onSelectAll: () => void
  onTagSelected: () => void
  onExportSelected: () => void
  onDeleteSelected: () => void
  // Match the filter distance field to the drive-row unit.
  metric: boolean
  filteredStats: DrivesFilteredStats
  loading: boolean
}

export function DrivesToolbar(props: DrivesToolbarProps) {
  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
      <DatePopover range={props.range} onChange={props.onRangeChange} />
      <FilterPopover
        drives={props.drives}
        filters={props.filters}
        onChange={props.onFiltersChange}
        metric={props.metric}
      />
      {/* Hide inline stats while bulk actions own the row. */}
      {!props.selectMode && (
        <div className="ml-3 min-w-0 flex-1">
          <DrivesSummaryStrip
            stats={props.filteredStats}
            loading={props.loading}
            metric={props.metric}
          />
        </div>
      )}
      <div className="ml-auto flex flex-wrap items-center gap-2">
        {props.selectMode ? (
          <SelectModeBar
            selectedCount={props.selectedCount}
            totalCount={props.totalCount}
            onSelectAll={props.onSelectAll}
            onTag={props.onTagSelected}
            onExport={props.onExportSelected}
            onDelete={props.onDeleteSelected}
            onCancel={props.onToggleSelectMode}
          />
        ) : (
          <button
            type="button"
            onClick={props.onToggleSelectMode}
            className="inline-flex items-center gap-2 rounded-full border border-white/10 bg-white/[0.03] px-3.5 py-1.5 text-sm font-medium text-slate-200 transition-colors hover:bg-white/[0.06]"
          >
            <CheckBoxIcon className="h-4 w-4" />
            Select
          </button>
        )}
      </div>
    </div>
  )
}
