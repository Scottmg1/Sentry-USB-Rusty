import { useEffect, useRef, useState } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"
import { LayersIcon } from "@/components/icons"
import { useScrubberState } from "@/hooks/useScrubberSync"
import type { FsdEvent } from "@/types/drives"

interface DriveMapProps {
  points: [number, number, number, number][]
  fsdStates?: number[]
  fsdEvents?: FsdEvent[]
  showEvents?: boolean
  source?: string
  // Drive start converts relative point offsets to wall-clock time.
  startTime?: string
  // Selects the playback card's speed unit.
  metric?: boolean
  // Battery samples use latest-at-or-before lookup rather than interpolation.
  batterySeries?: BatteryPoint[]
}

export interface BatteryPoint {
  ts: number      // unix ms
  batteryPct?: number
}

const TILES = {
  dark: "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png",
  streets: "https://{s}.tile.openstreetmap.org/{z}/{x}/{y}.png",
  satellite:
    "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}",
} as const

// Satellite imagery needs a transparent labels overlay.
const SATELLITE_LABELS_URL =
  "https://{s}.basemaps.cartocdn.com/rastertiles/voyager_only_labels/{z}/{x}/{y}{r}.png"

type Style = keyof typeof TILES

// FSD and manual segments use distinct colors; imported routes use violet.
const COLOR_FSD = "#34d399"
const COLOR_MANUAL = "#3b82f6"
const COLOR_TESSIE = "#a78bfa"

function startMarkerIcon() {
  return L.divIcon({
    className: "",
    html: '<div style="width:12px;height:12px;border-radius:50%;background:#34d399;border:2px solid #fff;box-shadow:0 0 4px rgba(0,0,0,0.4)"></div>',
    iconSize: [12, 12],
    iconAnchor: [6, 6],
  })
}

function endMarkerIcon() {
  return L.divIcon({
    className: "",
    html: '<div style="width:12px;height:12px;border-radius:50%;background:#ef4444;border:2px solid #fff;box-shadow:0 0 4px rgba(0,0,0,0.4)"></div>',
    iconSize: [12, 12],
    iconAnchor: [6, 6],
  })
}

// Rotate the inner marker so Leaflet can retain translation on the outer node.
function pulseMarkerIcon(bearingDeg: number) {
  return L.divIcon({
    className: "drive-pulse-marker",
    html:
      `<div class="drive-pulse" style="transform:rotate(${bearingDeg}deg)">` +
      `<img src="/arrow.png" alt="" width="18" height="18" draggable="false"/>` +
      `</div>`,
    iconSize: [18, 18],
    iconAnchor: [9, 9],
  })
}

// Great-circle bearing in degrees clockwise from north.
function bearingBetween(
  lat1: number,
  lng1: number,
  lat2: number,
  lng2: number,
): number {
  const toRad = Math.PI / 180
  const φ1 = lat1 * toRad
  const φ2 = lat2 * toRad
  const Δλ = (lng2 - lng1) * toRad
  const y = Math.sin(Δλ) * Math.cos(φ2)
  const x =
    Math.cos(φ1) * Math.sin(φ2) -
    Math.sin(φ1) * Math.cos(φ2) * Math.cos(Δλ)
  return ((Math.atan2(y, x) * 180) / Math.PI + 360) % 360
}

// Use a three-sample baseline to damp low-speed GPS jitter.
function headingAt(
  points: [number, number, number, number][],
  i: number,
): number {
  if (points.length < 2) return 0
  const LOOKBACK = 3
  let from = i - LOOKBACK
  let to = i
  if (from < 0) {
    // Near the start, derive heading from a forward sample.
    from = i
    to = Math.min(points.length - 1, i + LOOKBACK)
    if (from === to) return 0
  }
  return bearingBetween(
    points[from][0],
    points[from][1],
    points[to][0],
    points[to][1],
  )
}

