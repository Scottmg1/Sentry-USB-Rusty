import { useEffect, useRef, useState } from "react"
import { CalendarMonthIcon } from "@/components/icons"
import { cn } from "@/lib/utils"
import type { DatePreset, DateRange } from "@/hooks/useDrivesList"

interface DatePopoverProps {
  range: DateRange
  onChange: (r: DateRange) => void
}

const PRESETS: { value: DatePreset; label: string }[] = [
  { value: "today", label: "Today" },
  { value: "yesterday", label: "Yesterday" },
  { value: "last7", label: "Last 7 days" },
  { value: "last30", label: "Last 30 days" },
  { value: "thisYear", label: "This year" },
  { value: "lastYear", label: "Last year" },
  { value: "all", label: "All time" },
]

function presetLabel(range: DateRange): string {
  if (range.kind === "custom") {
    return `${range.start} – ${range.end}`
  }
  return PRESETS.find((p) => p.value === range.preset)?.label ?? "Last 7 days"
}

export function DatePopover({ range, onChange }: DatePopoverProps) {
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLDivElement>(null)
  const [customStart, setCustomStart] = useState(
    range.kind === "custom" ? range.start : "",
  )
  const [customEnd, setCustomEnd] = useState(
    range.kind === "custom" ? range.end : "",
  )

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener("mousedown", onDoc)
    return () => document.removeEventListener("mousedown", onDoc)
  }, [open])

  const pickPreset = (preset: DatePreset) => {
    onChange({ kind: "preset", preset })
    setOpen(false)
  }

  const applyCustom = () => {
    if (!customStart || !customEnd) return
    onChange({ kind: "custom", start: customStart, end: customEnd })
    setOpen(false)
  }

  const activePreset = range.kind === "preset" ? range.preset : null

  return (
    <div ref={wrapRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className="inline-flex items-center gap-2 rounded-full bg-emerald-500/95 px-3.5 py-1.5 text-sm font-medium text-slate-950 transition-colors hover:bg-emerald-400"
      >
        <CalendarMonthIcon className="h-4 w-4" />
        {presetLabel(range)}
      </button>
      {open && (
        <div className="absolute left-0 top-full z-50 mt-2 w-64 rounded-xl border border-white/10 bg-slate-900/95 p-2 shadow-2xl backdrop-blur">
          <div className="flex flex-col">
            {PRESETS.map((p) => {
              const active = p.value === activePreset
              return (
                <button
                  key={p.value}
                  type="button"
                  onClick={() => pickPreset(p.value)}
                  className={cn(
                    "rounded-md px-3 py-1.5 text-left text-sm transition-colors",
                    active
                      ? "bg-emerald-500/90 text-slate-950"
                      : "text-slate-300 hover:bg-white/5",
                  )}
                >
                  {p.label}
                </button>
              )
            })}
          </div>
          <div className="mt-2 border-t border-white/5 pt-2">
            <div className="px-1 text-[10px] font-semibold uppercase tracking-wider text-slate-500">
              Custom range
            </div>
            <div className="mt-1 flex flex-col gap-1.5 px-1">
              <input
                type="date"
                value={customStart}
                onChange={(e) => setCustomStart(e.target.value)}
                className="rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-xs text-slate-100 focus:border-emerald-400/40 focus:outline-none"
              />
              <input
                type="date"
                value={customEnd}
                onChange={(e) => setCustomEnd(e.target.value)}
                className="rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-xs text-slate-100 focus:border-emerald-400/40 focus:outline-none"
              />
              <button
                type="button"
                onClick={applyCustom}
                disabled={!customStart || !customEnd}
                className="rounded-md bg-emerald-500/90 px-2 py-1 text-xs font-medium text-slate-950 hover:bg-emerald-400 disabled:opacity-50"
              >
                Apply range
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}
