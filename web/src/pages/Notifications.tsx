import { useState, useEffect, useCallback } from "react"
import {
  Bell,
  BellOff,
  CheckCircle2,
  AlertTriangle,
  XCircle,
  Trash2,
  X,
  Settings,
  Clock,
  Archive,
  Thermometer,
  Zap,
  HardDrive,
  Download,
  Battery,
  Music,
  Filter,
  Loader2,
  Info,
} from "lucide-react"
import { cn } from "@/lib/utils"

// ─── Types ────────────────────────────────────────────────────────────────────

interface NotificationEvent {
  id: string
  ts: number
  type: string
  title: string
  message: string
  providers: string[]
  results: Record<string, string>
}

interface NotificationSettings {
  archive_start: boolean
  archive_complete: boolean
  archive_error: boolean
  temperature: boolean
  keep_awake_failure: boolean
  update: boolean
  drives: boolean
  rtc_battery: boolean
  music_sync: boolean
}

interface HistoryResponse {
  events: NotificationEvent[]
  total: number
  limit: number
  offset: number
}

type Tab = "history" | "settings"

// ─── Helpers ──────────────────────────────────────────────────────────────────

const NOTIFICATION_TYPES = [
  { key: "archive_start", label: "Archive Started", description: "When file archiving begins", icon: Archive },
  { key: "archive_complete", label: "Archive Complete", description: "When file archiving finishes successfully", icon: CheckCircle2 },
  { key: "archive_error", label: "Archive Errors", description: "When archiving encounters errors", icon: XCircle },
  { key: "temperature", label: "Temperature Alerts", description: "When CPU temperature exceeds safe thresholds", icon: Thermometer },
  { key: "keep_awake_failure", label: "Keep-Awake Failures", description: "When Sentry Mode keep-awake fails after retries", icon: Zap },
  { key: "update", label: "Update Available", description: "When a new software update is detected", icon: Download },
  { key: "drives", label: "New Drives Detected", description: "When new TeslaCam drives are mapped", icon: HardDrive },
  { key: "rtc_battery", label: "RTC Battery Warning", description: "When the real-time clock battery is low or missing", icon: Battery },
  { key: "music_sync", label: "Music Sync", description: "When music files finish syncing to USB", icon: Music },
] as const

function typeIcon(type: string) {
  const found = NOTIFICATION_TYPES.find(t => t.key === type)
  return found?.icon || Bell
}

function typeLabel(type: string): string {
  const found = NOTIFICATION_TYPES.find(t => t.key === type)
  return found?.label || type.replace(/_/g, " ").replace(/\b\w/g, c => c.toUpperCase())
}

function typeColor(type: string): string {
  switch (type) {
    case "archive_start": return "text-blue-400"
    case "archive_complete": return "text-emerald-400"
    case "archive_error": return "text-red-400"
    case "temperature": return "text-orange-400"
    case "keep_awake_failure": return "text-amber-400"
    case "update": return "text-cyan-400"
    case "drives": return "text-violet-400"
    case "rtc_battery": return "text-yellow-400"
    case "music_sync": return "text-pink-400"
    default: return "text-slate-400"
  }
}

function typeBgColor(type: string): string {
  switch (type) {
    case "archive_start": return "bg-blue-500/15"
    case "archive_complete": return "bg-emerald-500/15"
    case "archive_error": return "bg-red-500/15"
    case "temperature": return "bg-orange-500/15"
    case "keep_awake_failure": return "bg-amber-500/15"
    case "update": return "bg-cyan-500/15"
    case "drives": return "bg-violet-500/15"
    case "rtc_battery": return "bg-yellow-500/15"
    case "music_sync": return "bg-pink-500/15"
    default: return "bg-white/5"
  }
}

function providerStatusIcon(results: Record<string, string>) {
  const values = Object.values(results)
  if (values.length === 0) return null
  const allOk = values.every(v => v === "ok")
  const allError = values.every(v => v !== "ok")
  if (allOk) return <CheckCircle2 className="h-3.5 w-3.5 text-emerald-400" />
  if (allError) return <XCircle className="h-3.5 w-3.5 text-red-400" />
  return <AlertTriangle className="h-3.5 w-3.5 text-amber-400" />
}