function fsdEventIcon(kind: "disengagement" | "accel_push") {
  const isDisengage = kind === "disengagement"
  const color = isDisengage ? "#ef4444" : "#f59e0b"
  const label = isDisengage ? "D" : "A"
  return L.divIcon({
    className: "",
    html: `<div style="width:18px;height:18px;border-radius:50%;background:${color};border:2px solid #fff;display:flex;align-items:center;justify-content:center;font-size:10px;font-weight:700;color:#fff;line-height:1;box-shadow:0 0 4px rgba(0,0,0,0.5)">${label}</div>`,
    iconSize: [18, 18],
    iconAnchor: [9, 9],
  })
}

// The wrapper styles the static steering-wheel asset by engagement state.
const WHEEL_HTML =
  '<img src="/autosteer-icon.png" alt="" width="18" height="18" draggable="false" class="playback-info__wheel-img"/>'

const MPS_TO_MPH = 2.23694
const MPS_TO_KPH = 3.6

// Inline battery fill follows the current percentage and CSS color.
function batterySVG(pct: number): string {
  // Clamp the 15-unit fill span to valid percentages.
  const fillW = Math.max(0, Math.min(15, (pct / 100) * 15))
  return (
    '<svg viewBox="0 0 22 12" width="18" height="10" fill="none" stroke="currentColor" stroke-width="1.2">' +
    '<rect x="1" y="1" width="18" height="10" rx="2"/>' +
    `<rect x="3" y="3" width="${fillW.toFixed(2)}" height="6" rx="1" fill="currentColor" stroke="none"/>` +
    '<rect x="20" y="4" width="2" height="4" rx="0.5" fill="currentColor" stroke="none"/>' +
    "</svg>"
  )
}

// Leaflet updates this HTML directly on each scrubber tick.
function renderPlaybackHTML(
  pt: [number, number, number, number] | undefined,
  fsd: number | undefined,
  baseMs: number,
  metric: boolean,
  battery: number | undefined,
): string {
  if (!pt) return ""
  const speedMps = pt[3] ?? 0
  const speed = Math.round(metric ? speedMps * MPS_TO_KPH : speedMps * MPS_TO_MPH)
  const unit = metric ? "km/h" : "mph"
  const time = Number.isFinite(baseMs)
    ? new Date(baseMs + pt[2]).toLocaleTimeString([], {
        hour: "numeric",
        minute: "2-digit",
        second: "2-digit",
      })
    : ""
  const isFsd = (fsd ?? 0) > 0
  const wheelClass = isFsd
    ? "playback-info__wheel playback-info__wheel--fsd"
    : "playback-info__wheel"
  const wheelTitle = isFsd ? "FSD engaged" : "Manual driving"
  const batteryHtml =
    battery !== undefined && Number.isFinite(battery)
      ? `<span class="playback-info__battery" aria-label="Battery ${Math.round(battery)}%">` +
        `${batterySVG(battery)}${Math.round(battery)}%</span>`
      : ""
  return (
    `<div class="playback-info__time">${time}</div>` +
    `<div class="playback-info__row">` +
    `<span class="playback-info__speed">${speed} ${unit}</span>` +
    `<span class="${wheelClass}" title="${wheelTitle}" aria-label="${wheelTitle}">${WHEEL_HTML}</span>` +
    `</div>` +
    (batteryHtml
      ? `<div class="playback-info__row playback-info__row--battery">${batteryHtml}</div>`
      : "")
  )
}

// Use the latest sample at or before the target; interpolation would invent
// battery values. Before the first timestamp, use the earliest sample.
function lookupBatteryAt(
  series: BatteryPoint[] | undefined,
  currentMs: number,
): number | undefined {
  if (!series || series.length === 0) return undefined
  // Series is ascending and small enough for a reverse scan.
  for (let i = series.length - 1; i >= 0; i--) {
    if (series[i].ts <= currentMs) return series[i].batteryPct
  }
  // The first reading is representative before its timestamp.
  return series[0].batteryPct
}

