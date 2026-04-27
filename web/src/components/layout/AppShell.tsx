import { useState } from "react"
import { Outlet } from "react-router-dom"
import { Menu } from "lucide-react"
import { Sidebar } from "./Sidebar"
import { MobileNav } from "./MobileNav"
import { ConnectionBanner } from "./ConnectionBanner"
import { cn } from "@/lib/utils"
import { KeepAwakeProvider } from "@/hooks/useKeepAwake"
import { AwayModeProvider } from "@/hooks/useAwayMode"
import { ConnectionProvider } from "@/hooks/useConnectionStatus"

export function AppShell() {
  const [sidebarCollapsed, setSidebarCollapsed] = useState(false)
  const [mobileNavOpen, setMobileNavOpen] = useState(false)

  return (
    <ConnectionProvider>
      <AwayModeProvider>
        <KeepAwakeProvider>
        <div className="flex h-full">
          {/* Desktop sidebar */}
          <div className="hidden md:block">
            <Sidebar
              collapsed={sidebarCollapsed}
              onToggle={() => setSidebarCollapsed(!sidebarCollapsed)}
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
                <Menu className="h-5 w-5" />
              </button>
              <span className="text-sm font-semibold text-slate-100" style={{ fontFamily: '"Inter", -apple-system, system-ui, sans-serif' }}>Sentry USB</span>
            </div>

            <div className="p-4 pb-safe md:p-6">
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
