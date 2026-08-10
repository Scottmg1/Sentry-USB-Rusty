import { useEffect, useRef, useState } from "react"
import { CheckIcon, SellIcon } from "@/components/icons"
import { cn } from "@/lib/utils"

// Tag filter for the Charging page, mirroring the Drives filter UX: a
// pill that opens a checklist of every charge tag. Selecting tags keeps
// only sessions carrying at least one of them (union match).
export function ChargingTagFilter({
  tags,
  selected,
  onChange,
}: {
  tags: string[]
  selected: string[]
  onChange: (next: string[]) => void
}) {
  const [open, setOpen] = useState(false)
  const wrapRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener("mousedown", onDoc)
    return () => document.removeEventListener("mousedown", onDoc)
  }, [open])

  const toggle = (t: string) =>
    onChange(
      selected.includes(t)
        ? selected.filter((x) => x !== t)
        : [...selected, t],
    )

  const count = selected.length

  return (
    <div ref={wrapRef} className="relative">
      <button
        type="button"
        onClick={() => setOpen((o) => !o)}
        className={cn(
          "inline-flex items-center gap-1.5 rounded-lg border px-2.5 py-1.5 text-xs font-medium transition-colors",
          count > 0
            ? "border-emerald-400/30 bg-emerald-400/10 text-emerald-200 hover:bg-emerald-400/15"
            : "border-white/10 bg-white/[0.03] text-slate-300 hover:bg-white/[0.06]",
        )}
      >
        <SellIcon className="h-3.5 w-3.5" />
        Tags
        {count > 0 && (
          <span className="rounded-full bg-emerald-400/20 px-1.5 text-[10px] text-emerald-100">
            {count}
          </span>
        )}
      </button>
      {open && (
        <div className="absolute left-0 top-full z-50 mt-2 w-56 rounded-xl border border-white/10 bg-slate-900/95 p-2 shadow-2xl backdrop-blur">
          {tags.length === 0 ? (
            <p className="px-2 py-1.5 text-xs text-slate-500">
              No tags yet. Tag a charge to filter by it.
            </p>
          ) : (
            <div className="flex max-h-64 flex-col gap-0.5 overflow-y-auto">
              {tags.map((t) => {
                const on = selected.includes(t)
                return (
                  <button
                    key={t}
                    type="button"
                    onClick={() => toggle(t)}
                    className="flex items-center gap-2 rounded-md px-2 py-1.5 text-left text-sm text-slate-200 hover:bg-white/5"
                  >
                    <span
                      className={cn(
                        "flex h-4 w-4 items-center justify-center rounded border",
                        on
                          ? "border-emerald-400 bg-emerald-400/20 text-emerald-200"
                          : "border-white/15 text-transparent",
                      )}
                    >
                      <CheckIcon className="h-3 w-3" />
                    </span>
                    <span className="min-w-0 flex-1 truncate">{t}</span>
                  </button>
                )
              })}
            </div>
          )}
          {count > 0 && (
            <button
              type="button"
              onClick={() => onChange([])}
              className="mt-1 w-full rounded-md bg-white/[0.04] px-2 py-1 text-xs text-slate-300 hover:bg-white/[0.08]"
            >
              Clear
            </button>
          )}
        </div>
      )}
    </div>
  )
}