export function DriveMap({
  points,
  fsdStates,
  fsdEvents,
  showEvents = true,
  source,
  startTime,
  metric = false,
  batterySeries,
}: DriveMapProps) {
  const containerRef = useRef<HTMLDivElement>(null)
  const mapRef = useRef<L.Map | null>(null)
  const tileRef = useRef<L.TileLayer | null>(null)
  // Only satellite mode needs the transparent labels layer.
  const labelsRef = useRef<L.TileLayer | null>(null)
  const pulseRef = useRef<L.Marker | null>(null)
  const eventsLayerRef = useRef<L.LayerGroup | null>(null)
  const [style, setStyle] = useState<Style>("dark")
  const { currentIndex } = useScrubberState()

  useEffect(() => {
    const el = containerRef.current
    if (!el || mapRef.current || points.length === 0) return

    // Canvas handles large routes; the moving pulse remains a DOM marker.
    const map = L.map(el, {
      attributionControl: false,
      zoomControl: true,
      preferCanvas: true,
    })
    mapRef.current = map
    tileRef.current = L.tileLayer(TILES.dark, { maxZoom: 19 }).addTo(map)

    const latLngs = points.map(([lat, lng]) => L.latLng(lat, lng))

    // Segment index-aligned FSD states and overlap transition points. Break
    // jumps over GAP_M so GPS dropouts do not draw across the map.
    const GAP_M = 300

    const hasFsdSegments =
      fsdStates !== undefined && fsdStates.length === points.length
    if (hasFsdSegments) {
      let segStart = 0
      for (let i = 1; i <= points.length; i++) {
        const prevEngaged = fsdStates[i - 1] > 0
        const curEngaged = i < points.length ? fsdStates[i] > 0 : !prevEngaged
        const gap =
          i < points.length && latLngs[i - 1].distanceTo(latLngs[i]) > GAP_M
        if (curEngaged !== prevEngaged || gap || i === points.length) {
          const segPts = latLngs.slice(segStart, i)
          if (segPts.length >= 2) {
            L.polyline(segPts, {
              color: prevEngaged ? COLOR_FSD : COLOR_MANUAL,
              weight: 4,
              opacity: 1,
              smoothFactor: 1.2,
            }).addTo(map)
          }
          // Gaps restart at the current point; state changes share a boundary.
          segStart = gap ? i : Math.max(i - 1, 0)
        }
      }
    } else {
      const stroke = source === "tessie" ? COLOR_TESSIE : COLOR_FSD
      let segStart = 0
      for (let i = 1; i <= latLngs.length; i++) {
        const gap =
          i < latLngs.length && latLngs[i - 1].distanceTo(latLngs[i]) > GAP_M
        if (gap || i === latLngs.length) {
          const segPts = latLngs.slice(segStart, i)
          if (segPts.length >= 2) {
            L.polyline(segPts, {
              color: stroke,
              weight: 4,
              opacity: 1,
              smoothFactor: 1.2,
            }).addTo(map)
          }
          segStart = i
        }
      }
    }

    // DOM markers move independently from canvas-rendered routes.
    L.marker(latLngs[0], { icon: startMarkerIcon(), interactive: false }).addTo(map)
    L.marker(latLngs[latLngs.length - 1], { icon: endMarkerIcon(), interactive: false }).addTo(map)
    // Leaflet requires an interactive marker for openTooltip; exclude it from tab order.
    pulseRef.current = L.marker(latLngs[0], {
      icon: pulseMarkerIcon(headingAt(points, 0)),
      interactive: true,
      keyboard: false,
      zIndexOffset: 1000,
    }).addTo(map)

    // Bind once and update tooltip content from the playback effect.
    const baseMs = startTime ? new Date(startTime).getTime() : NaN
    const firstPointMs = Number.isFinite(baseMs) ? baseMs + points[0][2] : NaN
    const initialBattery = lookupBatteryAt(batterySeries, firstPointMs)
    const initialHtml = renderPlaybackHTML(
      points[0],
      fsdStates?.[0],
      baseMs,
      metric,
      initialBattery,
    )
    pulseRef.current
      .bindTooltip(initialHtml, {
        permanent: true,
        direction: "right",
        offset: [10, 0],
        className: "playback-info-tooltip",
        opacity: 1,
      })
      .openTooltip()

    eventsLayerRef.current = L.layerGroup().addTo(map)
    map.fitBounds(L.latLngBounds(latLngs), { padding: [24, 24], maxZoom: 16 })

    return () => {
      map.remove()
      mapRef.current = null
      tileRef.current = null
      labelsRef.current = null
      pulseRef.current = null
      eventsLayerRef.current = null
    }
    // Tooltip inputs update separately so they do not rebuild the Leaflet map.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [points, fsdStates, source])

  useEffect(() => {
    const map = mapRef.current
    if (!map || !tileRef.current) return
    map.removeLayer(tileRef.current)
    tileRef.current = L.tileLayer(TILES[style], { maxZoom: 19 }).addTo(map)
    // shadowPane keeps satellite labels below routes and markers.
    if (labelsRef.current) {
      map.removeLayer(labelsRef.current)
      labelsRef.current = null
    }
    if (style === "satellite") {
      labelsRef.current = L.tileLayer(SATELLITE_LABELS_URL, {
        maxZoom: 19,
        pane: "shadowPane",
      }).addTo(map)
    }
  }, [style])

  useEffect(() => {
    const layer = eventsLayerRef.current
    if (!layer) return
    layer.clearLayers()
    if (!showEvents || !fsdEvents) return
    for (const ev of fsdEvents) {
      const title = ev.type === "disengagement" ? "FSD Disengagement" : "Accel Push"
      L.marker([ev.lat, ev.lng], {
        icon: fsdEventIcon(ev.type),
        title,
        riseOnHover: true,
      })
        .bindTooltip(title, { direction: "top", offset: [0, -10] })
        .addTo(layer)
    }
  }, [showEvents, fsdEvents])

  useEffect(() => {
    const pulse = pulseRef.current
    if (!pulse || points.length === 0) return
    const i = Math.min(points.length - 1, Math.max(0, currentIndex))
    pulse.setLatLng(L.latLng(points[i][0], points[i][1]))
    // Rotate the inner node directly to avoid rebuilding the marker.
    const el = pulse.getElement()
    if (el) {
      const arrow = el.querySelector(".drive-pulse") as HTMLElement | null
      if (arrow) {
        const bearing = headingAt(points, i)
        arrow.style.transform = `rotate(${bearing}deg)`
      }
    }
    // setContent updates the attached tooltip without a React render.
    const tooltip = pulse.getTooltip()
    if (tooltip) {
      const baseMs = startTime ? new Date(startTime).getTime() : NaN
      const pointMs = Number.isFinite(baseMs) ? baseMs + points[i][2] : NaN
      const battery = lookupBatteryAt(batterySeries, pointMs)
      tooltip.setContent(
        renderPlaybackHTML(
          points[i],
          fsdStates?.[i],
          baseMs,
          metric,
          battery,
        ),
      )
    }
  }, [currentIndex, points, fsdStates, startTime, metric, batterySeries])

  const cycleStyle = () => {
    setStyle((s) => (s === "dark" ? "streets" : s === "streets" ? "satellite" : "dark"))
  }

  return (
    <div className="relative h-80 w-full overflow-hidden rounded-2xl ring-1 ring-inset ring-white/10 sm:h-96">
      <div ref={containerRef} className="absolute inset-0 bg-slate-900" />
      <div className="absolute right-2 top-2 z-[400] flex flex-col gap-1">
        <ControlBtn label={`Map style: ${style}`} onClick={cycleStyle}>
          <LayersIcon className="h-4 w-4" />
        </ControlBtn>
      </div>
    </div>
  )
}

function ControlBtn({
  label,
  onClick,
  children,
}: {
  label: string
  onClick: () => void
  children: React.ReactNode
}) {
  return (
    <button
      type="button"
      title={label}
      aria-label={label}
      onClick={onClick}
      className="flex h-8 w-8 items-center justify-center rounded-md border border-white/10 bg-slate-900/85 text-slate-300 backdrop-blur hover:bg-slate-800 hover:text-slate-100"
    >
      {children}
    </button>
  )
}
