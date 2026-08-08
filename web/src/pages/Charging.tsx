import { useCallback, useEffect, useMemo, useState } from "react"
import { useNavigate } from "react-router-dom"
import {
  BatteryCharging,
  CheckSquare,
  ChevronRight,
  Home,
  Loader2,
  MapPin,
  Trash2,
  Zap,
} from "lucide-react"
import {
  bulkDeleteCharges,
  fetchChargeSessions,
  fetchChargeTags,
  fetchCurrentCharge,
  setChargeTags,
} from "@/api/charging"
import type { ChargeSessionSummary, CurrentCharge } from "@/types/charging"
import { cn } from "@/lib/utils"
import { DatePopover } from "@/components/drives/DatePopover"
import { TagPopover } from "@/components/drives/TagPopover"
import {
  ChargingSummaryStrip,
  type ChargingStats,
} from "@/components/charging/ChargingSummaryStrip"
import { ChargingTagFilter } from "@/components/charging/ChargingTagFilter"
import { ChargingRatesButton } from "@/components/charging/ChargingRatesButton"
import { MiniPinMap } from "@/components/charging/MiniPinMap"
import { rangeBounds, type DateRange } from "@/hooks/useDrivesList"
import { useDistanceUnit } from "@/hooks/useDistanceUnit"
import { fmtDuration, fmtEnergy, fmtMoney, fmtSoc } from "@/lib/charge-format"

// While a charge is in progress, refresh on this cadence so the active
// session grows in place instead of only settling once the charge ends.
const POLL_MS = 30_000

