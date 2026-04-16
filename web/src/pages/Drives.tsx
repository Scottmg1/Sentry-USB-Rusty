import { useCallback, useEffect, useRef, useState } from "react"
import { Link } from "react-router-dom"
import L from "leaflet"
import "leaflet/dist/leaflet.css"
import {
  MapPin, Navigation, Clock, Gauge, Play, Pause,
  Download, Upload, Loader2, ChevronLeft, Search, List, X,
  Tag, Plus, Layers, RefreshCw, AlertTriangle,
  Eye, EyeOff, Zap, ChevronRight,
} from "lucide-react"
import { cn } from "@/lib/utils"

// ── Types ──────────────────────────────────────────────────────────

interface DriveSummary {
  id: number
  date: string
  startTime: string
  endTime: string
  durationMs: number
  distanceMi: number
  distanceKm: number
  avgSpeedMph: number
  maxSpeedMph: number
  avgSpeedKmh: number
  maxSpeedKmh: number
  clipCount: number
  pointCount: number
  startPoint: [number, number] | null
  endPoint: [number, number] | null
  tags?: string[]
  fsdEngagedMs: number
  fsdDisengagements: number
  fsdAccelPushes: number
  fsdPercent: number
  fsdDistanceKm: number
  fsdDistanceMi: number
  autosteerEngagedMs: number
  autosteerPercent: number
  autosteerDistanceKm: number
  autosteerDistanceMi: number
  taccEngagedMs: number
  taccPercent: number
  taccDistanceKm: number
  taccDistanceMi: number
  assistedPercent: number
}

interface FSDEventPoint {
  lat: number
  lng: number
  type: "disengagement" | "accel_push"
}

interface DriveDetail extends Omit<DriveSummary, "startPoint" | "endPoint"> {
  points: [number, number, number, number][] // [lat, lng, timeMs, speedMps]
  gearStates?: number[] // parallel to points: 0=P, 1=D, 2=R, 3=N
  fsdStates?: number[] // parallel to points: 0=manual, >0=FSD engaged
  fsdEvents?: FSDEventPoint[]
  tags?: string[]
}

interface RouteOverview {
  id: number
  points: [number, number][]
}

interface DriveStats {
  drives_count: number
  routes_count: number
  processed_count: number
  total_distance_km: number
  total_distance_mi: number
  total_duration_ms: number
  fsd_engaged_ms: number
  fsd_distance_km: number
  fsd_distance_mi: number
  fsd_percent: number
  fsd_disengagements: number
  fsd_accel_pushes: number
  autosteer_engaged_ms: number
  autosteer_distance_km: number
  autosteer_distance_mi: number
  tacc_engaged_ms: number
  tacc_distance_km: number
  tacc_distance_mi: number
  assisted_percent: number
}

// ── Helpers ────────────────────────────────────────────────────────

const GEAR_LABELS: Record<number, { text: string; color: string }> = {
  0: { text: "P", color: "text-blue-400" },
  1: { text: "D", color: "text-emerald-400" },
  2: { text: "R", color: "text-red-400" },
  3: { text: "N", color: "text-amber-400" },
}

function formatDuration(ms: number) {
  const totalMin = Math.floor(ms / 60000)
  const h = Math.floor(totalMin / 60)
  const m = totalMin % 60
  return h > 0 ? `${h}h ${m}m` : `${m} min`
}

/** Returns the label for the dominant assisted-driving mode in a drive */
function assistedModeLabel(d: { fsdPercent: number; autosteerPercent: number; taccPercent: number }): string {
  if (d.fsdPercent >= d.autosteerPercent && d.fsdPercent >= d.taccPercent && d.fsdPercent > 0) return "FSD"
  if (d.autosteerPercent >= d.fsdPercent && d.autosteerPercent >= d.taccPercent && d.autosteerPercent > 0) return "Autopilot"
  if (d.taccPercent > 0) return "TACC"
  return ""
}

/** Returns total assisted percent (FSD + Autopilot + TACC) */
function totalAssistedPercent(d: { fsdPercent: number; autosteerPercent: number; taccPercent: number; assistedPercent: number }): number {
  return d.assistedPercent || 0
}

function formatTime(iso: string) {
  return new Date(iso).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
}

function formatTimeMs(ms: number) {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", timeZone: "UTC" })
}

function formatDate(dateStr: string) {
  const d = new Date(dateStr + "T00:00:00")
  return d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric", year: "numeric" })
}

function haversine(lat1: number, lon1: number, lat2: number, lon2: number) {
  const R = 6371000
  const toRad = (d: number) => (d * Math.PI) / 180
  const dLat = toRad(lat2 - lat1)
  const dLon = toRad(lon2 - lon1)
  const a = Math.sin(dLat / 2) ** 2 + Math.cos(toRad(lat1)) * Math.cos(toRad(lat2)) * Math.sin(dLon / 2) ** 2
  return R * 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a))
}

type MapStyle = "dark" | "streets" | "google" | "satellite"

const TILE_LAYERS: Record<MapStyle, { url: string; attribution: string; subdomains?: string; maxZoom?: number }> = {
  dark: {
    url: "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png",
    attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OSM</a> &copy; <a href="https://carto.com/">CARTO</a>',
    subdomains: "abcd",
    maxZoom: 20,
  },
  streets: {
    url: "https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png",
    attribution: '&copy; <a href="https://www.openstreetmap.org/copyright">OpenStreetMap</a>',
    subdomains: "abc",
    maxZoom: 19,
  },
  google: {
    url: "https://mt1.google.com/vt/lyrs=m&x={x}&y={y}&z={z}",
    attribution: '&copy; Google',
    maxZoom: 20,
  },
  satellite: {
    url: "https://mt1.google.com/vt/lyrs=s&x={x}&y={y}&z={z}",
    attribution: '&copy; Google',
    maxZoom: 20,
  },
}

// ── Component ──────────────────────────────────────────────────────

