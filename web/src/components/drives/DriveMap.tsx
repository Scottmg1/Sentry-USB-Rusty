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
  // ISO start_time of the drive — needed to convert each point's
  // relative-ms field into an absolute clock time for the playback
  // info card. Without it the card would show offsets, not times.
  startTime?: string
  // True when the user's DRIVE_MAP_UNIT === "km". Controls the speed
  // unit displayed in the playback card.
  metric?: boolean
  // Per-sample battery time-series from
  // GET /api/drives/{id}/battery-series. The BLE telemetry sampler
  // polls every 60s in Active mode, so a 30-min drive has ~30 samples.
  // For each scrubber tick we look up the most recent sample at or
  // before the current point's wall-clock time — battery changes in
  // discrete steps, not smoothly, so step-lookup beats interpolation.
  // Omit the prop (or pass an empty array) to skip the battery row.
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

// CartoDB "labels only" overlay — transparent tiles with street + place
// names, drawn on top of the satellite base so the user can actually
// read the map. Dark/streets already include labels in their base tile.
const SATELLITE_LABELS_URL =
  "https://{s}.basemaps.cartocdn.com/rastertiles/voyager_only_labels/{z}/{x}/{y}{r}.png"

type Style = keyof typeof TILES

// Colors for the route polyline segments.
// FSD-engaged: emerald accent. Manual: indigo-blue (echo of the old design
// for instant familiarity). When fsdStates is unavailable, the route falls
// back to a single emerald polyline OR a violet polyline for Tessie-source
// drives (matches the existing source badge convention).
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

// Pulse marker — the bot that tracks the scrubber position along
// the route. Replaced the old green dot with a directional arrow that
// rotates to point along the current heading, matching how GPS apps
// indicate "you are here, going that way". The inner `.drive-pulse`
// wrapper carries the rotation transform; Leaflet's own translate
// transform lives on the outer marker element so the two don't fight.
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

// Bearing in degrees-clockwise-from-north between two GPS points.
// Standard great-circle formula; for adjacent samples a few seconds
// apart the lat/lng delta is tiny so this is fine without correction.
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

