import { useState, useEffect } from "react"
import { NavLink } from "react-router-dom"
import {
  LayoutDashboard,
  Video,
  FolderOpen,
  ScrollText,
  MapPin,
  MessageCircle,
  Settings,
  X,
  Shield,
  TerminalSquare,
  HeartPulse,
  Timer,
  LogOut,
  Users,
  Paintbrush,
  Volume2,
  BellRing,
  Wifi,
  Camera,
} from "lucide-react"
import { cn } from "@/lib/utils"
import { useAwayMode } from "@/hooks/useAwayMode"
import { useKeepAwake } from "@/hooks/useKeepAwake"
import { useUpdateAvailable } from "@/hooks/useUpdateAvailable"
import { useConnectionStatus } from "@/hooks/useConnectionStatus"
import { useAuth } from "@/hooks/useAuth"
import { useCommunityPrefs } from "@/hooks/useCommunityPrefs"

interface MobileNavProps {
  open: boolean
  onClose: () => void
}

const baseNavItems = [
  { to: "/", icon: LayoutDashboard, label: "Dashboard" },
  { to: "/viewer", icon: Video, label: "Viewer" },
  { to: "/files", icon: FolderOpen, label: "Files" },
  { to: "/snapshots", icon: Camera, label: "Snapshots" },
  { to: "/logs", icon: ScrollText, label: "Logs" },
  { to: "/drives", icon: MapPin, label: "Drives" },
  { to: "/community", icon: Users, label: "Community" },
  { to: "/notifications", icon: BellRing, label: "Notifications" },
  { to: "/settings", icon: Settings, label: "Settings" },
]

function buildNavItems(mode: ReturnType<typeof useCommunityPrefs>["mode"]) {
  return baseNavItems
    .filter((item) => item.to !== "/community" || mode !== "none")
    .map((item) => {
      if (item.to !== "/community") return item
      if (mode === "wraps-only") return { ...item, icon: Paintbrush, label: "Wraps" }
      if (mode === "chimes-only") return { ...item, icon: Volume2, label: "Lock Chimes" }
      return item
    })
}

