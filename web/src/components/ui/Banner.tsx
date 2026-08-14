import type { ReactNode } from "react"
import { cn } from "@/lib/utils"

type BannerKind = "error" | "warn" | "info" | "update"

const HALO_BY_KIND: Record<BannerKind, string> = {
  error: "halo-red",
  warn: "halo-amber",
  info: "halo-blue",
  update: "halo-accent",
}

const TITLE_COLOR_BY_KIND: Record<BannerKind, string> = {
  error: "#fca5a5",
  warn: "#fcd34d",
  info: "#bfdbfe",
  update: "oklch(0.86 0.16 150)",
}

export interface BannerItem {
  /** Stable key for the stack — pass the same value across renders. */
  id: string
  kind: BannerKind
  icon: ReactNode
  title: ReactNode
  sub?: ReactNode
  action?: ReactNode
}

function Banner({ kind, icon, title, sub, action }: Omit<BannerItem, "id">) {
  return (
    <div className={cn("banner", `banner--${kind}`)}>
      <span className={cn("banner-icon", HALO_BY_KIND[kind])}>{icon}</span>
      <div className="banner-body">
        <div className="banner-title" style={{ color: TITLE_COLOR_BY_KIND[kind] }}>
          {title}
        </div>
        {sub && <div className="banner-sub">{sub}</div>}
      </div>
      {action}
    </div>
  )
}

// Callers supply priority order; the stack preserves it.
export function BannerStack({ banners }: { banners: BannerItem[] }) {
  if (banners.length === 0) return null
  return (
    <div className="flex flex-col gap-1.5">
      {banners.map((b) => (
        <Banner
          key={b.id}
          kind={b.kind}
          icon={b.icon}
          title={b.title}
          sub={b.sub}
          action={b.action}
        />
      ))}
    </div>
  )
}
