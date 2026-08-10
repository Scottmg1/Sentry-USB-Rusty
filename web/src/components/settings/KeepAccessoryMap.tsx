import { useEffect, useRef } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"
import { normalizeLon } from "@/lib/geo"

// `dark_all` (not `dark_nolabels`) so street names, place names, and
// landmarks render — without them there's no way to judge whether the
// pin actually sits on the right spot.
const TILES = "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png"

// Simple CSS pin (a divIcon) — avoids Leaflet's default-marker image URLs,
// which break under bundlers. Draggable.
const HOME_ICON = L.divIcon({
  className: "",
  html: '<div style="width:16px;height:16px;border-radius:50%;background:#3b82f6;border:2px solid #fff;box-shadow:0 0 0 3px rgba(59,130,246,.35)"></div>',
  iconSize: [16, 16],
  iconAnchor: [8, 8],
})

/**
 * Interactive home-geofence map. Shows the home pin + the radius as an actual
 * circle so the abstract "120 m" becomes something you can see. Tap the map or
 * drag the pin to set home — works with no GPS/BLE (key for new users still in
 * setup). The radius circle resizes live as the parent changes `radiusM`.
 */
export function KeepAccessoryMap({
  lat: rawLat,
  lon: rawLon,
  radiusM,
  onPlace,
}: {
  lat: number | null
  lon: number | null
  radiusM: number
  onPlace: (lat: number, lon: number) => void
}) {
  // Sanitize NaN to null on the way in. The parent builds these with
  // `Number(cfg)` over config strings, so junk yields NaN — and `NaN != null`
  // is true, which would let it reach setView/L.latLng, where Leaflet throws
  // "Invalid LatLng object" and takes the whole settings page down.
  const lat = Number.isFinite(rawLat) ? (rawLat as number) : null
  // Legacy configs may hold a world-copy longitude (e.g. -221 for 139°E);
  // wrap so the pin and the readout agree with what gets stored.
  const lon = Number.isFinite(rawLon) ? normalizeLon(rawLon as number) : null
  const containerRef = useRef<HTMLDivElement>(null)
  const mapRef = useRef<L.Map | null>(null)
  const markerRef = useRef<L.Marker | null>(null)
  const circleRef = useRef<L.Circle | null>(null)
  // Latest-callback pattern: map click handlers read this at event time.
  const onPlaceRef = useRef(onPlace)
  useEffect(() => {
    onPlaceRef.current = onPlace
  }, [onPlace])
  // Capture the initial home so the init effect (run once) can center without
  // needing lat/lon in its deps. Subsequent changes are handled below.
  const initRef = useRef<{ lat: number | null; lon: number | null; r: number }>({
    lat,
    lon,
    r: radiusM,
  })

  // Init the map exactly once.
  useEffect(() => {
    const el = containerRef.current
    if (!el || mapRef.current) return
    const { lat: la, lon: lo, r } = initRef.current
    const hasHome = la != null && lo != null
    const center: L.LatLngExpression = hasHome ? [la as number, lo as number] : [39.5, -98.35]

    // worldCopyJump keeps the pin on the primary world copy while panning
    // across the antimeridian (e.g. from the US default to Japan).
    const map = L.map(el, { attributionControl: false, zoomControl: true, worldCopyJump: true })
    mapRef.current = map
    L.tileLayer(TILES, { maxZoom: 19, minZoom: 2 }).addTo(map)
    map.setView(center, hasHome ? 16 : 4)

    const marker = L.marker(center, { draggable: true, icon: HOME_ICON }).addTo(map)
    markerRef.current = marker
    if (!hasHome) marker.setOpacity(0)

    const circle = L.circle(center, {
      radius: r,
      color: "#3b82f6",
      weight: 1.5,
      fillColor: "#3b82f6",
      fillOpacity: hasHome ? 0.12 : 0,
      opacity: hasHome ? 0.9 : 0,
    }).addTo(map)
    circleRef.current = circle
    if (hasHome) map.fitBounds(circle.getBounds(), { padding: [24, 24], maxZoom: 17 })

    // Wrap lng: panning into an adjacent world copy yields values like
    // -221.4 for 138.6°E, which would otherwise display and persist as-is.
    marker.on("dragend", () => {
      const p = marker.getLatLng()
      onPlaceRef.current(p.lat, normalizeLon(p.lng))
    })
    map.on("click", (e: L.LeafletMouseEvent) => {
      onPlaceRef.current(e.latlng.lat, normalizeLon(e.latlng.lng))
    })

    // Leaflet mis-sizes if the container wasn't fully laid out at init
    // (cards/tabs). Nudge it once the next frame settles.
    const t = window.setTimeout(() => map.invalidateSize(), 120)

    return () => {
      window.clearTimeout(t)
      map.remove()
      mapRef.current = null
      markerRef.current = null
      circleRef.current = null
    }
    // Init-once: deliberately no deps. Live updates handled by the effect below.
  }, [])

  // Keep the pin + circle in sync with the live values.
  useEffect(() => {
    const map = mapRef.current
    const marker = markerRef.current
    const circle = circleRef.current
    if (!map || !marker || !circle) return
    if (lat != null && lon != null) {
      const ll = L.latLng(lat, lon)
      marker.setLatLng(ll)
      marker.setOpacity(1)
      circle.setLatLng(ll)
      circle.setRadius(radiusM)
      circle.setStyle({ opacity: 0.9, fillOpacity: 0.12 })
      // Always frame the whole circle so the radius is fully visible no
      // matter how big it's set (your "zoom out to show the whole circle").
      map.fitBounds(circle.getBounds(), { padding: [24, 24], maxZoom: 17 })
    } else {
      marker.setOpacity(0)
      circle.setStyle({ opacity: 0, fillOpacity: 0 })
    }
  }, [lat, lon, radiusM])

  return (
    <div
      ref={containerRef}
      className="h-52 w-full overflow-hidden rounded-lg ring-1 ring-inset ring-white/10"
    />
  )
}
