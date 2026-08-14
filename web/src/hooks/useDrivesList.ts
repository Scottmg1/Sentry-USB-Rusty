import { useEffect, useMemo, useState } from "react"
import { useSearchParams } from "react-router-dom"
import type { DriveSummary, RouteOverview } from "@/types/drives"
import { fetchDrives, fetchRouteOverviews } from "@/api/drives"
import {
  computeFilteredStats,
  type DrivesFilteredStats,
} from "@/lib/drive-stats"

export type { DrivesFilteredStats }

const PAGE_SIZE = 10

// Preserve list state across detail navigation. Fresh cache hits skip the
// request; stale hits render immediately while refreshing in the background.
interface DrivesCache {
  drives: DriveSummary[]
  routes: RouteOverview[]
  at: number
}
let listCache: DrivesCache | null = null
const CACHE_STALE_MS = 30_000

export type DateRange =
  | { kind: "preset"; preset: DatePreset }
  | { kind: "custom"; start: string; end: string }

export type DatePreset =
  | "today"
  | "yesterday"
  | "last7"
  | "last30"
  | "thisYear"
  | "lastYear"
  | "all"

export interface DrivesFilters {
  tag?: string
  // Persist minimum distance in miles; metric conversion is display-only.
  minDistanceMi?: number
}

export interface DrivesListState {
  drives: DriveSummary[]
  visible: DriveSummary[]
  routesByStartTime: Map<string, [number, number][]>
  total: number
  page: number
  pageCount: number
  pageStart: number
  pageEnd: number
  range: DateRange
  filters: DrivesFilters
  sortDir: "asc" | "desc"
  filteredStats: DrivesFilteredStats
  loading: boolean
  error: string | null
  setPage: (n: number) => void
  setRange: (r: DateRange) => void
  setFilters: (f: DrivesFilters) => void
  setSortDir: (d: "asc" | "desc") => void
  refresh: () => Promise<void>
  // Apply tag edits optimistically until the next authoritative refresh.
  patchDriveTags: (id: number, tags: string[]) => void
}

function readRange(params: URLSearchParams): DateRange {
  const preset = params.get("range") as DatePreset | null
  const start = params.get("start")
  const end = params.get("end")
  if (start && end) return { kind: "custom", start, end }
  return { kind: "preset", preset: preset ?? "last7" }
}

function readFilters(params: URLSearchParams): DrivesFilters {
  const minStr = params.get("minDist")
  const minDistanceMi = minStr ? Number(minStr) : undefined
  return {
    tag: params.get("tag") || undefined,
    minDistanceMi: Number.isFinite(minDistanceMi) ? minDistanceMi : undefined,
  }
}

export function rangeBounds(range: DateRange, now: Date): { from?: Date; to?: Date } {
  if (range.kind === "custom") {
    return { from: new Date(range.start), to: new Date(range.end) }
  }
  const startOfToday = new Date(now)
  startOfToday.setHours(0, 0, 0, 0)
  switch (range.preset) {
    case "today":
      return { from: startOfToday }
    case "yesterday": {
      const y = new Date(startOfToday)
      y.setDate(y.getDate() - 1)
      return { from: y, to: startOfToday }
    }
    case "last7": {
      const f = new Date(startOfToday)
      f.setDate(f.getDate() - 7)
      return { from: f }
    }
    case "last30": {
      const f = new Date(startOfToday)
      f.setDate(f.getDate() - 30)
      return { from: f }
    }
    case "thisYear": {
      const f = new Date(now.getFullYear(), 0, 1)
      return { from: f }
    }
    case "lastYear": {
      const f = new Date(now.getFullYear() - 1, 0, 1)
      const t = new Date(now.getFullYear(), 0, 1)
      return { from: f, to: t }
    }
    case "all":
    default:
      return {}
  }
}

function filterDrives(
  drives: DriveSummary[],
  range: DateRange,
  filters: DrivesFilters,
  now: Date,
): DriveSummary[] {
  const { from, to } = rangeBounds(range, now)
  return drives.filter((d) => {
    const t = new Date(d.startTime)
    if (from && t < from) return false
    if (to && t >= to) return false
    if (filters.tag && !(d.tags ?? []).includes(filters.tag)) return false
    if (filters.minDistanceMi !== undefined && d.distanceMi < filters.minDistanceMi) {
      return false
    }
    return true
  })
}