// Heading for the arrow at scrubber index `i`. Looks back ~3 samples
// for stability against GPS jitter at low speeds (a single-sample
// delta produces a twitchy arrow). For the first few samples we look
// forward instead so the very first frame still has a sensible
// direction. Returns 0 when there's no meaningful delta available.
function headingAt(
  points: [number, number, number, number][],
  i: number,
): number {
  if (points.length < 2) return 0
  const LOOKBACK = 3
  let from = i - LOOKBACK
  let to = i
  if (from < 0) {
    // Near start of drive — look forward instead so the arrow still
    // orients meaningfully on frame 0.
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

// Steering-wheel glyph in the playback info card. Uses the project
// asset (blue circular wheel) served from web/public. The parent
// `.playback-info__wheel` span carries the FSD/manual visual state
// via CSS (full opacity for engaged, dim+grey for manual) — the
// asset itself is a flat coloured PNG.
const WHEEL_HTML =
  '<img src="/autosteer-icon.png" alt="" width="18" height="18" draggable="false" class="playback-info__wheel-img"/>'

const MPS_TO_MPH = 2.23694
const MPS_TO_KPH = 3.6

// Inline-SVG battery icon. Outer rectangle with a small terminal nub
// on the right; the inner fill rect's width is set inline at call
// time from the current battery percentage so the visual gauge tracks
// the playback. `fill="currentColor"` so the CSS pill colour controls
// the whole glyph.
function batterySVG(pct: number): string {
  // Inner fill spans x=3..x=18 (15 units wide max) and is proportional
  // to pct. clamp so out-of-range readings can't blow past the bounds.
  const fillW = Math.max(0, Math.min(15, (pct / 100) * 15))
  return (
    '<svg viewBox="0 0 22 12" width="18" height="10" fill="none" stroke="currentColor" stroke-width="1.2">' +
    '<rect x="1" y="1" width="18" height="10" rx="2"/>' +
    `<rect x="3" y="3" width="${fillW.toFixed(2)}" height="6" rx="1" fill="currentColor" stroke="none"/>` +
    '<rect x="20" y="4" width="2" height="4" rx="0.5" fill="currentColor" stroke="none"/>' +
    "</svg>"
  )
}

// Build the HTML body of the playback tooltip for the given scrubber
// index. Returns a string so Leaflet's `tooltip.setContent` can swap
// it on every tick without paying React-reconciliation cost.
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
  // Layout: time on top, then speed + wheel row, then battery on its
  // own row below — matches the GPS-app convention the user requested
  // (battery is reference info, not part of the primary speed/state).
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

// Step-lookup of the most recent battery sample at or before
// `currentMs`. Battery changes in 1% steps and the sampler runs
// at 60s cadence — linear interpolation would invent values
// between samples, so we use the latest sample as the canonical
// reading at the scrubber's wall-clock time. Falls back to the
// first sample when the scrubber is before any sample (so the card
// always shows something once the series has loaded). Returns
// undefined when the series is empty or the value is NULL.
function lookupBatteryAt(
  series: BatteryPoint[] | undefined,
  currentMs: number,
): number | undefined {
  if (!series || series.length === 0) return undefined
  // Series is ASC-ordered by ts (the backend's ORDER BY ts ASC).
  // Walk backwards to find the latest sample with ts <= currentMs.
  // A binary search would be theoretically nicer; for ~30 samples
  // on a typical drive the linear scan is cheaper than the branch.
  for (let i = series.length - 1; i >= 0; i--) {
    if (series[i].ts <= currentMs) return series[i].batteryPct
  }
  // currentMs is before every sample (e.g. scrubber at frame 0,
  // first sample arrived a few seconds in). Show the earliest
  // reading rather than nothing — battery doesn't jump in 60s.
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
  // Transparent labels layer drawn on top of satellite imagery so place
  // and street names remain readable. Only attached in the "satellite"
  // style; the dark/streets base tiles already include labels.
  const labelsRef = useRef<L.TileLayer | null>(null)
  const pulseRef = useRef<L.Marker | null>(null)
  const eventsLayerRef = useRef<L.LayerGroup | null>(null)
  const [style, setStyle] = useState<Style>("dark")
  const { currentIndex } = useScrubberState()

  useEffect(() => {
    const el = containerRef.current
    if (!el || mapRef.current || points.length === 0) return

    // preferCanvas keeps the polyline(s) on a single 2D canvas, which
    // re-projects much faster than the default SVG renderer on zoom for
    // routes with thousands of points. The pulse marker stays as a DOM
    // divIcon so its scrubber-driven setLatLng() moves via CSS transform
    // without triggering a canvas redraw.
    const map = L.map(el, {
      attributionControl: false,
      zoomControl: true,
      preferCanvas: true,
    })
    mapRef.current = map
    tileRef.current = L.tileLayer(TILES.dark, { maxZoom: 19 }).addTo(map)

    const latLngs = points.map(([lat, lng]) => L.latLng(lat, lng))

    // Segment the polyline by FSD state when fsdStates is parallel to
    // points. Each contiguous run of the same engagement state becomes
    // one polyline; adjacent segments overlap by one point so there's
    // no visible gap at the transition. Falls back to a single-color
    // polyline (emerald for SEI, violet for Tessie-source) when
    // fsdStates is missing or length-mismatched.
    // Break the route wherever consecutive points jump more than GAP_M apart.
    // Happens on a genuine GPS dropout (tunnel/signal loss) and at the seam left
    // when an overlapping/duplicate RecentClips segment is dropped upstream —
    // without the break the polyline slashes a straight line across the map
    // between the two. (The data layer's monotonic-time filter removes the bulk
    // of any overlap; this catches the residual seam.)
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
          // Gap → start fresh at i (don't bridge). State change → share the
          // boundary point so the colors meet seamlessly.
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

    // Start / end / pulse all use DOM markers (divIcon) so the pulse
    // marker can move on every scrubber tick without redrawing the
    // canvas-rendered polylines.
    L.marker(latLngs[0], { icon: startMarkerIcon(), interactive: false }).addTo(map)
    L.marker(latLngs[latLngs.length - 1], { icon: endMarkerIcon(), interactive: false }).addTo(map)
    // The pulse marker MUST be interactive so the bound tooltip can
    // open via openTooltip() — Leaflet refuses to open tooltips on
    // non-interactive markers. We still set keyboard:false so it
    // doesn't capture tab focus.
    pulseRef.current = L.marker(latLngs[0], {
      icon: pulseMarkerIcon(headingAt(points, 0)),
      interactive: true,
      keyboard: false,
      zIndexOffset: 1000,
    }).addTo(map)

    // Playback info card — permanent tooltip floating next to the
    // pulse marker. Bound once here, content swapped on each scrubber
    // tick by the dedicated effect below. Position "right" so the
    // card sits beside the pulse rather than covering it; Leaflet
    // auto-flips when it would go off the map edge.
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
    // batterySeries/metric/startTime only seed the initial tooltip; adding
    // them would tear down and recreate the whole Leaflet map. Live tooltip
    // updates are handled by the playback effect below.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [points, fsdStates, source])

  useEffect(() => {
    const map = mapRef.current
    if (!map || !tileRef.current) return
    map.removeLayer(tileRef.current)
    tileRef.current = L.tileLayer(TILES[style], { maxZoom: 19 }).addTo(map)
    // Manage the satellite labels overlay: attach on switch-to-satellite,
    // remove on switch-away. Pane "shadowPane" keeps it above the
    // base tiles but below polylines and markers.
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
    // Rotate the arrow to match current heading. Direct DOM mutation
    // of the inner div avoids setIcon's full element rebuild on every
    // scrubber tick — Leaflet's own translate transform stays on the
    // outer marker element, our rotate sits on the inner wrapper.
    const el = pulse.getElement()
    if (el) {
      const arrow = el.querySelector(".drive-pulse") as HTMLElement | null
      if (arrow) {
        const bearing = headingAt(points, i)
        arrow.style.transform = `rotate(${bearing}deg)`
      }
    }
    // Refresh the playback info card to match the new position.
    // setContent on an attached Leaflet tooltip patches its innerHTML
    // in place — no React render, no marker rebind, no map redraw.
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
