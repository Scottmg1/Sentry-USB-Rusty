import { useState } from "react"
import { InfoIcon } from "@/components/icons"
import { cn } from "@/lib/utils"

interface StatTileProps {
  label: string
  value: React.ReactNode
  icon: React.ReactNode
  info?: string
  star?: boolean
  size?: "section" | "headline"
}

export function StatTile({ label, value, icon, info, star, size = "section" }: StatTileProps) {
  const [tipOpen, setTipOpen] = useState(false)
  const isHeadline = size === "headline"
  return (
    <div className="flex items-center gap-3">
      <span
        className={cn(
          "flex shrink-0 items-center justify-center rounded-full bg-white/[0.04] ring-1 ring-inset ring-white/10 text-slate-300",
          isHeadline ? "h-10 w-10" : "h-8 w-8",
        )}
        aria-hidden
      >
        {icon}
      </span>
      <div className="min-w-0">
        <div className="flex items-center gap-1 text-[10px] font-semibold uppercase tracking-wider text-slate-500">
          <span>{label}</span>
          {info && (
            <span className="relative inline-block">
              <button
                type="button"
                aria-label={`${label} info`}
                onMouseEnter={() => setTipOpen(true)}
                onMouseLeave={() => setTipOpen(false)}
                onClick={() => setTipOpen((o) => !o)}
                className="inline-flex h-3 w-3 items-center justify-center text-slate-600 hover:text-slate-400"
              >
                <InfoIcon className="h-3 w-3" />
              </button>
              {tipOpen && (
                <span className="absolute left-1/2 top-full z-50 mt-1 w-48 -translate-x-1/2 rounded-md border border-white/10 bg-slate-900/95 px-2 py-1 text-[11px] normal-case tracking-normal text-slate-300 shadow-2xl">
                  {info}
                </span>
              )}
            </span>
          )}
        </div>
        <div
          className={cn(
            "mt-0.5 font-semibold text-slate-100 tabular-nums",
            isHeadline ? "text-2xl" : "text-lg",
          )}
        >
          {value}
          {star && <span className="ml-1 text-amber-300">★</span>}
        </div>
      </div>
    </div>
  )
}

export function SectionHeading({ children }: { children: React.ReactNode }) {
  return (
    <h2 className="mt-8 mb-3 text-lg font-semibold text-slate-100">{children}</h2>
  )
}