function relativeTime(ts: number): string {
  const now = Math.floor(Date.now() / 1000)
  const diff = now - ts
  if (diff < 60) return "just now"
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`
  if (diff < 604800) return `${Math.floor(diff / 86400)}d ago`
  return new Date(ts * 1000).toLocaleDateString()
}

function absoluteTime(ts: number): string {
  return new Date(ts * 1000).toLocaleString()
}

// ─── Main component ───────────────────────────────────────────────────────────

export default function Notifications() {
  const [activeTab, setActiveTab] = useState<Tab>("history")
  const [events, setEvents] = useState<NotificationEvent[]>([])
  const [total, setTotal] = useState(0)
  const [loading, setLoading] = useState(true)
  const [settings, setSettings] = useState<NotificationSettings | null>(null)
  const [savingSettings, setSavingSettings] = useState(false)
  const [typeFilter, setTypeFilter] = useState<string>("")
  const [confirmClear, setConfirmClear] = useState(false)
  const [offset, setOffset] = useState(0)
  const PAGE_SIZE = 50

  // Load notification history
  const loadHistory = useCallback(async (currentOffset = 0, filter = typeFilter) => {
    setLoading(true)
    try {
      const params = new URLSearchParams({ limit: String(PAGE_SIZE), offset: String(currentOffset) })
      if (filter) params.set("type", filter)
      const res = await fetch(`/api/notifications/history?${params}`)
      if (!res.ok) throw new Error("Failed to load history")
      const data: HistoryResponse = await res.json()
      setEvents(data.events || [])
      setTotal(data.total)
    } catch {
      setEvents([])
      setTotal(0)
    } finally {
      setLoading(false)
    }
  }, [typeFilter])

  // Load notification settings
  const loadSettings = useCallback(async () => {
    try {
      const res = await fetch("/api/notifications/settings")
      if (!res.ok) throw new Error("Failed to load settings")
      const data: NotificationSettings = await res.json()
      setSettings(data)
    } catch {
      setSettings({
        archive_start: true,
        archive_complete: true,
        archive_error: true,
        temperature: true,
        keep_awake_failure: true,
        update: true,
        drives: true,
        rtc_battery: true,
        music_sync: true,
      })
    }
  }, [])

  useEffect(() => {
    loadHistory(0)
    loadSettings()
  }, [loadHistory, loadSettings])

  // Save settings
  async function handleToggle(key: keyof NotificationSettings) {
    if (!settings) return
    const updated = { ...settings, [key]: !settings[key] }
    setSettings(updated)
    setSavingSettings(true)
    try {
      const res = await fetch("/api/notifications/settings", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(updated),
      })
      if (!res.ok) {
        // Rollback
        setSettings(settings)
      }
    } catch {
      setSettings(settings)
    } finally {
      setSavingSettings(false)
    }
  }

  // Clear all history
  async function handleClearAll() {
    if (!confirmClear) {
      setConfirmClear(true)
      setTimeout(() => setConfirmClear(false), 5000)
      return
    }
    try {
      await fetch("/api/notifications/history", { method: "DELETE" })
      setEvents([])
      setTotal(0)
      setOffset(0)
    } catch { /* ignore */ }
    setConfirmClear(false)
  }

  // Delete single notification
  async function handleDeleteOne(id: string) {
    setEvents(prev => prev.filter(e => e.id !== id))
    setTotal(prev => prev - 1)
    try {
      await fetch(`/api/notifications/history/${id}`, { method: "DELETE" })
    } catch {
      // Reload on failure
      loadHistory(offset)
    }
  }

  // Filter change
  function handleFilterChange(filter: string) {
    setTypeFilter(filter)
    setOffset(0)
    loadHistory(0, filter)
  }

  // Pagination
  function handlePage(direction: "next" | "prev") {
    const newOffset = direction === "next" ? offset + PAGE_SIZE : Math.max(0, offset - PAGE_SIZE)
    setOffset(newOffset)
    loadHistory(newOffset)
  }

  const TABS = [
    { id: "history" as const, label: "History", icon: Clock },
    { id: "settings" as const, label: "Settings", icon: Settings },
  ]

  return (
    <div className="space-y-6">
      {/* Header */}
      <div>
        <h1 className="text-2xl font-bold text-slate-100">Notifications</h1>
        <p className="mt-1 text-sm text-slate-500">
          View notification history and configure which events trigger alerts
        </p>
      </div>

      {/* Tab bar */}
      <div className="tab-bar">
        {TABS.map((tab) => (
          <button
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            className={cn("tab-item flex items-center justify-center gap-2", activeTab === tab.id && "active")}
          >
            <tab.icon className="h-3.5 w-3.5 hidden sm:block" />
            {tab.label}
          </button>
        ))}
      </div>

      {/* ── History Tab ──────────────────────────────────────────────── */}
      {activeTab === "history" && (
        <div className="space-y-4">
          {/* Toolbar */}
          <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
            {/* Filter */}
            <div className="flex items-center gap-2">
              <Filter className="h-4 w-4 text-slate-500" />
              <select
                value={typeFilter}
                onChange={e => handleFilterChange(e.target.value)}
                className="rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-sm text-slate-300 outline-none transition-colors focus:border-blue-500/50"
              >
                <option value="">All types</option>
                {NOTIFICATION_TYPES.map(t => (
                  <option key={t.key} value={t.key}>{t.label}</option>
                ))}
              </select>
              {typeFilter && (
                <button
                  onClick={() => handleFilterChange("")}
                  className="rounded-md p-1 text-slate-500 transition-colors hover:bg-white/5 hover:text-slate-300"
                >
                  <X className="h-3.5 w-3.5" />
                </button>
              )}
            </div>

            {/* Clear all */}
            <button
              onClick={handleClearAll}
              disabled={events.length === 0}
              className={cn(
                "flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-sm font-medium transition-colors",
                confirmClear
                  ? "bg-red-500/20 text-red-400 hover:bg-red-500/30"
                  : "border border-white/10 bg-white/5 text-slate-400 hover:bg-white/10 hover:text-slate-300",
                events.length === 0 && "cursor-not-allowed opacity-50"
              )}
            >
              <Trash2 className="h-3.5 w-3.5" />
              {confirmClear ? "Click again to confirm" : "Clear All"}
            </button>
          </div>

          {/* Events list */}
          {loading ? (
            <div className="flex items-center justify-center py-16">
              <Loader2 className="h-6 w-6 animate-spin text-blue-400" />
            </div>
          ) : events.length === 0 ? (
            <div className="glass-card flex flex-col items-center justify-center py-16 text-center">
              <div className="flex h-14 w-14 items-center justify-center rounded-2xl bg-white/5">
                <BellOff className="h-7 w-7 text-slate-600" />
              </div>
              <p className="mt-4 text-sm font-medium text-slate-400">No notifications yet</p>
              <p className="mt-1 text-xs text-slate-600">
                {typeFilter ? "No notifications match this filter" : "Notification events will appear here as they occur"}
              </p>
            </div>
          ) : (
            <div className="space-y-2">
              {events.map((event) => {
                const Icon = typeIcon(event.type)
                const color = typeColor(event.type)
                const bg = typeBgColor(event.type)
                return (
                  <div
                    key={event.id}
                    className="glass-card group relative overflow-hidden p-4 transition-colors hover:bg-white/[0.04]"
                  >
                    {/* Dismiss button */}
                    <button
                      onClick={() => handleDeleteOne(event.id)}
                      className="absolute right-3 top-3 rounded-md p-1 text-slate-600 opacity-0 transition-all hover:bg-white/10 hover:text-slate-400 group-hover:opacity-100"
                      title="Dismiss"
                    >
                      <X className="h-3.5 w-3.5" />
                    </button>

                    <div className="flex gap-3">
                      {/* Type icon */}
                      <div className={cn("flex h-9 w-9 shrink-0 items-center justify-center rounded-xl", bg)}>
                        <Icon className={cn("h-4.5 w-4.5", color)} />
                      </div>

                      {/* Content */}
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-2">
                          <span className={cn("text-xs font-semibold uppercase tracking-wider", color)}>
                            {typeLabel(event.type)}
                          </span>
                          {providerStatusIcon(event.results)}
                        </div>
                        <p className="mt-0.5 text-sm text-slate-300 leading-relaxed">{event.message}</p>

                        {/* Footer: time + providers */}
                        <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-1">
                          <span className="text-xs text-slate-600" title={absoluteTime(event.ts)}>
                            {relativeTime(event.ts)}
                          </span>
                          {event.providers.length > 0 && (
                            <div className="flex flex-wrap gap-1">
                              {event.providers.map(p => {
                                const status = event.results[p]
                                return (
                                  <span
                                    key={p}
                                    className={cn(
                                      "rounded-md px-1.5 py-0.5 text-[10px] font-medium",
                                      status === "ok"
                                        ? "bg-emerald-500/10 text-emerald-400"
                                        : "bg-red-500/10 text-red-400"
                                    )}
                                  >
                                    {p.replace(/_/g, " ")}
                                  </span>
                                )
                              })}
                            </div>
                          )}
                        </div>
                      </div>
                    </div>
                  </div>
                )
              })}
            </div>
          )}

          {/* Pagination */}
          {total > PAGE_SIZE && (
            <div className="flex items-center justify-between pt-2">
              <button
                onClick={() => handlePage("prev")}
                disabled={offset === 0}
                className="rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-sm text-slate-400 transition-colors hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-50"
              >
                Previous
              </button>
              <span className="text-xs text-slate-600">
                {offset + 1}–{Math.min(offset + PAGE_SIZE, total)} of {total}
              </span>
              <button
                onClick={() => handlePage("next")}
                disabled={offset + PAGE_SIZE >= total}
                className="rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-sm text-slate-400 transition-colors hover:bg-white/10 disabled:cursor-not-allowed disabled:opacity-50"
              >
                Next
              </button>
            </div>
          )}
        </div>
      )}

      {/* ── Settings Tab ─────────────────────────────────────────────── */}
      {activeTab === "settings" && settings && (
        <div className="space-y-4">
          <div className="glass-card overflow-hidden p-5">
            <div className="flex items-start gap-3">
              <div className="flex h-10 w-10 shrink-0 items-center justify-center rounded-xl bg-blue-500/15">
                <Info className="h-5 w-5 text-blue-400" />
              </div>
              <div>
                <p className="text-sm text-slate-300">
                  Toggle which events trigger notifications. Disabling a type will prevent it
                  from being sent to <em>all</em> providers (Pushover, Discord, mobile, etc.).
                </p>
                <p className="mt-1 text-xs text-slate-600">
                  To configure notification providers, use the Setup Wizard in Settings.
                </p>
              </div>
            </div>
          </div>

          <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
            {NOTIFICATION_TYPES.map((nt) => {
              const Icon = nt.icon
              const enabled = settings[nt.key as keyof NotificationSettings]
              const color = typeColor(nt.key)
              const bg = typeBgColor(nt.key)
              return (
                <div
                  key={nt.key}
                  className={cn(
                    "glass-card flex items-center gap-4 p-4 transition-colors",
                    enabled ? "border-white/10" : "border-white/5 opacity-60"
                  )}
                >
                  <div className={cn("flex h-10 w-10 shrink-0 items-center justify-center rounded-xl", enabled ? bg : "bg-white/5")}>
                    <Icon className={cn("h-5 w-5", enabled ? color : "text-slate-600")} />
                  </div>
                  <div className="min-w-0 flex-1">
                    <p className="text-sm font-medium text-slate-200">{nt.label}</p>
                    <p className="text-xs text-slate-500 leading-relaxed">{nt.description}</p>
                  </div>
                  <button
                    onClick={() => handleToggle(nt.key as keyof NotificationSettings)}
                    disabled={savingSettings}
                    className={cn(
                      "relative h-6 w-11 shrink-0 rounded-full transition-colors",
                      enabled ? "bg-blue-500" : "bg-white/10"
                    )}
                  >
                    <span
                      className={cn(
                        "absolute top-0.5 left-0.5 h-5 w-5 rounded-full bg-white transition-transform shadow-sm",
                        enabled && "translate-x-5"
                      )}
                    />
                  </button>
                </div>
              )
            })}
          </div>
        </div>
      )}
    </div>
  )
}
