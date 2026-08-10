import { useState, useEffect } from "react"
import { Outlet } from "react-router-dom"
import { MenuIcon } from "@/components/icons"
import { Sidebar } from "./Sidebar"
import { MobileNav } from "./MobileNav"
import { ConnectionBanner } from "./ConnectionBanner"
import { cn } from "@/lib/utils"
import { KeepAwakeProvider } from "@/hooks/useKeepAwake"
import { AwayModeProvider } from "@/hooks/useAwayMode"
import { ConnectionProvider } from "@/hooks/useConnectionStatus"

// Routes likely to be visited after the Dashboard. Prefetched on idle
// so navigation is instant. Heavy/rare routes (Terminal, Viewer with
// leaflet, Community wraps) are intentionally NOT in this list — we
// don't want to burn data on screens the user may never open.
const PREFETCH_ROUTES: Array<() => Promise<unknown>> = [
  () => import("@/pages/Drives"),
  () => import("@/pages/Files"),
  () => import("@/pages/Settings"),
  () => import("@/pages/Logs"),
]

export function AppShell() {
  // Persist the collapse choice so it survives reloads (previously reset
  // to expanded every refresh).
  const [sidebarCollapsed, setSidebarCollapsed] = useState(() => {
    try {
      return localStorage.getItem("sidebar-collapsed") === "1"
    } catch {
      return false
    }
  })
  const [mobileNavOpen, setMobileNavOpen] = useState(false)

  // Warm the module cache for likely-next routes on idle. Respects
  // Save-Data and effectively-slow connections so we don't punish
  // metered users.
  useEffect(() => {
    const conn = (navigator as Navigator & {
      connection?: { saveData?: boolean; effectiveType?: string }
    }).connection
    if (conn?.saveData) return
    if (conn?.effectiveType === "slow-2g" || conn?.effectiveType === "2g") return

    const idle = (window as Window & {
      requestIdleCallback?: (cb: () => void, opts?: { timeout: number }) => number
    }).requestIdleCallback ?? ((cb: () => void) => setTimeout(cb, 1500))

    const handle = idle(() => {
      PREFETCH_ROUTES.forEach((load) => { load().catch(() => {}) })
    }, { timeout: 3000 })

    return () => {
      const cancel = (window as Window & {
        cancelIdleCallback?: (handle: number) => void
      }).cancelIdleCallback
      if (cancel && typeof handle === "number") cancel(handle)
    }
  }, [])

  return (
    <ConnectionProvider>
      <AwayModeProvider>
        <KeepAwakeProvider>
        <div className="flex h-full">
          {/* Desktop sidebar */}
          <div className="hidden md:block">
            <Sidebar
              collapsed={sidebarCollapsed}
              onToggle={() =>
                setSidebarCollapsed((c) => {
                  const next = !c
                  try {
                    localStorage.setItem("sidebar-collapsed", next ? "1" : "0")
                  } catch {
                    /* ignore */
                  }
                  return next
                })
              }
            />
          </div>

          {/* Mobile nav drawer */}
          <MobileNav open={mobileNavOpen} onClose={() => setMobileNavOpen(false)} />

          {/* Main content */}
          <main
            className={cn(
              "flex-1 overflow-y-auto transition-all duration-300",
              "md:ml-56",
              sidebarCollapsed && "md:ml-16"
            )}
          >
            {/* Mobile header */}
            <div className="sticky top-0 z-[500] flex h-14 items-center gap-3 border-b border-white/5 bg-slate-950/80 px-4 backdrop-blur-md md:hidden">
              <button
                onClick={() => setMobileNavOpen(true)}
                className="rounded-lg p-2 text-slate-400 hover:bg-white/5 hover:text-slate-200"
              >
                <MenuIcon className="h-5 w-5" />
              </button>
              <span className="text-sm font-semibold text-slate-100" style={{ fontFamily: '"Inter", -apple-system, system-ui, sans-serif' }}>Sentry USB</span>
            </div>

            {/* Cap the content column so cards don't stretch into giant
                sparse bands on wide monitors / zoomed-out viewports —
                centered in the post-sidebar area. Pages that genuinely
                need full bleed can opt out with their own wrapper. */}
            <div className="mx-auto w-full max-w-[1280px] p-4 pb-safe md:p-6">
              <ConnectionBanner />
              <Outlet />
            </div>
          </main>
        </div>
        </KeepAwakeProvider>
      </AwayModeProvider>
    </ConnectionProvider>
  )
}
