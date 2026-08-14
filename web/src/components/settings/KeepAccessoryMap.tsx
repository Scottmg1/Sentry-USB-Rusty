import { useEffect, useRef } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"
import { normalizeLon } from "@/lib/geo"

// Labeled tiles make precise manual placement possible.
const TILES = "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png"

// A CSS divIcon avoids bundled default-marker image URLs.
const HOME_ICON = L.divIcon({
  className: "",
  html: '<div style="width:16px;height:16px;border-radius:50%;background:#3b82f6;border:2px solid #fff;box-shadow:0 0 0 3px rgba(59,130,246,.35)"></div>',
  iconSize: [16, 16],
  iconAnchor: [8, 8],
})

/** Interactive home pin and radius editor that works without GPS or BLE. */
export function KeepAccessoryMap({
  lat: rawLat,
  lon: rawLon,
  radiusM: rawRadiusM,
  onPlace,
}: {
  lat: number | null
  lon: number | null
  radiusM: number
  onPlace: (lat: number, lon: number) => void
}) {
  // Reject NaN config values before they reach Leaflet.
  const lat = Number.isFinite(rawLat) ? (rawLat as number) : null
  // Canonicalize longitudes from repeated Leaflet world copies.
  const lon = Number.isFinite(rawLon) ? normalizeLon(rawLon as number) : null
  // Leaflet rejects NaN radii; 120 m is the shared default.
  const radiusM = Number.isFinite(rawRadiusM) ? rawRadiusM : 120
  const containerRef = useRef<HTMLDivElement>(null)
  const mapRef = useRef<L.Map | null>(null)
  const markerRef = useRef<L.Marker | null>(null)
  const circleRef = useRef<L.Circle | null>(null)
  // Map handlers read the latest callback without rebuilding the map.
  const onPlaceRef = useRef(onPlace)
  useEffect(() => {
    onPlaceRef.current = onPlace
  }, [onPlace])
  // Capture initial center separately from live-value updates.
  const initRef = useRef<{ lat: number | null; lon: number | null; r: number }>({
    lat,
    lon,
    r: radiusM,
  })

  useEffect(() => {
    const el = containerRef.current
    if (!el || mapRef.current) return
    const { lat: la, lon: lo, r } = initRef.current
    const hasHome = la != null && lo != null
    const center: L.LatLngExpression = hasHome ? [la as number, lo as number] : [39.5, -98.35]

    // Keep the pin on the primary world copy across the antimeridian.
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

    // Panning may return a longitude from an adjacent world copy.
    marker.on("dragend", () => {
      const p = marker.getLatLng()
      onPlaceRef.current(p.lat, normalizeLon(p.lng))
    })
    map.on("click", (e: L.LeafletMouseEvent) => {
      onPlaceRef.current(e.latlng.lat, normalizeLon(e.latlng.lng))
    })

    // Recalculate after card/tab layout settles.
    const t = window.setTimeout(() => map.invalidateSize(), 120)

    return () => {
      window.clearTimeout(t)
      map.remove()
      mapRef.current = null
      markerRef.current = null
      circleRef.current = null
    }
    // Live values are handled by the following effect.
  }, [])

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
      // Frame the entire radius after a live update.
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
