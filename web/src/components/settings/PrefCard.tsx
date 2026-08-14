import { Children, useLayoutEffect, useRef, type ReactNode } from "react"
import { cn } from "@/lib/utils"
import type { Halo } from "@/components/ui/StatusTile"
import { SectionErrorBoundary } from "@/components/ErrorBoundary"

/** Content shown over a disabled card body while its header remains visible. */
export interface DisabledConfig {
  /** Centered text explaining what the user needs to enable. */
  reason: string
  /** Optional "go enable this" affordance. */
  cta?: {
    label: string
    onClick?: () => void
    href?: string
  }
}

interface PrefCardProps {
  icon: ReactNode
  halo?: Halo
  title: ReactNode
  badge?: ReactNode
  footer?: ReactNode
  className?: string
  /**
   * Keeps disabled children mounted behind an inert overlay. `inert` also
   * removes controls from keyboard and assistive-technology navigation.
   */
  disabled?: DisabledConfig
  children: ReactNode
}

export function PrefCard({
  icon,
  halo = "slate",
  title,
  badge,
  footer,
  className,
  disabled,
  children,
}: PrefCardProps) {
  return (
    <div
      className={cn("glass-card overflow-hidden", className)}
      aria-disabled={disabled ? true : undefined}
      data-disabled={disabled ? "true" : undefined}
    >
      <div className="flex items-center gap-2.5 border-b border-white/5 px-3.5 py-2.5">
        <span
          className={cn("halo-" + halo, "inline-flex h-7 w-7 shrink-0 items-center justify-center rounded-lg")}
        >
          {icon}
        </span>
        <span className="t-md font-semibold">{title}</span>
        {badge && <span className="ml-auto">{badge}</span>}
      </div>
      {disabled ? (
        <div className="relative">
          {/* Keep control state mounted while the scrim makes content unreadable. */}
          <div
            inert
            aria-hidden
            className="flex flex-col gap-2.5 p-3.5 blur-[3px] opacity-20 select-none"
          >
            {children}
          </div>
          <div className="absolute inset-0 flex flex-col items-center justify-center gap-2.5 bg-gradient-to-b from-slate-900/75 to-slate-950/85 p-4 text-center backdrop-blur-[1px]">
            <p className="max-w-[30ch] text-xs leading-relaxed text-slate-300">{disabled.reason}</p>
            {disabled.cta && <DisabledCta cta={disabled.cta} />}
          </div>
        </div>
      ) : (
        <div className="flex flex-col gap-2.5 p-3.5">{children}</div>
      )}
      {footer && (
        <div className="border-t border-white/5 px-3.5 py-2.5">{footer}</div>
      )}
    </div>
  )
}

function DisabledCta({ cta }: { cta: NonNullable<DisabledConfig["cta"]> }) {
  const cls =
    "rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 transition-colors hover:bg-blue-500/25"
  if (cta.href) {
    return (
      <a href={cta.href} className={cls}>
        {cta.label}
      </a>
    )
  }
  return (
    <button type="button" onClick={cta.onClick} className={cls}>
      {cta.label}
    </button>
  )
}

/** Must match PrefGrid's gap-2.5 spacing. */
const GRID_GAP_PX = 10

/** Greedily pack ordered heights under a column cap. */
function packWithin(heights: number[], cap: number, max: number): number[] | null {
  const runs: number[] = []
  let count = 0
  let h = 0
  for (const x of heights) {
    if (count > 0 && h + GRID_GAP_PX + x > cap) {
      runs.push(count)
      count = 1
      h = x
    } else {
      count += 1
      h += count === 1 ? x : GRID_GAP_PX + x
    }
  }
  if (count > 0) runs.push(count)
  return runs.length <= max ? runs : null
}

/** Balance ordered items into contiguous columns using the smallest feasible cap. */
function balancedRuns(heights: number[], n: number): number[] {
  let lo = Math.max(0, ...heights)
  let hi = heights.reduce((a, b) => a + b, 0) + GRID_GAP_PX * Math.max(0, heights.length - 1)
  let best = packWithin(heights, hi, n) ?? [heights.length]
  while (lo < hi) {
    const mid = Math.floor((lo + hi) / 2)
    const runs = packWithin(heights, mid, n)
    if (runs) {
      best = runs
      hi = mid
    } else {
      lo = mid + 1
    }
  }
  while (best.length < n) best.push(0)
  return best
}

/**
 * Measured masonry preserves source order and fixed-width cards. CSS
 * multi-column mispaints filtered, clipped, positioned cards in WebKit and
 * older in-car Chromium, so flattened children are positioned without remounting.
 */
export function PrefGrid({ children, min = 340 }: { children: ReactNode; min?: number }) {
  const ref = useRef<HTMLDivElement>(null)

  // Reattach observers after renders that may change flattened card children.
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return

    const ro = new ResizeObserver(() => layout())
    const layout = () => {
      const kids = Array.from(el.children) as HTMLElement[]
      for (const kid of kids) ro.observe(kid)
      if (kids.length === 0) {
        el.style.height = "0px"
        return
      }
      const width = el.clientWidth
      const n = Math.max(1, Math.floor((width + GRID_GAP_PX) / (min + GRID_GAP_PX)))
      const colWidth = (width - (n - 1) * GRID_GAP_PX) / n

      // Batch width writes before height reads; equality guards prevent loops.
      for (const kid of kids) {
        const w = `${colWidth}px`
        if (kid.style.width !== w) kid.style.width = w
        if (kid.style.position !== "absolute") kid.style.position = "absolute"
      }
      const heights = kids.map((kid) => kid.offsetHeight)

      const runs = balancedRuns(heights, n)
      let i = 0
      let tallest = 0
      runs.forEach((count, col) => {
        const left = `${col * (colWidth + GRID_GAP_PX)}px`
        let y = 0
        for (let j = 0; j < count; j++, i++) {
          const kid = kids[i]
          if (kid.style.left !== left) kid.style.left = left
          const top = `${y}px`
          if (kid.style.top !== top) kid.style.top = top
          y += heights[i] + GRID_GAP_PX
        }
        tallest = Math.max(tallest, y - GRID_GAP_PX)
      })
      const h = `${tallest}px`
      if (el.style.height !== h) el.style.height = h
    }

    layout()
    ro.observe(el)
    const mo = new MutationObserver(layout)
    mo.observe(el, { childList: true })
    return () => {
      ro.disconnect()
      mo.disconnect()
    }
  })

  return (
    <div ref={ref} className="relative">
      {/* Boundaries add no wrapper, preserving flattened-card measurement. */}
      {Children.map(children, (child) => (
        <SectionErrorBoundary>{child}</SectionErrorBoundary>
      ))}
    </div>
  )
}
