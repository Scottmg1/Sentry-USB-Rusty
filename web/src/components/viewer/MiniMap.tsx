import { useEffect, useRef, useState, memo } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"
import { MapPin, Minimize2, Maximize2 } from "lucide-react"
import type { ClipTelemetry, TelemetryFrame } from "@/lib/api"
import { useDraggable } from "@/hooks/useDraggable"

interface MiniMapProps {
  telemetry: ClipTelemetry
  currentFrame: TelemetryFrame | null
}

export default memo(function MiniMap({ telemetry, currentFrame }: MiniMapProps) {
  const { ref: dragRef, dragProps } = useDraggable({ initialAnchor: "top-right" })
  const mapRef = useRef<HTMLDivElement>(null)
  const mapInstance = useRef<L.Map | null>(null)
  const markerRef = useRef<L.CircleMarker | null>(null)
  const lastMapUpdateRef = useRef(0)
  const [collapsed, setCollapsed] = useState(false)

  // Build GPS path from frames
  useEffect(() => {
    if (!mapRef.current || mapInstance.current) return

    const map = L.map(mapRef.current, {
      zoomControl: false,
      attributionControl: false,
      dragging: true,
      scrollWheelZoom: false,
    }).setView([0, 0], 15)

    L.tileLayer("https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png", {
      subdomains: "abcd",
      maxZoom: 20,
    }).addTo(map)

    mapInstance.current = map

    // Draw route polyline
    const gpsFrames = telemetry.frames.filter((f) => f.lat !== 0 || f.lng !== 0)
    if (gpsFrames.length < 2) return

    // Split into FSD and manual segments for color coding
    let currentSegment: L.LatLng[] = []
    let currentIsFSD = gpsFrames[0].autopilot > 0

    const addSegment = (pts: L.LatLng[], isFSD: boolean) => {
      if (pts.length < 2) return
      L.polyline(pts, {
        color: isFSD ? "#34d399" : "#60a5fa",
        weight: 3,
        opacity: 0.8,
      }).addTo(map)
    }

    for (const frame of gpsFrames) {
      const isFSD = frame.autopilot > 0
      const pt = L.latLng(frame.lat, frame.lng)

      if (isFSD !== currentIsFSD) {
        // Add overlap point for continuity
        currentSegment.push(pt)
        addSegment(currentSegment, currentIsFSD)
        currentSegment = [pt]
        currentIsFSD = isFSD
      } else {
        currentSegment.push(pt)
      }
    }
    addSegment(currentSegment, currentIsFSD)

    // Fit bounds
    const bounds = L.latLngBounds(gpsFrames.map((f) => [f.lat, f.lng] as [number, number]))
    map.fitBounds(bounds, { padding: [20, 20] })

    // Add position marker
    markerRef.current = L.circleMarker([gpsFrames[0].lat, gpsFrames[0].lng], {
      radius: 6,
      fillColor: "#fff",
      fillOpacity: 1,
      color: "#3b82f6",
      weight: 3,
    }).addTo(map)

    return () => {
      map.remove()
      mapInstance.current = null
      markerRef.current = null
    }
  }, [telemetry])

  // Update marker position — throttled to ~10fps
  useEffect(() => {
    if (!markerRef.current || !currentFrame) return
    if (currentFrame.lat === 0 && currentFrame.lng === 0) return
    const now = performance.now()
    if (now - lastMapUpdateRef.current < 100) return
    lastMapUpdateRef.current = now
    markerRef.current.setLatLng([currentFrame.lat, currentFrame.lng])
  }, [currentFrame])

  if (!telemetry.has_gps) return null

  return (
    <div
      ref={dragRef}
      {...dragProps}
      className="z-20 overflow-hidden rounded-lg border border-white/10 bg-black/40 shadow-xl backdrop-blur-sm select-none"
    >
      {/* Header — drag handle */}
      <div className="flex items-center justify-between bg-black/40 px-2 py-1 cursor-grab">
        <div className="flex items-center gap-1">
          <MapPin className="h-3 w-3 text-blue-400" />
          <span className="text-[10px] font-medium text-slate-300">Map</span>
          {telemetry.has_autopilot && (
            <span className="ml-1 flex items-center gap-0.5 text-[9px] text-slate-500">
              <span className="inline-block h-1.5 w-1.5 rounded-full bg-emerald-400" /> Assisted
              <span className="inline-block h-1.5 w-1.5 rounded-full bg-blue-400" /> Manual
            </span>
          )}
        </div>
        <button
          onClick={() => setCollapsed(!collapsed)}
          className="rounded p-0.5 text-slate-500 hover:text-slate-300"
        >
          {collapsed ? <Maximize2 className="h-3 w-3" /> : <Minimize2 className="h-3 w-3" />}
        </button>
      </div>
      {!collapsed && (
        <div
          ref={mapRef}
          className="h-36 w-52"
          style={{ minHeight: 144 }}
        />
      )}
    </div>
  )
})