export function useDrivesList(): DrivesListState {
  const [params, setParams] = useSearchParams()
  // Hydrate synchronously so back-navigation does not flash an empty list.
  const [drives, setDrives] = useState<DriveSummary[]>(
    () => listCache?.drives ?? [],
  )
  const [routes, setRoutes] = useState<RouteOverview[]>(
    () => listCache?.routes ?? [],
  )
  const [loading, setLoading] = useState(listCache === null)
  const [error, setError] = useState<string | null>(null)
  const [refreshTick, setRefreshTick] = useState(0)

  const page = Math.max(1, Number(params.get("page") ?? "1"))
  const sortDir = (params.get("sort") === "asc" ? "asc" : "desc") as "asc" | "desc"
  const range = useMemo(() => readRange(params), [params])
  const filters = useMemo(() => readFilters(params), [params])

  useEffect(() => {
    let cancelled = false
    const cacheFresh =
      listCache !== null && Date.now() - listCache.at < CACHE_STALE_MS
    // Initial mounts can use a fresh cache without another request.
    if (cacheFresh && refreshTick === 0) {
      return () => {
        cancelled = true
      }
    }

    // Only cold starts need a loading state; keep stale data visible.
    if (listCache === null) {
      /* eslint-disable-next-line react-hooks/set-state-in-effect */
      setLoading(true)
    }
    setError(null)
    Promise.all([fetchDrives(), fetchRouteOverviews(20).catch(() => [])])
      .then(([d, r]) => {
        if (cancelled) return
        listCache = { drives: d, routes: r, at: Date.now() }
        setDrives(d)
        setRoutes(r)
      })
      .catch((e) => {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [refreshTick])

  const routesByStartTime = useMemo(() => {
    const m = new Map<string, [number, number][]>()
    for (const r of routes) {
      if (r.startTime) m.set(r.startTime, r.points)
    }
    return m
  }, [routes])

  const filtered = useMemo(() => {
    const sorted = [...drives].sort((a, b) => {
      const cmp = new Date(a.startTime).getTime() - new Date(b.startTime).getTime()
      return sortDir === "asc" ? cmp : -cmp
    })
    return filterDrives(sorted, range, filters, new Date())
  }, [drives, range, filters, sortDir])

  const total = filtered.length
  const pageCount = Math.max(1, Math.ceil(total / PAGE_SIZE))
  const safePage = Math.min(page, pageCount)
  const pageStart = total === 0 ? 0 : (safePage - 1) * PAGE_SIZE + 1
  const pageEnd = Math.min(total, safePage * PAGE_SIZE)
  const visible = filtered.slice((safePage - 1) * PAGE_SIZE, safePage * PAGE_SIZE)

  // Summary stats cover the full filtered set, not the current page.
  const filteredStats = useMemo<DrivesFilteredStats>(
    () => computeFilteredStats(filtered),
    [filtered],
  )

  const updateParams = (mut: (p: URLSearchParams) => void) => {
    const next = new URLSearchParams(params)
    mut(next)
    setParams(next, { replace: true })
  }

  const setPage = (n: number) => {
    const clamped = Math.max(1, Math.min(pageCount, n))
    updateParams((p) => {
      if (clamped === 1) p.delete("page")
      else p.set("page", String(clamped))
    })
  }

  const setRange = (r: DateRange) => {
    updateParams((p) => {
      p.delete("page")
      p.delete("start")
      p.delete("end")
      p.delete("range")
      if (r.kind === "custom") {
        p.set("start", r.start)
        p.set("end", r.end)
      } else if (r.preset !== "last7") {
        p.set("range", r.preset)
      }
    })
  }

  const setFilters = (f: DrivesFilters) => {
    updateParams((p) => {
      p.delete("page")
      p.delete("tag")
      p.delete("minDist")
      // Remove unsupported filter parameters once filters change.
      p.delete("origin")
      p.delete("destination")
      if (f.tag) p.set("tag", f.tag)
      if (f.minDistanceMi !== undefined) p.set("minDist", String(f.minDistanceMi))
    })
  }

  const setSortDir = (d: "asc" | "desc") => {
    updateParams((p) => {
      if (d === "desc") p.delete("sort")
      else p.set("sort", "asc")
    })
  }

  const refresh = async () => {
    setRefreshTick((t) => t + 1)
  }

  const patchDriveTags = (id: number, tags: string[]) => {
    setDrives((prev) => prev.map((d) => (d.id === id ? { ...d, tags } : d)))
    // Keep the navigation cache consistent with the optimistic edit.
    if (listCache) {
      listCache = {
        ...listCache,
        drives: listCache.drives.map((d) =>
          d.id === id ? { ...d, tags } : d,
        ),
      }
    }
  }

  return {
    drives,
    visible,
    routesByStartTime,
    total,
    page: safePage,
    pageCount,
    pageStart,
    pageEnd,
    range,
    filters,
    sortDir,
    filteredStats,
    loading,
    error,
    setPage,
    setRange,
    setFilters,
    setSortDir,
    refresh,
    patchDriveTags,
  }
}
