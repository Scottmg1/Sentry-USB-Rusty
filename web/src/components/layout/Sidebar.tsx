import { useState, useEffect } from "react"
import { NavLink } from "react-router-dom"
import {
  BrushIcon,
  CardiologyIcon,
  ChargerIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  DashboardIcon,
  FolderOpenIcon,
  GroupIcon,
  LocationOnIcon,
  LogoutIcon,
  NotificationsActiveIcon,
  PhotoCameraIcon,
  ReceiptLongIcon,
  SettingsIcon,
  ShieldIcon,
  SmartToyIcon,
  Terminal2Icon,
  TimerIcon,
  VideocamIcon,
  VolumeUpIcon,
  WifiIcon,
} from "@/components/icons"
import { cn } from "@/lib/utils"
import { useAwayMode } from "@/hooks/useAwayMode"
import { useKeepAwake } from "@/hooks/useKeepAwake"
import { useUpdateAvailable } from "@/hooks/useUpdateAvailable"
import { useConnectionStatus } from "@/hooks/useConnectionStatus"
import { useAuth } from "@/hooks/useAuth"
import { useCommunityPrefs } from "@/hooks/useCommunityPrefs"

interface SidebarProps {
  collapsed: boolean
  onToggle: () => void
}

const baseNavItems = [
  { to: "/", icon: DashboardIcon, label: "Dashboard" },
  { to: "/viewer", icon: VideocamIcon, label: "Viewer" },
  { to: "/files", icon: FolderOpenIcon, label: "Files" },
  { to: "/snapshots", icon: PhotoCameraIcon, label: "Snapshots" },
  { to: "/logs", icon: ReceiptLongIcon, label: "Logs" },
  { to: "/drives", icon: LocationOnIcon, label: "Drives" },
  { to: "/community", icon: GroupIcon, label: "Community" },
  { to: "/notifications", icon: NotificationsActiveIcon, label: "Notifications" },
  { to: "/settings", icon: SettingsIcon, label: "Settings" },
]

function buildNavItems(
  mode: ReturnType<typeof useCommunityPrefs>["mode"],
) {
  // Charging history is a standard view now — slot it right after Drives
  // so the two telemetry views sit together.
  const items = baseNavItems.flatMap((item) =>
    item.to === "/drives"
      ? [item, { to: "/charging", icon: ChargerIcon, label: "Charging" }]
      : [item],
  )
  return items
    .filter((item) => item.to !== "/community" || mode !== "none")
    .map((item) => {
      if (item.to !== "/community") return item
      if (mode === "wraps-only") return { ...item, icon: BrushIcon, label: "Wraps" }
      if (mode === "chimes-only") return { ...item, icon: VolumeUpIcon, label: "Lock Chimes" }
      return item
    })
}