export default function Drives() {
  const mapRef = useRef<HTMLDivElement>(null)
  const mapInstance = useRef<L.Map | null>(null)
  const overviewLayers = useRef<L.Polyline[]>([])
  const selectionLayers = useRef<L.Layer[]>([])
  const arrowMarker = useRef<L.Marker | null>(null)
  const tileLayerRef = useRef<L.TileLayer | null>(null)

  const [drives, setDrives] = useState<DriveSummary[]>([])
  const [stats, setStats] = useState<DriveStats | null>(null)
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const [selectedDrive, setSelectedDrive] = useState<DriveDetail | null>(null)
  const [search, setSearch] = useState("")
  const [metric, setMetric] = useState(false)
  const [mapStyle, setMapStyle] = useState<MapStyle>("dark")
  const [showLayerPicker, setShowLayerPicker] = useState(false)

  // Load unit from setup config (DRIVE_MAP_UNIT set in wizard)
  useEffect(() => {
    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        const entry = cfg.DRIVE_MAP_UNIT
        if (entry) {
          const val = typeof entry === "object"
            ? (entry.active ? entry.value : null)
            : entry
          if (val !== null) setMetric(val === "km")
        }
      })
      .catch(() => { })
  }, [])
  const [sliderIdx, setSliderIdx] = useState(0)
  const [sliderPlaying, setSliderPlaying] = useState(false)
  const sliderPlayRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const [loading, setLoading] = useState(true)
  const [processing, setProcessing] = useState(false)
  const [processMsg, setProcessMsg] = useState("")
  const [mobileListOpen, setMobileListOpen] = useState(false)
  const [visibleCount, setVisibleCount] = useState(30)
  const sentinelRef = useRef<HTMLDivElement>(null)
  const mobileSentinelRef = useRef<HTMLDivElement>(null)

  const fileInputRef = useRef<HTMLInputElement>(null)

  const [allTags, setAllTags] = useState<string[]>([])
  const [tagFilter, setTagFilter] = useState<string>("")
  const [tagInput, setTagInput] = useState("")
  const [showTagInput, setShowTagInput] = useState(false)
  const [listTagInputId, setListTagInputId] = useState<number | null>(null)
  const [listTagValue, setListTagValue] = useState("")
  const [showProcessMenu, setShowProcessMenu] = useState(false)
  const [archiving, setArchiving] = useState(false)
  const [showFSDMarkers, setShowFSDMarkers] = useState(true)
  const [importing, setImporting] = useState(false)
  const [importMsg, setImportMsg] = useState("")
  const fsdEventLayers = useRef<L.Layer[]>([])

  // ── Init map ──
  useEffect(() => {
    if (!mapRef.current || mapInstance.current) return
    const map = L.map(mapRef.current, { zoomControl: false, preferCanvas: true }).setView([39.8, -98.6], 5)
    L.control.zoom({ position: "bottomright" }).addTo(map)
    const initCfg = TILE_LAYERS.dark
    tileLayerRef.current = L.tileLayer(initCfg.url, {
      attribution: initCfg.attribution,
      subdomains: initCfg.subdomains || "abc",
      maxZoom: initCfg.maxZoom || 20,
    }).addTo(map)
    mapInstance.current = map
    return () => { map.remove(); mapInstance.current = null }
  }, [])

  // ── Swap tile layer on style change ──
  const skipInitialTileSwap = useRef(true)
  useEffect(() => {
    if (skipInitialTileSwap.current) { skipInitialTileSwap.current = false; return }
    const map = mapInstance.current
    if (!map) return
    if (tileLayerRef.current) map.removeLayer(tileLayerRef.current)
    const cfg = TILE_LAYERS[mapStyle]
    tileLayerRef.current = L.tileLayer(cfg.url, {
      attribution: cfg.attribution,
      subdomains: cfg.subdomains || "abc",
      maxZoom: cfg.maxZoom || 20,
    }).addTo(map)
  }, [mapStyle])

  // ── Load data ──
  const loadDrives = useCallback(async () => {
    setLoading(true)
    try {
      const [drivesRes, statsRes] = await Promise.all([
        fetch("/api/drives"),
        fetch("/api/drives/stats"),
      ])
      const drivesData: DriveSummary[] = await drivesRes.json()
      const statsData: DriveStats = await statsRes.json()
      drivesData.sort((a, b) => new Date(b.startTime).getTime() - new Date(a.startTime).getTime())
      setDrives(drivesData)
      setStats(statsData)

      // Draw overview routes
      const [routesRes, tagsRes] = await Promise.all([
        fetch("/api/drives/routes"),
        fetch("/api/drives/tags"),
      ])
      const routes: RouteOverview[] = await routesRes.json()
      const tagsData: string[] = await tagsRes.json()
      setAllTags(tagsData ?? [])
      drawOverview(routes)
    } catch {
      // API may not be available in dev
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => { loadDrives() }, [loadDrives])

  function drawOverview(routes: RouteOverview[]) {
    const map = mapInstance.current
    if (!map) return
    // Clear old
    overviewLayers.current.forEach((l) => map.removeLayer(l))
    overviewLayers.current = []

    for (const r of routes) {
      if (r.points && r.points.length > 1) {
        const line = L.polyline(r.points as L.LatLngExpression[], {
          color: "#3b82f6", weight: 2, opacity: 0.4, smoothFactor: 1.2,
        }).addTo(map)
          ; (line as any)._driveId = r.id
        line.on("click", () => selectDrive(r.id))
        overviewLayers.current.push(line)
      }
    }
    if (overviewLayers.current.length > 0) {
      const group = L.featureGroup(overviewLayers.current)
      map.fitBounds(group.getBounds(), { padding: [40, 40] })
    }
  }

  function clearSelection() {
    const map = mapInstance.current
    if (!map) return
    selectionLayers.current.forEach((l) => map.removeLayer(l))
    selectionLayers.current = []
    fsdEventLayers.current = []
    if (arrowMarker.current) { map.removeLayer(arrowMarker.current); arrowMarker.current = null }
  }

  async function saveTags(driveId: number, startTime: string, tags: string[]) {
    try {
      // Update local state optimistically
      setDrives((prev) => prev.map((d) => d.id === driveId ? { ...d, tags } : d))
      if (selectedDrive && selectedId === driveId) {
        setSelectedDrive({ ...selectedDrive, tags })
      }
      // Update tag list locally (add any new tags)
      setAllTags((prev) => {
        const s = new Set(prev)
        for (const t of tags) if (!s.has(t)) s.add(t)
        return s.size !== prev.length ? Array.from(s) : prev
      })
      await fetch(`/api/drives/${driveId}/tags`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ tags, start_time: startTime }),
      })
    } catch { /* ignore */ }
  }

  function addTagToDrive(driveId: number, startTime: string, currentTags: string[], tag: string) {
    const trimmed = tag.trim()
    if (!trimmed || currentTags.includes(trimmed)) return
    saveTags(driveId, startTime, [...currentTags, trimmed])
  }

  function removeTagFromDrive(driveId: number, startTime: string, currentTags: string[], tag: string) {
    saveTags(driveId, startTime, currentTags.filter((t) => t !== tag))
  }

  async function selectDrive(id: number) {
    setSelectedId(id)
    setSliderIdx(0)
    setShowTagInput(false)
    setTagInput("")
    const map = mapInstance.current
    if (!map) return

    try {
      const res = await fetch(`/api/drives/${id}`)
      const data: DriveDetail = await res.json()
      setSelectedDrive(data)

      clearSelection()
      overviewLayers.current.forEach((l) => map.removeLayer(l))

      const pts = data.points
      if (!pts || pts.length < 2) return
      const latlngs = pts.map((p) => [p[0], p[1]] as L.LatLngExpression)
      const fsd = data.fsdStates

      // Draw route with FSD coloring if available
      if (fsd && fsd.length === pts.length) {
        // Split into segments by FSD state
        let segStart = 0
        for (let i = 1; i <= pts.length; i++) {
          const prevEngaged = fsd[i - 1] > 0
          const curEngaged = i < pts.length ? fsd[i] > 0 : !prevEngaged
          if (curEngaged !== prevEngaged || i === pts.length) {
            const segPts = latlngs.slice(segStart, i)
            if (segPts.length >= 2) {
              const color = prevEngaged ? "#22c55e" : "#3b82f6" // green for FSD, blue for manual
              const line = L.polyline(segPts, { color, weight: 4, opacity: 1, smoothFactor: 1.2 }).addTo(map)
              selectionLayers.current.push(line)
            }
            segStart = Math.max(i - 1, 0) // overlap by 1 point for continuity
          }
        }
      } else {
        const route = L.polyline(latlngs, { color: "#3b82f6", weight: 4, opacity: 1, smoothFactor: 1.2 }).addTo(map)
        selectionLayers.current.push(route)
      }

      const startM = L.marker(latlngs[0], {
        icon: L.divIcon({ className: "", html: '<div style="width:10px;height:10px;border-radius:50%;background:#22c55e;border:2px solid #fff"></div>', iconSize: [10, 10], iconAnchor: [5, 5] }),
      }).addTo(map)
      const endM = L.marker(latlngs[latlngs.length - 1], {
        icon: L.divIcon({ className: "", html: '<div style="width:10px;height:10px;border-radius:50%;background:#ef4444;border:2px solid #fff"></div>', iconSize: [10, 10], iconAnchor: [5, 5] }),
      }).addTo(map)
      selectionLayers.current.push(startM, endM)

      // Draw FSD event markers
      fsdEventLayers.current = []
      if (data.fsdEvents && data.fsdEvents.length > 0) {
        for (const ev of data.fsdEvents) {
          const isDisengage = ev.type === "disengagement"
          const color = isDisengage ? "#ef4444" : "#f59e0b"
          const label = isDisengage ? "D" : "A"
          const title = isDisengage ? "FSD Disengagement" : "Accel Push"
          const m = L.marker([ev.lat, ev.lng], {
            icon: L.divIcon({
              className: "",
              html: `<div title="${title}" style="width:16px;height:16px;border-radius:50%;background:${color};border:2px solid #fff;display:flex;align-items:center;justify-content:center;font-size:9px;font-weight:bold;color:#fff;line-height:1;box-shadow:0 0 4px rgba(0,0,0,0.5)">${label}</div>`,
              iconSize: [16, 16],
              iconAnchor: [8, 8],
            }),
          }).bindTooltip(title, { permanent: false, direction: "top", offset: [0, -10] })
          if (showFSDMarkers) m.addTo(map)
          fsdEventLayers.current.push(m)
          selectionLayers.current.push(m)
        }
      }

      const arrow = L.marker(latlngs[0], {
        icon: L.divIcon({
          className: "",
          html: '<div style="width:12px;height:12px;border-radius:50%;background:#3b82f6;border:2px solid #fff;box-shadow:0 0 6px rgba(59,130,246,0.6)"></div>',
          iconSize: [12, 12], iconAnchor: [6, 6],
        }),
      }).addTo(map)
      arrowMarker.current = arrow
      selectionLayers.current.push(arrow)

      const allSelLines = selectionLayers.current.filter((l): l is L.Polyline => l instanceof L.Polyline)
      if (allSelLines.length > 0) {
        map.fitBounds(L.featureGroup(allSelLines).getBounds(), { padding: [60, 60] as L.PointExpression })
      }

      // Re-validate map size after the detail panel renders and changes available height
      requestAnimationFrame(() => map.invalidateSize())
    } catch {
      // ignore
    }
  }

  // Toggle FSD event markers on/off
  useEffect(() => {
    const map = mapInstance.current
    if (!map) return
    for (const layer of fsdEventLayers.current) {
      if (showFSDMarkers) {
        if (!map.hasLayer(layer)) layer.addTo(map)
      } else {
        map.removeLayer(layer)
      }
    }
  }, [showFSDMarkers])

  function goBack() {
    setSelectedId(null)
    setSelectedDrive(null)
    clearSelection()
    const map = mapInstance.current
    if (!map) return
    overviewLayers.current.forEach((l) => l.addTo(map))
    if (overviewLayers.current.length > 0) {
      map.fitBounds(L.featureGroup(overviewLayers.current).getBounds(), { padding: [40, 40] })
    }
    requestAnimationFrame(() => map.invalidateSize())
  }

  function handleSlider(idx: number) {
    setSliderIdx(idx)
    if (!selectedDrive || !arrowMarker.current) return
    const pt = selectedDrive.points[idx]
    arrowMarker.current.setLatLng([pt[0], pt[1]])
  }

  function toggleSliderPlay() {
    if (sliderPlaying) {
      if (sliderPlayRef.current) clearInterval(sliderPlayRef.current)
      sliderPlayRef.current = null
      setSliderPlaying(false)
      return
    }
    if (!selectedDrive || selectedDrive.points.length === 0) return
    // If at end, restart from beginning
    if (sliderIdx >= selectedDrive.points.length - 1) {
      handleSlider(0)
    }
    setSliderPlaying(true)
    sliderPlayRef.current = setInterval(() => {
      setSliderIdx((prev) => {
        const drive = selectedDrive
        if (!drive) return prev
        const next = prev + 1
        if (next >= drive.points.length) {
          if (sliderPlayRef.current) clearInterval(sliderPlayRef.current)
          sliderPlayRef.current = null
          setSliderPlaying(false)
          return drive.points.length - 1
        }
        const pt = drive.points[next]
        if (arrowMarker.current) arrowMarker.current.setLatLng([pt[0], pt[1]])
        return next
      })
    }, 100)
  }

  // Stop playback when drive changes
  useEffect(() => {
    if (sliderPlayRef.current) {
      clearInterval(sliderPlayRef.current)
      sliderPlayRef.current = null
    }
    setSliderPlaying(false)
  }, [selectedId])

  // ── Check archive status ──
  useEffect(() => {
    async function checkArchive() {
      try {
        const s = await fetch("/api/drives/status")
        const data = await s.json()
        setArchiving(!!data.archiving)
      } catch { /* ignore */ }
    }
    checkArchive()
    const iv = setInterval(checkArchive, 5000)
    return () => clearInterval(iv)
  }, [])

  // ── Process ──
  async function triggerProcess(mode: "new" | "reprocess" = "new") {
    setProcessing(true)
    setShowProcessMenu(false)
    const modeLabel = mode === "new" ? "Processing new drives" : "Reprocessing all drives"
    setProcessMsg(`${modeLabel}...`)
    try {
      const url = mode === "new" ? "/api/drives/process" : "/api/drives/reprocess"
      const res = await fetch(url, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: mode === "new" ? JSON.stringify({ throttle_ms: 15 }) : "{}",
      })
      if (!res.ok) {
        const err = await res.json()
        setProcessMsg(`Error: ${err.error}`)
        setProcessing(false)
        return
      }
      setProcessMsg(`${modeLabel}... check back shortly`)
      // Poll status
      const poll = setInterval(async () => {
        try {
          const s = await fetch("/api/drives/status")
          const data = await s.json()
          if (!data.running) {
            clearInterval(poll)
            setProcessing(false)
            setProcessMsg("")
            loadDrives()
          }
        } catch { clearInterval(poll); setProcessing(false) }
      }, 3000)
    } catch {
      setProcessMsg("Failed to start processing")
      setProcessing(false)
    }
  }

  // ── Upload / Download ──
  async function handleUpload(file: File) {
    setImporting(true)
    setImportMsg("")
    try {
      const text = await file.text()
      const res = await fetch("/api/drives/data/upload", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: text,
      })
      if (res.ok) {
        const result = await res.json()
        setImportMsg(`Imported ${result.routes_count} drive${result.routes_count !== 1 ? "s" : ""}`)
        loadDrives()
      } else {
        const err = await res.json().catch(() => null)
        setImportMsg(err?.error || `Import failed (${res.status})`)
      }
    } catch (err) {
      setImportMsg(`Import failed — ${err instanceof Error ? err.message : "could not reach server"}`)
    } finally {
      setImporting(false)
      setTimeout(() => setImportMsg(""), 5000)
    }
  }

  // ── Derived ──
  // Reset visible count when search changes
  useEffect(() => { setVisibleCount(30) }, [search])

  // IntersectionObserver for lazy loading more drives
  useEffect(() => {
    const cb: IntersectionObserverCallback = (entries) => {
      if (entries.some((e) => e.isIntersecting)) {
        setVisibleCount((c) => c + 30)
      }
    }
    const obs = new IntersectionObserver(cb, { rootMargin: "200px" })
    if (sentinelRef.current) obs.observe(sentinelRef.current)
    if (mobileSentinelRef.current) obs.observe(mobileSentinelRef.current)
    return () => obs.disconnect()
  }, [drives, search])

  const filtered = drives.filter((d) => {
    // Tag filter
    if (tagFilter && !(d.tags ?? []).includes(tagFilter)) return false
    // Text search
    if (search) {
      const q = search.toLowerCase()
      const matchDate = d.date.includes(search) || formatDate(d.date).toLowerCase().includes(q)
      const matchTag = (d.tags ?? []).some((t) => t.toLowerCase().includes(q))
      return matchDate || matchTag
    }
    return true
  })
  const visible = filtered.slice(0, visibleCount)

  const dist = (d: DriveSummary | DriveDetail) => metric ? `${d.distanceKm} km` : `${d.distanceMi} mi`
  const avgSpd = (d: DriveSummary | DriveDetail) => metric ? `${d.avgSpeedKmh} km/h` : `${d.avgSpeedMph} mph`
  const maxSpd = (d: DriveSummary | DriveDetail) => metric ? `${d.maxSpeedKmh} km/h` : `${d.maxSpeedMph} mph`
  const mpsToDisplay = (mps: number) => metric ? (Math.abs(mps) * 3.6).toFixed(1) : (Math.abs(mps) * 2.23694).toFixed(1)
  const distUnit = metric ? "km" : "mi"
  const speedUnit = metric ? "km/h" : "mph"

  // Cumulative distances for slider
  const cumDist = selectedDrive
    ? selectedDrive.points.reduce<number[]>((acc, pt, i) => {
      if (i === 0) return [0]
      const prev = selectedDrive.points[i - 1]
      acc.push(acc[i - 1] + haversine(prev[0], prev[1], pt[0], pt[1]))
      return acc
    }, [])
    : []

  const sliderPt = selectedDrive?.points[sliderIdx]
  const sliderDist = cumDist[sliderIdx] ?? 0
  const sliderDistDisplay = metric ? (sliderDist / 1000).toFixed(2) : (sliderDist / 1609.344).toFixed(2)

  const isFiltered = tagFilter !== "" || search !== ""
  const filteredStats = isFiltered
    ? filtered.reduce(
      (acc, d) => ({
        count: acc.count + 1,
        distKm: acc.distKm + d.distanceKm,
        distMi: acc.distMi + d.distanceMi,
        durMs: acc.durMs + d.durationMs,
      }),
      { count: 0, distKm: 0, distMi: 0, durMs: 0 }
    )
    : null
  const displayCount = filteredStats ? filteredStats.count : stats?.drives_count ?? 0
  const totalDist = filteredStats
    ? (metric ? filteredStats.distKm.toFixed(1) : filteredStats.distMi.toFixed(1))
    : stats ? (metric ? stats.total_distance_km.toFixed(1) : stats.total_distance_mi.toFixed(1)) : "0"
  const totalDur = filteredStats
    ? formatDuration(filteredStats.durMs)
    : stats ? formatDuration(stats.total_duration_ms) : "0"

  return (
    <div className="flex h-[calc(100vh-5rem)] flex-col gap-4 md:h-[calc(100vh-3rem)]">
      {/* Header bar */}
      <div className="flex flex-wrap items-center justify-between gap-2 sm:gap-3">
        <div className="flex items-center gap-3">
          <MapPin className="h-5 w-5 text-blue-400" />
          <h1 className="text-lg font-semibold text-slate-100">Drive Map</h1>
          {stats && (
            <div className="hidden items-center gap-4 text-xs text-slate-500 sm:flex">
              <span>Drives: <span className="font-semibold text-blue-400">{displayCount}</span>{isFiltered && <span className="text-slate-600">/{stats.drives_count}</span>}</span>
              <span>Total: <span className="font-semibold text-blue-400">{totalDist} {distUnit}</span></span>
              <span>Time: <span className="font-semibold text-blue-400">{totalDur}</span></span>
            </div>
          )}
        </div>
        <div className="flex flex-wrap items-center gap-2">
          {/* FSD Analytics */}
          {stats && stats.fsd_engaged_ms > 0 && (
            <Link to="/fsd" className="flex items-center gap-1.5 rounded-lg border border-emerald-500/30 bg-emerald-500/10 px-3 py-1.5 text-xs font-medium text-emerald-400 transition-colors hover:bg-emerald-500/20">
              <Zap className="h-3 w-3" /> FSD {stats.fsd_percent}%
              {(stats.autosteer_engaged_ms > 0 || stats.tacc_engaged_ms > 0) && (
                <span className="text-slate-500 text-[10px] ml-0.5">({stats.assisted_percent}% assisted)</span>
              )}
              <ChevronRight className="h-3 w-3 opacity-50" />
            </Link>
          )}
          {/* Process dropdown */}
          <div className="relative">
            <button
              onClick={() => setShowProcessMenu(!showProcessMenu)}
              disabled={processing}
              className="flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10 disabled:opacity-50"
            >
              {processing ? <Loader2 className="h-3 w-3 animate-spin" /> : <Play className="h-3 w-3" />}
              Process
            </button>
            {showProcessMenu && !processing && (
              <div className="absolute right-0 z-[1100] mt-1 w-56 rounded-lg border border-white/10 bg-slate-950/95 py-1 shadow-xl backdrop-blur-sm">
                <button
                  onClick={() => triggerProcess("new")}
                  disabled={archiving}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-slate-300 transition-colors hover:bg-white/5 disabled:opacity-40"
                >
                  <Play className="h-3 w-3 text-blue-400" />
                  <div>
                    <p className="font-medium">Process New Drives</p>
                    <p className="text-[10px] text-slate-500">Extract GPS from unprocessed clips</p>
                  </div>
                </button>
                <button
                  onClick={() => triggerProcess("reprocess")}
                  disabled={archiving}
                  className="flex w-full items-center gap-2 px-3 py-2 text-left text-xs text-slate-300 transition-colors hover:bg-white/5 disabled:opacity-40"
                >
                  <RefreshCw className="h-3 w-3 text-amber-400" />
                  <div>
                    <p className="font-medium">Reprocess All Drives</p>
                    <p className="text-[10px] text-slate-500">Re-extract existing clips on disk only</p>
                  </div>
                </button>
                {archiving && (
                  <div className="flex items-center gap-1.5 border-t border-white/5 px-3 py-2 text-[10px] text-amber-400">
                    <AlertTriangle className="h-3 w-3" /> Archive in progress — wait to process
                  </div>
                )}
                <div className="border-t border-white/5 px-3 py-2 text-[10px] text-slate-600">
                  Note: Clips removed from snapshots cannot be reprocessed.
                </div>
              </div>
            )}
          </div>
          {/* Download */}
          <a href="/api/drives/data/download" className="flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10">
            <Download className="h-3 w-3" /> Export
          </a>
          {/* Upload */}
          <button onClick={() => fileInputRef.current?.click()} disabled={importing} className="flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10 disabled:opacity-50 disabled:pointer-events-none">
            {importing ? <Loader2 className="h-3 w-3 animate-spin" /> : <Upload className="h-3 w-3" />} {importing ? "Importing…" : "Import"}
          </button>
          <input ref={fileInputRef} type="file" accept=".json" className="hidden" onChange={(e) => { const f = e.target.files?.[0]; if (f) handleUpload(f) }} />
        </div>
      </div>

      {processMsg && <p className="text-xs text-amber-400">{processMsg}</p>}
      {importMsg && <p className={cn("text-xs", importMsg.startsWith("Imported") ? "text-emerald-400" : "text-red-400")}>{importMsg}</p>}

      {/* Main content: sidebar + map */}
      <div className="relative flex flex-1 gap-4 overflow-hidden rounded-xl border border-white/5">
        {/* Mobile drive list toggle */}
        <button
          onClick={() => setMobileListOpen(!mobileListOpen)}
          className="absolute left-3 bottom-3 z-[1000] flex items-center gap-1.5 rounded-lg border border-white/10 bg-slate-950/90 px-3 py-2 text-xs font-medium text-slate-300 backdrop-blur-sm transition-colors hover:bg-slate-900 md:hidden"
        >
          {mobileListOpen ? <X className="h-3.5 w-3.5" /> : <List className="h-3.5 w-3.5" />}
          {mobileListOpen ? "Hide Drives" : `Drives (${drives.length})`}
        </button>

        {/* Mobile drive list overlay */}
        {mobileListOpen && (
          <div className="absolute inset-0 z-[1000] flex flex-col overflow-hidden bg-slate-950/95 backdrop-blur-sm md:hidden">
            <div className="border-b border-white/5 p-3">
              <div className="relative">
                <Search className="absolute left-2.5 top-2 h-3.5 w-3.5 text-slate-600" />
                <input
                  type="text"
                  value={search}
                  onChange={(e) => setSearch(e.target.value)}
                  placeholder="Filter by date or tag..."
                  className="w-full rounded-lg border border-white/10 bg-white/5 py-1.5 pl-8 pr-3 text-xs text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
                />
              </div>
              {allTags.length > 0 && (
                <div className="mt-2 flex flex-wrap gap-1">
                  <button
                    onClick={() => setTagFilter("")}
                    className={cn(
                      "rounded-full px-2 py-0.5 text-[10px] font-medium transition-colors",
                      tagFilter === "" ? "bg-blue-500/20 text-blue-400" : "bg-white/5 text-slate-500 hover:text-slate-300"
                    )}
                  >All</button>
                  {allTags.map((t) => (
                    <button
                      key={t}
                      onClick={() => setTagFilter(tagFilter === t ? "" : t)}
                      className={cn(
                        "rounded-full px-2 py-0.5 text-[10px] font-medium transition-colors",
                        tagFilter === t ? "bg-blue-500/20 text-blue-400" : "bg-white/5 text-slate-500 hover:text-slate-300"
                      )}
                    >
                      <Tag className="mr-0.5 inline h-2.5 w-2.5" />{t}
                    </button>
                  ))}
                </div>
              )}
            </div>
            <div className="flex-1 overflow-y-auto">
              {loading && <p className="p-4 text-center text-xs text-slate-600">Loading drives...</p>}
              {!loading && filtered.length === 0 && <p className="p-4 text-center text-xs text-slate-600">No drives found</p>}
              {(() => {
                let cd = ""
                return visible.map((d) => {
                  const sh = d.date !== cd
                  cd = d.date
                  return (
                    <div key={d.id}>
                      {sh && (
                        <div className="sticky top-0 z-10 bg-slate-950/90 px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wider text-slate-600">
                          {formatDate(d.date)}
                        </div>
                      )}
                      <button
                        onClick={() => { selectDrive(d.id); setMobileListOpen(false) }}
                        className={cn(
                          "w-full border-b border-white/[0.03] px-3 py-2.5 text-left transition-colors hover:bg-white/[0.04]",
                          selectedId === d.id && "border-l-2 border-l-blue-500 bg-blue-500/10"
                        )}
                      >
                        <div className="flex items-start justify-between">
                          <p className="text-sm font-medium text-slate-200">
                            {formatTime(d.startTime)} — {formatTime(d.endTime)}
                          </p>
                          {totalAssistedPercent(d) > 0 && (
                            <span className={cn(
                              "ml-1 shrink-0 rounded-full px-1.5 py-0.5 text-[10px] font-bold",
                              d.fsdPercent >= 95 ? "bg-emerald-500/15 text-emerald-400" : d.fsdPercent >= 50 ? "bg-blue-500/15 text-blue-400" : "bg-slate-700 text-slate-400"
                            )}>
                              {totalAssistedPercent(d)}% {assistedModeLabel(d) || "Assisted"}
                            </span>
                          )}
                        </div>
                        <div className="mt-1 flex gap-x-3 text-[11px] text-slate-500">
                          <span>{dist(d)}</span>
                          <span>{formatDuration(d.durationMs)}</span>
                          <span>{avgSpd(d)}</span>
                        </div>
                        {d.fsdDisengagements > 0 && (
                          <div className="mt-0.5 text-[11px] text-red-400/70">{d.fsdDisengagements} disengagement{d.fsdDisengagements !== 1 ? "s" : ""}</div>
                        )}
                        <div className="mt-1.5 flex flex-wrap items-center gap-1">
                          {(d.tags ?? []).map((t) => (
                            <span key={t} className="group/tag inline-flex items-center rounded-full bg-blue-500/10 px-1.5 py-0.5 text-[10px] font-medium text-blue-400">
                              <Tag className="mr-0.5 h-2 w-2" />{t}
                              <button
                                onClick={(e) => { e.stopPropagation(); removeTagFromDrive(d.id, d.startTime, d.tags ?? [], t) }}
                                className="ml-0.5 hidden rounded-full p-0.5 text-blue-400/60 hover:bg-blue-500/20 hover:text-blue-300 group-hover/tag:inline-flex"
                              ><X className="h-2 w-2" /></button>
                            </span>
                          ))}
                          {listTagInputId === d.id ? (
                            <>
                              {allTags
                                .filter((t) => !(d.tags ?? []).includes(t) && (!listTagValue || t.toLowerCase().includes(listTagValue.toLowerCase())))
                                .map((t) => (
                                  <button
                                    key={t}
                                    onMouseDown={(e) => {
                                      e.preventDefault()
                                      e.stopPropagation()
                                      addTagToDrive(d.id, d.startTime, d.tags ?? [], t)
                                      setListTagValue(""); setListTagInputId(null)
                                    }}
                                    onClick={(e) => e.stopPropagation()}
                                    className="inline-flex items-center gap-0.5 rounded-full border border-dashed border-blue-500/20 bg-blue-500/5 px-1.5 py-0.5 text-[10px] font-medium text-blue-400/70 transition-colors hover:border-blue-500/40 hover:bg-blue-500/15 hover:text-blue-400"
                                  >
                                    <Plus className="h-2 w-2" />{t}
                                  </button>
                                ))}
                              <input
                                autoFocus
                                value={listTagValue}
                                onChange={(e) => setListTagValue(e.target.value)}
                                onClick={(e) => e.stopPropagation()}
                                onKeyDown={(e) => {
                                  e.stopPropagation()
                                  if (e.key === "Enter" && listTagValue.trim()) {
                                    addTagToDrive(d.id, d.startTime, d.tags ?? [], listTagValue)
                                    setListTagValue(""); setListTagInputId(null)
                                  }
                                  if (e.key === "Escape") { setListTagInputId(null); setListTagValue("") }
                                }}
                                onBlur={() => { setListTagInputId(null); setListTagValue("") }}
                                placeholder="New..."
                                className="w-16 rounded-full border border-blue-500/30 bg-white/5 px-1.5 py-0.5 text-[10px] text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
                              />
                            </>
                          ) : (
                            <button
                              onClick={(e) => { e.stopPropagation(); setListTagInputId(d.id); setListTagValue("") }}
                              className="inline-flex items-center gap-0.5 rounded-full border border-dashed border-white/10 px-1.5 py-0.5 text-[10px] text-slate-600 transition-colors hover:border-blue-500/30 hover:text-blue-400"
                            >
                              <Plus className="h-2 w-2" />
                            </button>
                          )}
                        </div>
                      </button>
                    </div>
                  )
                })
              })()}
              {visibleCount < filtered.length && <div ref={mobileSentinelRef} className="py-4 text-center text-[10px] text-slate-600">Loading more...</div>}
            </div>
          </div>
        )}

        {/* Desktop Sidebar */}
        <div className="hidden w-72 shrink-0 flex-col overflow-hidden border-r border-white/5 bg-white/[0.02] md:flex">
          <div className="border-b border-white/5 p-3">
            <div className="relative">
              <Search className="absolute left-2.5 top-2 h-3.5 w-3.5 text-slate-600" />
              <input
                type="text"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder="Filter by date or tag..."
                className="w-full rounded-lg border border-white/10 bg-white/5 py-1.5 pl-8 pr-3 text-xs text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
              />
            </div>
            {allTags.length > 0 && (
              <div className="mt-2 flex flex-wrap gap-1">
                <button
                  onClick={() => setTagFilter("")}
                  className={cn(
                    "rounded-full px-2 py-0.5 text-[10px] font-medium transition-colors",
                    tagFilter === "" ? "bg-blue-500/20 text-blue-400" : "bg-white/5 text-slate-500 hover:text-slate-300"
                  )}
                >All</button>
                {allTags.map((t) => (
                  <button
                    key={t}
                    onClick={() => setTagFilter(tagFilter === t ? "" : t)}
                    className={cn(
                      "rounded-full px-2 py-0.5 text-[10px] font-medium transition-colors",
                      tagFilter === t ? "bg-blue-500/20 text-blue-400" : "bg-white/5 text-slate-500 hover:text-slate-300"
                    )}
                  >
                    <Tag className="mr-0.5 inline h-2.5 w-2.5" />{t}
                  </button>
                ))}
              </div>
            )}
          </div>
          <div className="flex-1 overflow-y-auto">
            {loading && <p className="p-4 text-center text-xs text-slate-600">Loading drives...</p>}
            {!loading && filtered.length === 0 && <p className="p-4 text-center text-xs text-slate-600">No drives found</p>}
            {(() => {
              let currentDate = ""
              return visible.map((d) => {
                const showHeader = d.date !== currentDate
                currentDate = d.date
                return (
                  <div key={d.id}>
                    {showHeader && (
                      <div className="sticky top-0 z-10 bg-slate-950/90 px-3 py-1.5 text-[10px] font-semibold uppercase tracking-wider text-slate-600">
                        {formatDate(d.date)}
                      </div>
                    )}
                    <button
                      onClick={() => selectDrive(d.id)}
                      className={cn(
                        "w-full border-b border-white/[0.03] px-3 py-2.5 text-left transition-colors hover:bg-white/[0.04]",
                        selectedId === d.id && "border-l-2 border-l-blue-500 bg-blue-500/10"
                      )}
                    >
                      <div className="flex items-start justify-between">
                        <p className="text-sm font-medium text-slate-200">
                          {formatTime(d.startTime)} — {formatTime(d.endTime)}
                        </p>
                        {totalAssistedPercent(d) > 0 && (
                          <span className={cn(
                            "ml-1 shrink-0 rounded-full px-1.5 py-0.5 text-[10px] font-bold",
                            d.fsdPercent >= 95 ? "bg-emerald-500/15 text-emerald-400" : d.fsdPercent >= 50 ? "bg-blue-500/15 text-blue-400" : "bg-slate-700 text-slate-400"
                          )}>
                            {totalAssistedPercent(d)}% {assistedModeLabel(d) || "Assisted"}
                          </span>
                        )}
                      </div>
                      <div className="mt-1 flex gap-x-3 text-[11px] text-slate-500">
                        <span>{dist(d)}</span>
                        <span>{formatDuration(d.durationMs)}</span>
                        <span>{avgSpd(d)}</span>
                      </div>
                      {d.fsdDisengagements > 0 && (
                        <div className="mt-0.5 text-[11px] text-red-400/70">{d.fsdDisengagements} disengagement{d.fsdDisengagements !== 1 ? "s" : ""}</div>
                      )}
                      <div className="mt-1.5 flex flex-wrap items-center gap-1">
                        {(d.tags ?? []).map((t) => (
                          <span key={t} className="group/tag inline-flex items-center rounded-full bg-blue-500/10 px-1.5 py-0.5 text-[10px] font-medium text-blue-400">
                            <Tag className="mr-0.5 h-2 w-2" />{t}
                            <button
                              onClick={(e) => { e.stopPropagation(); removeTagFromDrive(d.id, d.startTime, d.tags ?? [], t) }}
                              className="ml-0.5 hidden rounded-full p-0.5 text-blue-400/60 hover:bg-blue-500/20 hover:text-blue-300 group-hover/tag:inline-flex"
                            ><X className="h-2 w-2" /></button>
                          </span>
                        ))}
                        {listTagInputId === d.id ? (
                          <>
                            {allTags
                              .filter((t) => !(d.tags ?? []).includes(t) && (!listTagValue || t.toLowerCase().includes(listTagValue.toLowerCase())))
                              .map((t) => (
                                <button
                                  key={t}
                                  onMouseDown={(e) => {
                                    e.preventDefault()
                                    e.stopPropagation()
                                    addTagToDrive(d.id, d.startTime, d.tags ?? [], t)
                                    setListTagValue(""); setListTagInputId(null)
                                  }}
                                  onClick={(e) => e.stopPropagation()}
                                  className="inline-flex items-center gap-0.5 rounded-full border border-dashed border-blue-500/20 bg-blue-500/5 px-1.5 py-0.5 text-[10px] font-medium text-blue-400/70 transition-colors hover:border-blue-500/40 hover:bg-blue-500/15 hover:text-blue-400"
                                >
                                  <Plus className="h-2 w-2" />{t}
                                </button>
                              ))}
                            <input
                              autoFocus
                              value={listTagValue}
                              onChange={(e) => setListTagValue(e.target.value)}
                              onClick={(e) => e.stopPropagation()}
                              onKeyDown={(e) => {
                                e.stopPropagation()
                                if (e.key === "Enter" && listTagValue.trim()) {
                                  addTagToDrive(d.id, d.startTime, d.tags ?? [], listTagValue)
                                  setListTagValue(""); setListTagInputId(null)
                                }
                                if (e.key === "Escape") { setListTagInputId(null); setListTagValue("") }
                              }}
                              onBlur={() => { setListTagInputId(null); setListTagValue("") }}
                              placeholder="New..."
                              className="w-16 rounded-full border border-blue-500/30 bg-white/5 px-1.5 py-0.5 text-[10px] text-slate-200 placeholder-slate-600 outline-none focus:border-blue-500/50"
                            />
                          </>
                        ) : (
                          <button
                            onClick={(e) => { e.stopPropagation(); setListTagInputId(d.id); setListTagValue("") }}
                            className="inline-flex items-center gap-0.5 rounded-full border border-dashed border-white/10 px-1.5 py-0.5 text-[10px] text-slate-600 transition-colors hover:border-blue-500/30 hover:text-blue-400"
                          >
                            <Plus className="h-2 w-2" />
                          </button>
                        )}
                      </div>
                    </button>
                  </div>
                )
              })
            })()}
            {visibleCount < filtered.length && <div ref={sentinelRef} className="py-4 text-center text-[10px] text-slate-600">Loading more...</div>}
          </div>
        </div>

        {/* Map */}
        <div className="relative isolate flex-1 overflow-hidden">
          <div ref={mapRef} className="h-full w-full" />

          {/* Map style picker */}
          <div className="absolute right-3 top-3 z-[1000]">
            <div className="relative">
              <button
                onClick={() => setShowLayerPicker(!showLayerPicker)}
                className="flex items-center gap-1.5 rounded-lg border border-white/10 bg-slate-950/90 px-2.5 py-1.5 text-xs font-medium text-slate-300 backdrop-blur-sm transition-colors hover:bg-slate-900"
              >
                <Layers className="h-3.5 w-3.5" />
              </button>
              {showLayerPicker && (
                <div className="absolute right-0 mt-1 w-36 rounded-lg border border-white/10 bg-slate-950/95 py-1 shadow-xl backdrop-blur-sm">
                  {(["dark", "streets", "google", "satellite"] as MapStyle[]).map((s) => (
                    <button
                      key={s}
                      onClick={() => { setMapStyle(s); setShowLayerPicker(false) }}
                      className={cn(
                        "w-full px-3 py-1.5 text-left text-xs transition-colors hover:bg-white/5",
                        mapStyle === s ? "font-semibold text-blue-400" : "text-slate-400"
                      )}
                    >
                      {s === "dark" ? "Dark" : s === "streets" ? "Streets" : s === "google" ? "Google Maps" : "Satellite"}
                    </button>
                  ))}
                </div>
              )}
            </div>
          </div>

          {loading && (
            <div className="absolute inset-0 z-[1000] flex items-center justify-center bg-black/70">
              <p className="text-sm text-slate-400">Loading drives...</p>
            </div>
          )}

          {/* Back button */}
          {selectedId !== null && (
            <button
              onClick={goBack}
              className="absolute left-3 top-3 z-[1000] flex items-center gap-1.5 rounded-lg border border-white/10 bg-slate-950/90 px-3 py-1.5 text-xs font-medium text-slate-300 backdrop-blur-sm transition-colors hover:bg-slate-900"
            >
              <ChevronLeft className="h-3.5 w-3.5" /> All Drives
            </button>
          )}

          {/* Detail panel */}
          {selectedDrive && (
            <div className="absolute inset-x-0 bottom-0 z-[1000] border-t border-white/10 bg-slate-950/90 px-4 py-3 backdrop-blur-md">
              {/* Tags row */}
              <div className="mb-3 flex flex-wrap items-center gap-2 rounded-lg border border-white/5 bg-white/[0.03] px-3 py-2">
                <span className="flex items-center gap-1.5 text-xs font-medium text-slate-400">
                  <Tag className="h-3.5 w-3.5" /> Tags:
                </span>
                {(selectedDrive.tags ?? []).map((t) => (
                  <span key={t} className="inline-flex items-center gap-1 rounded-full bg-blue-500/15 px-2.5 py-1 text-xs font-medium text-blue-400">
                    {t}
                    <button onClick={() => removeTagFromDrive(selectedId!, selectedDrive.startTime, selectedDrive.tags ?? [], t)} className="ml-0.5 rounded-full p-0.5 text-blue-400/60 hover:bg-blue-500/20 hover:text-blue-300"><X className="h-3 w-3" /></button>
                  </span>
                ))}
                {showTagInput ? (
                  <>
                    {allTags
                      .filter((t) => !(selectedDrive.tags ?? []).includes(t) && (!tagInput || t.toLowerCase().includes(tagInput.toLowerCase())))
                      .map((t) => (
                        <button
                          key={t}
                          onMouseDown={(e) => {
                            e.preventDefault()
                            addTagToDrive(selectedId!, selectedDrive.startTime, selectedDrive.tags ?? [], t)
                            setTagInput("")
                            setShowTagInput(false)
                          }}
                          className="inline-flex items-center gap-1 rounded-full border border-dashed border-blue-500/20 bg-blue-500/5 px-2.5 py-1 text-xs font-medium text-blue-400/70 transition-colors hover:border-blue-500/40 hover:bg-blue-500/15 hover:text-blue-400"
                        >
                          <Plus className="h-3 w-3" />{t}
                        </button>
                      ))}
                    <input
                      autoFocus
                      value={tagInput}
                      onChange={(e) => setTagInput(e.target.value)}
                      onKeyDown={(e) => {
                        if (e.key === "Enter" && tagInput.trim()) {
                          addTagToDrive(selectedId!, selectedDrive.startTime, selectedDrive.tags ?? [], tagInput)
                          setTagInput("")
                          setShowTagInput(false)
                        }
                        if (e.key === "Escape") { setShowTagInput(false); setTagInput("") }
                      }}
                      onBlur={() => { setShowTagInput(false); setTagInput("") }}
                      placeholder="New tag..."
                      className="w-28 rounded-full border border-blue-500/30 bg-white/5 px-3 py-1 text-xs text-slate-200 placeholder-slate-500 outline-none focus:border-blue-500/50"
                    />
                  </>
                ) : (
                  <button
                    onClick={() => setShowTagInput(true)}
                    className="inline-flex items-center gap-1 rounded-full border border-dashed border-white/20 bg-white/[0.03] px-3 py-1 text-xs font-medium text-slate-400 transition-colors hover:border-blue-500/40 hover:bg-blue-500/10 hover:text-blue-400"
                  >
                    <Plus className="h-3.5 w-3.5" /> Add Tag
                  </button>
                )}
              </div>
              <div className="mb-2 flex flex-wrap gap-x-4 gap-y-2 sm:gap-x-6">
                <Stat icon={<Navigation className="h-3 w-3" />} label="Distance" value={dist(selectedDrive)} highlight />
                <Stat icon={<Clock className="h-3 w-3" />} label="Duration" value={formatDuration(selectedDrive.durationMs)} />
                <Stat label="Start" value={formatTime(selectedDrive.startTime)} />
                <Stat label="End" value={formatTime(selectedDrive.endTime)} />
                <Stat icon={<Gauge className="h-3 w-3" />} label="Avg" value={avgSpd(selectedDrive)} />
                <Stat label="Max" value={maxSpd(selectedDrive)} highlight />
              </div>

              {/* Assisted driving stats row */}
              {totalAssistedPercent(selectedDrive) > 0 && (
                <div className="mb-2 flex flex-wrap items-center gap-x-5 gap-y-1 rounded-lg border border-emerald-500/10 bg-emerald-500/5 px-3 py-1.5">
                  {selectedDrive.fsdPercent > 0 && (
                    <div className="flex items-center gap-1.5 text-[11px]">
                      <span className="font-bold text-emerald-400">{selectedDrive.fsdPercent}%</span>
                      <span className="text-slate-500">FSD</span>
                    </div>
                  )}
                  {selectedDrive.autosteerPercent > 0 && (
                    <div className="flex items-center gap-1.5 text-[11px]">
                      <span className="font-bold text-emerald-400">{selectedDrive.autosteerPercent}%</span>
                      <span className="text-slate-500">Autopilot</span>
                    </div>
                  )}
                  {selectedDrive.taccPercent > 0 && (
                    <div className="flex items-center gap-1.5 text-[11px]">
                      <span className="font-bold text-emerald-400">{selectedDrive.taccPercent}%</span>
                      <span className="text-slate-500">TACC</span>
                    </div>
                  )}
                  {selectedDrive.fsdDisengagements > 0 && (
                    <div className="flex items-center gap-1.5 text-[11px]">
                      <span className="font-bold text-red-400">{selectedDrive.fsdDisengagements}</span>
                      <span className="text-slate-500">Disengagement{selectedDrive.fsdDisengagements !== 1 ? "s" : ""}</span>
                    </div>
                  )}
                  {selectedDrive.fsdAccelPushes > 0 && (
                    <div className="flex items-center gap-1.5 text-[11px]">
                      <span className="font-bold text-amber-400">{selectedDrive.fsdAccelPushes}</span>
                      <span className="text-slate-500">Accel Push{selectedDrive.fsdAccelPushes !== 1 ? "es" : ""}</span>
                    </div>
                  )}
                  {(selectedDrive.fsdDistanceKm > 0 || selectedDrive.autosteerDistanceKm > 0 || selectedDrive.taccDistanceKm > 0) && (
                    <div className="flex items-center gap-1.5 text-[11px]">
                      <span className="text-slate-500">Assisted:</span>
                      <span className="font-semibold text-emerald-400">{metric
                        ? `${(selectedDrive.fsdDistanceKm + selectedDrive.autosteerDistanceKm + selectedDrive.taccDistanceKm).toFixed(2)} km`
                        : `${(selectedDrive.fsdDistanceMi + selectedDrive.autosteerDistanceMi + selectedDrive.taccDistanceMi).toFixed(2)} mi`
                      }</span>
                    </div>
                  )}
                  <div className="ml-auto flex items-center gap-2 text-[10px] text-slate-600">
                    <button
                      onClick={() => setShowFSDMarkers(!showFSDMarkers)}
                      className={cn(
                        "flex items-center gap-1 rounded-full px-2 py-0.5 transition-colors",
                        showFSDMarkers ? "bg-white/5 text-slate-400 hover:bg-white/10" : "bg-white/5 text-slate-600 hover:bg-white/10"
                      )}
                      title={showFSDMarkers ? "Hide event markers" : "Show event markers"}
                    >
                      {showFSDMarkers ? <Eye className="h-3 w-3" /> : <EyeOff className="h-3 w-3" />}
                      Markers
                    </button>
                    <span className="flex items-center gap-1"><span className="inline-block h-1.5 w-3 rounded-full bg-emerald-500" /> Assisted</span>
                    <span className="flex items-center gap-1"><span className="inline-block h-1.5 w-3 rounded-full bg-blue-500" /> Manual</span>
                  </div>
                </div>
              )}

              {/* Slider */}
              <div className="flex items-center gap-3">
                <button
                  onClick={toggleSliderPlay}
                  className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full bg-blue-500/20 text-blue-400 transition-colors hover:bg-blue-500/30"
                  title={sliderPlaying ? "Pause playback" : "Play drive route"}
                >
                  {sliderPlaying ? <Pause className="h-3.5 w-3.5" /> : <Play className="h-3.5 w-3.5 translate-x-px" />}
                </button>
                <span className="min-w-[52px] text-[10px] tabular-nums text-slate-500">
                  {selectedDrive.points.length > 0 ? formatTimeMs(selectedDrive.points[0][2]) : "--"}
                </span>
                <input
                  type="range"
                  min={0}
                  max={selectedDrive.points.length - 1}
                  value={sliderIdx}
                  onChange={(e) => { if (sliderPlaying) toggleSliderPlay(); handleSlider(parseInt(e.target.value)) }}
                  className="h-1 flex-1 cursor-pointer appearance-none rounded-full bg-slate-800 accent-blue-500 [&::-webkit-slider-thumb]:h-3.5 [&::-webkit-slider-thumb]:w-3.5 [&::-webkit-slider-thumb]:appearance-none [&::-webkit-slider-thumb]:rounded-full [&::-webkit-slider-thumb]:bg-blue-500 [&::-webkit-slider-thumb]:shadow-[0_0_6px_rgba(59,130,246,0.5)]"
                />
                <span className="min-w-[52px] text-right text-[10px] tabular-nums text-slate-500">
                  {selectedDrive.points.length > 0 ? formatTimeMs(selectedDrive.points[selectedDrive.points.length - 1][2]) : "--"}
                </span>
              </div>

              {sliderPt && (
                <div className="mt-1.5 flex flex-wrap justify-center gap-x-4 gap-y-1 text-[11px] text-slate-500 sm:gap-5">
                  <span>Time: <span className="font-semibold text-blue-400">{formatTimeMs(sliderPt[2])}</span></span>
                  <span>Speed: <span className="font-semibold text-blue-400">{mpsToDisplay(sliderPt[3])} {speedUnit}</span></span>
                  {selectedDrive.gearStates && selectedDrive.gearStates[sliderIdx] !== undefined && (
                    <span>
                      Gear: <span className={`font-semibold ${(GEAR_LABELS[selectedDrive.gearStates[sliderIdx]] || GEAR_LABELS[0]).color}`}>
                        {(GEAR_LABELS[selectedDrive.gearStates[sliderIdx]] || GEAR_LABELS[0]).text}
                      </span>
                    </span>
                  )}
                  <span>Dist: <span className="font-semibold text-blue-400">{sliderDistDisplay} {distUnit}</span></span>
                  <span>Pt: <span className="font-semibold text-blue-400">{sliderIdx + 1}/{selectedDrive.points.length}</span></span>
                  {selectedDrive.fsdStates && selectedDrive.fsdStates[sliderIdx] !== undefined && (
                    <span>
                      {selectedDrive.fsdStates[sliderIdx] > 0
                        ? <span className="font-semibold text-emerald-400">FSD</span>
                        : <span className="font-semibold text-slate-400">Manual</span>}
                    </span>
                  )}
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  )
}

function Stat({ icon, label, value, highlight }: { icon?: React.ReactNode; label: string; value: string; highlight?: boolean }) {
  return (
    <div className="flex items-center gap-1.5">
      {icon && <span className="text-slate-600">{icon}</span>}
      <span className="text-[10px] uppercase tracking-wider text-slate-600">{label}</span>
      <span className={cn("text-xs font-semibold", highlight ? "text-blue-400" : "text-slate-300")}>{value}</span>
    </div>
  )
}