export function MobileNav({ open, onClose }: MobileNavProps) {
  const { status: awayModeStatus } = useAwayMode()
  const { status } = useKeepAwake()
  const isAwake = status.state === "active" || status.state === "pending"
  const { available: updateAvailable } = useUpdateAvailable()
  const { state: connState } = useConnectionStatus()
  const { authRequired, logout } = useAuth()
  const { mode: communityMode } = useCommunityPrefs()
  const [version, setVersion] = useState<string | null>(null)
  const navItems = buildNavItems(communityMode)

  useEffect(() => {
    fetch("/api/system/version")
      .then(r => r.json())
      .then(data => setVersion(data.version || null))
      .catch(() => {})
  }, [])

  if (!open) return null

  return (
    <>
      {/* Backdrop */}
      <div
        className="fixed inset-0 z-[600] bg-black/60"
        onClick={onClose}
      />

      {/* Drawer */}
      <div className="glass-sidebar fixed left-0 top-0 z-[700] flex h-full w-64 flex-col">
        <div className="flex min-h-16 items-center justify-between px-4 py-3">
          <div className="flex items-center gap-3">
            <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-blue-500/20">
              <Shield className="h-5 w-5 text-blue-400" />
            </div>
            <div>
              <span className="text-lg font-semibold tracking-tight text-slate-100" style={{ fontFamily: '"Inter", -apple-system, system-ui, sans-serif' }}>
                Sentry USB
              </span>
              {version && (
                <p className="text-[10px] leading-tight text-slate-600">
                  {version}
                </p>
              )}
            </div>
          </div>
          <button
            onClick={onClose}
            className="rounded-lg p-1.5 text-slate-500 hover:bg-white/5 hover:text-slate-300"
          >
            <X className="h-5 w-5" />
          </button>
        </div>

        <nav className="flex-1 space-y-1 px-2 py-4">
          {navItems.map((item) => {
            const showBadge = updateAvailable && item.to === "/settings"
            return (
              <NavLink
                key={item.to}
                to={item.to}
                end={item.to === "/"}
                onClick={onClose}
                className={({ isActive }) =>
                  cn(
                    "flex items-center gap-3 rounded-lg px-3 py-2.5 text-sm font-medium transition-colors",
                    isActive
                      ? "bg-blue-500/15 text-blue-400"
                      : "text-slate-400 hover:bg-white/5 hover:text-slate-200"
                  )
                }
              >
                <span className="relative shrink-0">
                  <item.icon className="h-5 w-5" />
                  {showBadge && (
                    <span className="absolute -right-1 -top-1 h-2 w-2 rounded-full bg-amber-400" />
                  )}
                </span>
                <span className="flex flex-1 items-center justify-between">
                  {item.label}
                  {showBadge && (
                    <span className="rounded-full bg-amber-500/20 px-1.5 py-0.5 text-[10px] font-medium text-amber-400">
                      Update
                    </span>
                  )}
                </span>
              </NavLink>
            )
          })}
        </nav>

        {/* Terminal link (secondary) */}
        <NavLink
          to="/terminal"
          onClick={onClose}
          className={({ isActive }) =>
            cn(
              "mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs transition-colors",
              isActive
                ? "bg-blue-500/10 text-blue-400"
                : "text-slate-600 hover:bg-white/5 hover:text-slate-400"
            )
          }
        >
          <TerminalSquare className="h-3.5 w-3.5 shrink-0" />
          <span>Terminal</span>
        </NavLink>

        {/* Support link (secondary) */}
        <NavLink
          to="/support"
          onClick={onClose}
          className={({ isActive }) =>
            cn(
              "mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs transition-colors",
              isActive
                ? "bg-blue-500/10 text-blue-400"
                : "text-slate-600 hover:bg-white/5 hover:text-slate-400"
            )
          }
        >
          <MessageCircle className="h-3.5 w-3.5 shrink-0" />
          <span>Support</span>
        </NavLink>

        {/* Connection status */}
        <div className={cn(
          "mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs",
          connState === "connected" ? "text-emerald-400" : connState === "reconnecting" ? "text-amber-400" : "text-red-400"
        )}>
          <span className={cn(
            "h-2 w-2 shrink-0 rounded-full",
            connState === "connected" ? "bg-emerald-400" : connState === "reconnecting" ? "bg-amber-400 animate-pulse" : "bg-red-400"
          )} />
          <span className="opacity-70">
            {connState === "connected" ? "Connected" : connState === "reconnecting" ? "Reconnecting" : "Offline"}
          </span>
        </div>

        {/* Away Mode indicator */}
        {awayModeStatus.state === "active" && (
          <div className="mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs text-blue-400">
            <Wifi className="h-3.5 w-3.5 animate-pulse" />
            <span className="opacity-70">Away Mode</span>
          </div>
        )}

        {/* Keep-awake indicator */}
        {isAwake && (
          <div className={cn(
            "mx-2 mb-2 flex items-center gap-2 rounded-lg px-3 py-2 text-xs",
            status.state === "active"
              ? "text-rose-400"
              : "text-amber-400"
          )}>
            {status.state === "active" ? (
              <HeartPulse className="h-3.5 w-3.5 animate-pulse" />
            ) : (
              <Timer className="h-3.5 w-3.5 animate-pulse" />
            )}
            <span className="opacity-70">
              {status.state === "active" ? "Keeping awake" : "Waiting for archive..."}
            </span>
          </div>
        )}

        {/* Logout */}
        {authRequired && (
          <button
            onClick={() => { logout(); onClose() }}
            className="mx-2 mb-4 flex items-center gap-2 rounded-lg px-3 py-2 text-xs text-slate-600 transition-colors hover:bg-white/5 hover:text-slate-400"
          >
            <LogOut className="h-3.5 w-3.5" />
            <span>Logout</span>
          </button>
        )}
      </div>
    </>
  )
}