export function Sidebar({ collapsed, onToggle }: SidebarProps) {
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

  return (
    <aside
      className={cn(
        "glass-sidebar fixed left-0 top-0 z-30 flex h-full flex-col transition-all duration-300",
        collapsed ? "w-16" : "w-56"
      )}
    >
      {/* Logo */}
      <div className="flex min-h-16 items-center gap-3 px-4 py-3">
        <div className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-blue-500/20">
          <ShieldIcon className="h-5 w-5 text-blue-400" />
        </div>
        {!collapsed && (
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
        )}
      </div>

      {/* Navigation */}
      <nav className="flex-1 space-y-1 px-2 py-4">
        {navItems.map((item) => {
          const showBadge = updateAvailable && item.to === "/settings"
          return (
            <NavLink
              key={item.to}
              to={item.to}
              end={item.to === "/"}
              // Native tooltip so the icon-only collapsed rail is still
              // usable — without it, collapsed nav items are unlabeled.
              title={collapsed ? item.label : undefined}
              aria-label={collapsed ? item.label : undefined}
              className={({ isActive }) =>
                cn(
                  "flex items-center gap-3 rounded-lg px-3 py-2.5 text-sm font-medium transition-colors",
                  collapsed && "justify-center",
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
              {!collapsed && (
                <span className="flex flex-1 items-center justify-between">
                  {item.label}
                  {showBadge && (
                    <span className="rounded-full bg-amber-500/20 px-1.5 py-0.5 text-[10px] font-medium text-amber-400">
                      Update
                    </span>
                  )}
                </span>
              )}
            </NavLink>
          )
        })}
      </nav>

      {/* Terminal link (secondary) */}
      <NavLink
        to="/terminal"
        title={collapsed ? "Terminal" : undefined}
        aria-label={collapsed ? "Terminal" : undefined}
        className={({ isActive }) =>
          cn(
            "mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs transition-colors",
            collapsed && "justify-center",
            isActive
              ? "bg-blue-500/10 text-blue-400"
              : "text-slate-600 hover:bg-white/5 hover:text-slate-400"
          )
        }
      >
        <Terminal2Icon className="h-3.5 w-3.5 shrink-0" />
        {!collapsed && <span>Terminal</span>}
      </NavLink>

      {/* AI Support link (secondary) */}
      <NavLink
        to="/support"
        title={collapsed ? "AI Support & Help" : undefined}
        aria-label={collapsed ? "AI Support & Help" : undefined}
        className={({ isActive }) =>
          cn(
            "mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs transition-colors",
            collapsed && "justify-center",
            isActive
              ? "bg-blue-500/10 text-blue-400"
              : "text-slate-600 hover:bg-white/5 hover:text-slate-400"
          )
        }
      >
        <SmartToyIcon className="h-3.5 w-3.5 shrink-0" />
        {!collapsed && <span>AI Support &amp; Help</span>}
      </NavLink>

      {/* Connection status */}
      <div
        title={collapsed ? (connState === "connected" ? "Connected" : connState === "reconnecting" ? "Reconnecting" : "Offline") : undefined}
        className={cn(
        "mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs",
        collapsed && "justify-center",
        connState === "connected" ? "text-emerald-400" : connState === "reconnecting" ? "text-amber-400" : "text-red-400"
      )}>
        <span className={cn(
          "h-2 w-2 shrink-0 rounded-full",
          connState === "connected" ? "bg-emerald-400" : connState === "reconnecting" ? "bg-amber-400 animate-pulse" : "bg-red-400"
        )} />
        {!collapsed && (
          <span className="opacity-70">
            {connState === "connected" ? "Connected" : connState === "reconnecting" ? "Reconnecting" : "Offline"}
          </span>
        )}
      </div>

      {/* Away Mode indicator */}
      {awayModeStatus.state === "active" && (
        <div title={collapsed ? "Away Mode" : undefined} className={cn("mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs text-blue-400", collapsed && "justify-center")}>
          <WifiIcon className="h-3.5 w-3.5 animate-pulse" />
          {!collapsed && (
            <span className="opacity-70">Away Mode</span>
          )}
        </div>
      )}

      {/* Keep-awake indicator */}
      {isAwake && (
        <div
          title={collapsed ? (status.state === "active" ? "Keeping awake" : "Waiting for archive...") : undefined}
          className={cn(
          "mx-2 mb-2 flex items-center gap-2 rounded-lg px-3 py-2 text-xs",
          collapsed && "justify-center",
          status.state === "active"
            ? "text-rose-400"
            : "text-amber-400"
        )}>
          {status.state === "active" ? (
            <CardiologyIcon className="h-3.5 w-3.5 animate-pulse" />
          ) : (
            <TimerIcon className="h-3.5 w-3.5 animate-pulse" />
          )}
          {!collapsed && (
            <span className="opacity-70">
              {status.state === "active" ? "Keeping awake" : "Waiting for archive..."}
            </span>
          )}
        </div>
      )}

      {/* Logout */}
      {authRequired && (
        <button
          onClick={logout}
          title={collapsed ? "Logout" : undefined}
          aria-label={collapsed ? "Logout" : undefined}
          className={cn(
            "mx-2 mb-1 flex items-center gap-2 rounded-lg px-3 py-2 text-xs text-slate-600 transition-colors hover:bg-white/5 hover:text-slate-400",
            collapsed && "justify-center"
          )}
        >
          <LogoutIcon className="h-3.5 w-3.5 shrink-0" />
          {!collapsed && <span>Logout</span>}
        </button>
      )}

      {/* Collapse toggle — pinned to right edge, vertically centered */}
      <button
        onClick={onToggle}
        className="absolute right-0 top-1/2 z-40 -translate-y-1/2 translate-x-1/2 flex h-6 w-6 items-center justify-center rounded-full border border-white/10 bg-slate-900 text-slate-500 shadow-lg transition-colors hover:bg-slate-800 hover:text-slate-300"
      >
        {collapsed ? <ChevronRightIcon className="h-3 w-3" /> : <ChevronLeftIcon className="h-3 w-3" />}
      </button>
    </aside>
  )
}