export default function Charging() {
  const [sessions, setSessions] = useState<ChargeSessionSummary[]>([])
  const [tags, setTags] = useState<string[]>([])
  const [selectedTags, setSelectedTags] = useState<string[]>([])
  const [current, setCurrent] = useState<CurrentCharge | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const metric = useDistanceUnit()
  // Charge sessions are infrequent, so default to All time rather than
  // the Drives default of Last 7 days (which would usually be empty).
  const [range, setRange] = useState<DateRange>({ kind: "preset", preset: "all" })

  const [selectMode, setSelectMode] = useState(false)
  const [selected, setSelected] = useState<Set<number>>(new Set())
  const [confirmingBulkDelete, setConfirmingBulkDelete] = useState<{
    ids: number[]
  } | null>(null)
  const [deletingBulk, setDeletingBulk] = useState(false)
  const [bulkDeleteError, setBulkDeleteError] = useState<string | null>(null)

  // Refresh sessions + tags + live status. Used after a tag edit, a rate
  // change, or a delete so server-computed values stay in sync.
  const reload = useCallback(async () => {
    const [s, t, c] = await Promise.all([
      fetchChargeSessions(),
      fetchChargeTags(),
      fetchCurrentCharge().catch(() => null),
    ])
    setSessions(s)
    setTags(t)
    if (c) setCurrent(c)
  }, [])

  useEffect(() => {
    let cancelled = false
    setLoading(true)
    setError(null)
    Promise.all([
      fetchChargeSessions(),
      fetchChargeTags(),
      fetchCurrentCharge().catch(() => null),
    ])
      .then(([s, t, c]) => {
        if (cancelled) return
        setSessions(s)
        setTags(t)
        if (c) setCurrent(c)
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  // Poll live status; while actively charging, also refresh the session
  // list so the in-progress charge updates without a manual reload.
  useEffect(() => {
    const id = setInterval(async () => {
      if (document.hidden) return
      const c = await fetchCurrentCharge().catch(() => null)
      if (!c) return
      setCurrent(c)
      if (c.charging) {
        const [s, t] = await Promise.all([
          fetchChargeSessions(),
          fetchChargeTags(),
        ])
        setSessions(s)
        setTags(t)
      }
    }, POLL_MS)
    return () => clearInterval(id)
  }, [])

  const onTagsChange = useCallback(
    async (id: number, next: string[]) => {
      // Optimistic: show the new tags immediately, then resync (cost is
      // recomputed server-side from the tags).
      setSessions((prev) =>
        prev.map((s) => (s.id === id ? { ...s, tags: next } : s)),
      )
      try {
        await setChargeTags(id, next)
      } finally {
        await reload()
      }
    },
    [reload],
  )

  const toggleSelectMode = () => {
    setSelectMode((s) => {
      if (s) setSelected(new Set())
      return !s
    })
  }

  const onToggleSelected = useCallback((id: number) => {
    setSelected((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }, [])

  // The newest session is the in-progress one while the car reports
  // charging (sessions come back newest-first).
  const activeId = current?.charging ? (sessions[0]?.id ?? null) : null

  const visible = useMemo(() => {
    const { from, to } = rangeBounds(range, new Date())
    return sessions.filter((s) => {
      const t = new Date(s.startMs)
      if (from && t < from) return false
      if (to && t >= to) return false
      if (selectedTags.length > 0) {
        // "Home" and "Fast charging" are derived filters (not stored tags) —
        // match them via atHome / fastCharging; everything else is a real tag.
        const sel = selectedTags.map((t) => t.toLowerCase())
        const tagMatch =
          s.tags.some((tag) => selectedTags.includes(tag)) ||
          (s.atHome && sel.includes("home")) ||
          (s.fastCharging && sel.includes("fast charging"))
        if (!tagMatch) return false
      }
      return true
    })
  }, [sessions, range, selectedTags])

  // Tags-dropdown options = real tags (incl. the API-surfaced "Home") plus the
  // derived "Fast charging" filter when any session qualifies — so it lives in
  // the Tags dropdown alongside Home, not as a separate pill.
  const filterTags = useMemo(() => {
    if (
      sessions.some((s) => s.fastCharging) &&
      !tags.some((t) => t.toLowerCase() === "fast charging")
    ) {
      return [...tags, "Fast charging"]
    }
    return tags
  }, [tags, sessions])

  const onSelectAll = useCallback(() => {
    setSelected(new Set(visible.map((s) => s.id)))
  }, [visible])

  const onDeleteSelected = useCallback(() => {
    if (selected.size === 0) return
    setBulkDeleteError(null)
    setConfirmingBulkDelete({ ids: Array.from(selected) })
  }, [selected])

  const confirmBulkDelete = useCallback(async () => {
    if (!confirmingBulkDelete) return
    setDeletingBulk(true)
    setBulkDeleteError(null)
    try {
      await bulkDeleteCharges(confirmingBulkDelete.ids)
      setConfirmingBulkDelete(null)
      setSelected(new Set())
      setSelectMode(false)
      await reload()
    } catch (e) {
      setBulkDeleteError(e instanceof Error ? e.message : String(e))
    } finally {
      setDeletingBulk(false)
    }
  }, [confirmingBulkDelete, reload])

  const stats: ChargingStats = useMemo(() => {
    const costs = visible
      .map((s) => s.cost)
      .filter((c): c is number => c != null)
    const effs = visible
      .map((s) => s.efficiencyPct)
      .filter((e): e is number => e != null)
    return {
      count: visible.length,
      totalEnergyKwh: visible.reduce((sum, s) => sum + (s.energyAddedKwh ?? 0), 0),
      totalDurationSecs: visible.reduce((sum, s) => sum + s.durationSecs, 0),
      totalCost: costs.length ? costs.reduce((a, b) => a + b, 0) : null,
      currency: visible.find((s) => s.currency)?.currency ?? "$",
      avgEfficiency: effs.length
        ? effs.reduce((a, b) => a + b, 0) / effs.length
        : null,
    }
  }, [visible])

  return (
    <div className="mx-auto w-full max-w-5xl px-4 py-6 sm:px-6 sm:py-8">
      <div className="mb-4 flex flex-wrap items-center justify-between gap-3 sm:mb-6">
        <h1 className="text-2xl font-semibold text-slate-100 sm:text-3xl">
          Charging
        </h1>
      </div>

      <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
        <DatePopover range={range} onChange={setRange} />
        <ChargingTagFilter
          tags={filterTags}
          selected={selectedTags}
          onChange={setSelectedTags}
        />
        <ChargingRatesButton tags={tags} onSaved={reload} />
        {!selectMode && (
          <div className="order-last w-full sm:order-none sm:ml-3 sm:w-auto sm:min-w-0 sm:flex-1">
            <ChargingSummaryStrip stats={stats} loading={loading} />
          </div>
        )}
        <div className="ml-auto flex flex-wrap items-center gap-2">
          {selectMode ? (
            <ChargingSelectBar
              selectedCount={selected.size}
              totalCount={visible.length}
              onSelectAll={onSelectAll}
              onDelete={onDeleteSelected}
              onCancel={toggleSelectMode}
            />
          ) : (
            <button
              type="button"
              onClick={toggleSelectMode}
              className="inline-flex items-center gap-2 rounded-full border border-white/10 bg-white/[0.03] px-3.5 py-1.5 text-sm font-medium text-slate-200 transition-colors hover:bg-white/[0.06]"
            >
              <CheckSquare className="h-4 w-4" />
              Select
            </button>
          )}
        </div>
      </div>

      <div className="mt-4 flex flex-col gap-3">
        {loading && (
          <div className="flex items-center justify-center gap-2 rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-sm text-slate-400">
            <Loader2 className="h-4 w-4 animate-spin" />
            Loading charging history…
          </div>
        )}
        {error && !loading && (
          <div className="rounded-2xl border border-rose-400/30 bg-rose-500/5 p-6 text-sm text-rose-200">
            Failed to load charging history: {error}
          </div>
        )}
        {!loading && !error && visible.length === 0 && (
          <div className="rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-center text-sm text-slate-400">
            <BatteryCharging className="mx-auto mb-3 h-8 w-8 text-slate-600" />
            {sessions.length === 0
              ? "No charging sessions recorded yet. Sessions appear here once the car charges while the Pi is sampling."
              : "No charging sessions match these filters."}
          </div>
        )}
        {!loading &&
          visible.map((s) => (
            <ChargeRow
              key={s.id}
              session={s}
              metric={metric}
              active={s.id === activeId}
              livePowerKw={s.id === activeId ? (current?.powerKw ?? null) : null}
              selectMode={selectMode}
              selected={selected.has(s.id)}
              onToggleSelected={onToggleSelected}
              onTagsChange={onTagsChange}
            />
          ))}
      </div>

      {confirmingBulkDelete && (
        <div className="fixed inset-0 z-[2000] flex items-center justify-center bg-black/60 backdrop-blur-sm">
          <div className="w-full max-w-sm rounded-2xl border border-white/10 bg-slate-950 p-6 shadow-2xl">
            <h3 className="text-base font-semibold text-slate-100">
              {confirmingBulkDelete.ids.length === 1
                ? "Delete 1 charge?"
                : `Delete ${confirmingBulkDelete.ids.length} charges?`}
            </h3>
            <p className="mt-2 text-xs leading-relaxed text-slate-400">
              This removes the selected charge session
              {confirmingBulkDelete.ids.length === 1 ? "" : "s"} and their
              telemetry samples from the database. The action cannot be undone.
            </p>
            {bulkDeleteError && (
              <p className="mt-3 text-xs text-rose-300">{bulkDeleteError}</p>
            )}
            <div className="mt-5 flex items-center justify-end gap-2">
              <button
                type="button"
                disabled={deletingBulk}
                onClick={() => setConfirmingBulkDelete(null)}
                className="rounded-lg border border-white/10 bg-white/[0.03] px-4 py-1.5 text-xs font-medium text-slate-300 hover:bg-white/[0.06] disabled:opacity-50"
              >
                Cancel
              </button>
              <button
                type="button"
                disabled={deletingBulk}
                onClick={confirmBulkDelete}
                className="inline-flex items-center gap-1.5 rounded-lg bg-rose-600 px-4 py-1.5 text-xs font-medium text-white transition-colors hover:bg-rose-500 disabled:opacity-50"
              >
                {deletingBulk ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                ) : (
                  <Trash2 className="h-3.5 w-3.5" />
                )}
                {deletingBulk
                  ? "Deleting…"
                  : confirmingBulkDelete.ids.length === 1
                    ? "Delete charge"
                    : `Delete ${confirmingBulkDelete.ids.length} charges`}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  )
}

function ChargingSelectBar({
  selectedCount,
  totalCount,
  onSelectAll,
  onDelete,
  onCancel,
}: {
  selectedCount: number
  totalCount: number
  onSelectAll: () => void
  onDelete: () => void
  onCancel: () => void
}) {
  const hasSelection = selectedCount > 0
  return (
    <div className="flex items-center gap-2">
      <span className="mr-1 text-sm text-slate-400">
        {selectedCount} of {totalCount} selected
      </span>
      <button
        type="button"
        disabled={!hasSelection}
        onClick={onDelete}
        className="inline-flex items-center gap-1.5 rounded-full bg-rose-500/95 px-3 py-1 text-xs font-medium text-white transition-colors hover:bg-rose-400 disabled:opacity-50"
      >
        <Trash2 className="h-3.5 w-3.5" />
        Delete
      </button>
      <button
        type="button"
        onClick={onSelectAll}
        className="inline-flex items-center gap-1.5 rounded-full border border-white/10 bg-white/[0.03] px-3 py-1 text-xs font-medium text-slate-200 transition-colors hover:bg-white/[0.06]"
      >
        Select all
      </button>
      <button
        type="button"
        onClick={onCancel}
        className="inline-flex items-center gap-1.5 rounded-full border border-white/10 bg-white/[0.03] px-3 py-1 text-xs font-medium text-slate-200 transition-colors hover:bg-white/[0.06]"
      >
        Cancel
      </button>
    </div>
  )
}

function ChargeRow({
  session,
  metric,
  active,
  livePowerKw,
  selectMode,
  selected,
  onToggleSelected,
  onTagsChange,
}: {
  session: ChargeSessionSummary
  metric: boolean
  active: boolean
  livePowerKw: number | null
  selectMode: boolean
  selected: boolean
  onToggleSelected: (id: number) => void
  onTagsChange: (id: number, tags: string[]) => Promise<void> | void
}) {
  const navigate = useNavigate()
  const start = new Date(session.startMs)
  const onRowClick = () => {
    if (selectMode) {
      onToggleSelected(session.id)
      return
    }
    navigate(`/charging/${session.id}`)
  }

  // Two forms so the SoC range degrades instead of vanishing when the
  // row is tight: `socShort` is always shown ("62% → 79%"); the range
  // ("62% (132 mi) → …") only appears when there's room (sm+).
  const socShort =
    session.startSoc != null && session.endSoc != null
      ? `${fmtSoc(session.startSoc)} → ${fmtSoc(session.endSoc)}`
      : session.endSoc != null
        ? fmtSoc(session.endSoc)
        : "—"
  const socPart = (pct: number | null, mi: number | null): string => {
    if (pct == null) return "—"
    if (mi == null) return fmtSoc(pct)
    const dist = metric ? `${Math.round(mi * 1.609344)} km` : `${Math.round(mi)} mi`
    return `${fmtSoc(pct)} (${dist})`
  }
  const socFull =
    session.startSoc != null && session.endSoc != null
      ? `${socPart(session.startSoc, session.startRangeMi)} → ${socPart(session.endSoc, session.endRangeMi)}`
      : session.endSoc != null
        ? socPart(session.endSoc, session.endRangeMi)
        : "—"

  return (
    <div
      role="button"
      tabIndex={0}
      onClick={onRowClick}
      onKeyDown={(e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault()
          onRowClick()
        }
      }}
      className={cn(
        "group flex cursor-pointer items-center gap-3 rounded-2xl border p-3 transition-colors sm:gap-4 sm:p-4",
        selected
          ? "border-emerald-400/40 bg-emerald-400/[0.06]"
          : active
            ? "border-emerald-400/30 bg-emerald-500/10 hover:bg-emerald-500/15"
            : "border-white/[0.06] bg-white/[0.025] hover:border-white/10 hover:bg-white/[0.04]",
      )}
    >
      {selectMode && (
        <span
          aria-hidden
          className={cn(
            "flex h-5 w-5 shrink-0 items-center justify-center rounded border-2 transition-colors",
            selected ? "border-emerald-400 bg-emerald-400" : "border-white/30",
          )}
        >
          {selected && (
            <svg viewBox="0 0 12 12" className="h-3 w-3 text-slate-950">
              <path
                d="M2 6.5 L5 9.5 L10 3.5"
                stroke="currentColor"
                strokeWidth="2"
                fill="none"
              />
            </svg>
          )}
        </span>
      )}

      <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-emerald-500/10 text-emerald-300 ring-1 ring-inset ring-emerald-500/20">
        <BatteryCharging className={"h-5 w-5" + (active ? " animate-pulse" : "")} />
      </span>

      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-1.5 text-sm font-medium text-slate-100">
          {active && (
            <span className="inline-flex shrink-0 items-center gap-1 rounded-full bg-emerald-500/15 px-1.5 py-0.5 text-[10px] font-medium text-emerald-300 ring-1 ring-inset ring-emerald-400/20">
              <span className="h-1.5 w-1.5 rounded-full bg-emerald-400 animate-pulse" />
              Charging
            </span>
          )}
          {session.fastCharging && (
            <span
              title="DC fast charging (Supercharger / CCS) — peak power over 22 kW"
              className="inline-flex shrink-0 items-center gap-1 rounded-full bg-amber-500/15 px-1.5 py-0.5 text-[10px] font-medium text-amber-300 ring-1 ring-inset ring-amber-400/20"
            >
              <Zap className="h-2.5 w-2.5 fill-amber-300" />
              Fast
            </span>
          )}
          {session.location ? (
            <>
              <MapPin className="h-3.5 w-3.5 shrink-0 text-emerald-300/80" />
              <span className="truncate">{session.location}</span>
            </>
          ) : (
            <span className="truncate">{formatDate(start)}</span>
          )}
        </div>
        <div className="mt-0.5 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs text-slate-500">
          {session.location && <span>{formatDate(start)}</span>}
          {session.location && <span>·</span>}
          <span>{formatTime(start)}</span>
          <span>·</span>
          <span>{fmtDuration(session.durationSecs)}</span>
          {active && livePowerKw != null && (
            <>
              <span>·</span>
              <span className="text-emerald-300">{livePowerKw} kW</span>
            </>
          )}
        </div>
        {/* Mobile: energy + SoC + cost sit below the meta line so the
            title gets the full row width instead of being squeezed. */}
        <div className="mt-1.5 flex items-center gap-2.5 tabular-nums sm:hidden">
          <span className="inline-flex items-center gap-1 text-sm font-semibold text-emerald-300">
            <Zap className="h-3.5 w-3.5" />
            {fmtEnergy(session.energyAddedKwh)}
          </span>
          <span className="text-xs text-slate-500">{socShort}</span>
          {session.cost != null && (
            <span className="text-xs font-medium text-slate-300">
              {fmtMoney(session.cost, session.currency)}
            </span>
          )}
        </div>
      </div>

      {/* Desktop: energy + SoC + cost as a right-aligned column. */}
      <div className="hidden shrink-0 text-right sm:block">
        <div className="flex items-center justify-end gap-1 text-sm font-semibold text-emerald-300 tabular-nums">
          <Zap className="h-3.5 w-3.5" />
          {fmtEnergy(session.energyAddedKwh)}
        </div>
        <div className="mt-0.5 text-xs text-slate-500 tabular-nums">{socFull}</div>
        {session.cost != null && (
          <div className="mt-0.5 text-xs font-medium text-slate-300 tabular-nums">
            {fmtMoney(session.cost, session.currency)}
          </div>
        )}
      </div>

      <div
        onClick={(e) => e.stopPropagation()}
        className="flex items-center gap-1.5"
      >
        {session.atHome && (
          <span
            title="Charged at home (automatic)"
            className="inline-flex items-center gap-1 rounded-md border border-emerald-400/30 bg-emerald-400/10 px-2 py-1 text-xs font-medium text-emerald-200"
          >
            <Home className="h-3 w-3" />
            Home
          </span>
        )}
        <TagPopover
          tags={session.tags}
          onChange={(t) => onTagsChange(session.id, t)}
        />
      </div>

      {session.locationLat != null && session.locationLon != null && (
        <MiniPinMap
          lat={session.locationLat}
          lon={session.locationLon}
          className="h-14 w-20 sm:h-20 sm:w-32"
        />
      )}

      <ChevronRight className="h-4 w-4 shrink-0 text-slate-600 transition-colors group-hover:text-slate-400" />
    </div>
  )
}

function formatDate(d: Date): string {
  return d.toLocaleDateString([], {
    weekday: "short",
    month: "short",
    day: "numeric",
  })
}

function formatTime(d: Date): string {
  return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
}
