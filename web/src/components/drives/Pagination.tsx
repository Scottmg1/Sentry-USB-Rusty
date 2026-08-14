import { ChevronLeftIcon, ChevronRightIcon, FirstPageIcon, LastPageIcon } from "@/components/icons"
import { cn } from "@/lib/utils"

interface PaginationProps {
  page: number
  pageCount: number
  pageStart: number
  pageEnd: number
  total: number
  onChange: (page: number) => void
}

export function Pagination({ page, pageCount, pageStart, pageEnd, total, onChange }: PaginationProps) {
  const atStart = page <= 1
  const atEnd = page >= pageCount

  return (
    <div className="flex items-center gap-2 text-sm text-slate-400">
      <Btn label="First page" disabled={atStart} onClick={() => onChange(1)}>
        <FirstPageIcon className="h-4 w-4" />
      </Btn>
      <Btn label="Previous page" disabled={atStart} onClick={() => onChange(page - 1)}>
        <ChevronLeftIcon className="h-4 w-4" />
      </Btn>
      <span className="px-1 tabular-nums text-slate-300">
        {total === 0 ? "0 of 0" : `${pageStart}–${pageEnd} of ${total}`}
      </span>
      <Btn label="Next page" disabled={atEnd} onClick={() => onChange(page + 1)}>
        <ChevronRightIcon className="h-4 w-4" />
      </Btn>
      <Btn label="Last page" disabled={atEnd} onClick={() => onChange(pageCount)}>
        <LastPageIcon className="h-4 w-4" />
      </Btn>
    </div>
  )
}

interface BtnProps {
  label: string
  disabled: boolean
  onClick: () => void
  children: React.ReactNode
}

function Btn({ label, disabled, onClick, children }: BtnProps) {
  return (
    <button
      type="button"
      aria-label={label}
      onClick={onClick}
      disabled={disabled}
      className={cn(
        "rounded-md p-1 transition-colors",
        disabled
          ? "text-slate-700"
          : "text-slate-400 hover:bg-white/5 hover:text-slate-200",
      )}
    >
      {children}
    </button>
  )
}
