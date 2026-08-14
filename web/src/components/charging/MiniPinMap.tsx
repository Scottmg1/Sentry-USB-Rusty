import { useEffect, useRef, useState } from "react"
import L from "leaflet"
import "leaflet/dist/leaflet.css"

// Charge sessions render one non-interactive pin and initialize only when visible.
const DARK_TILES =
  "https://{s}.basemaps.cartocdn.com/dark_nolabels/{z}/{x}/{y}{r}.png"

const PIN_COLOR = "#34d399"

export function MiniPinMap({
  lat,
  lon,
  zoom = 15,
  className = "h-20 w-32",
}: {
  lat: number | null | undefined
  lon: number | null | undefined
  zoom?: number
  className?: string
}) {
  const containerRef = useRef<HTMLDivElement>(null)
  const mapRef = useRef<L.Map | null>(null)
  const [visible, setVisible] = useState(false)

  useEffect(() => {
    const el = containerRef.current
    if (!el) return
    const io = new IntersectionObserver(
      (entries) => {
        for (const e of entries) {
          if (e.isIntersecting) {
            setVisible(true)
            io.disconnect()
            break
          }
        }
      },
      { rootMargin: "200px" },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [])

  useEffect(() => {
    if (!visible) return
    const el = containerRef.current
    if (!el || mapRef.current) return
    if (lat == null || lon == null) return

    const map = L.map(el, {
      attributionControl: false,
      zoomControl: false,
      dragging: false,
      scrollWheelZoom: false,
      doubleClickZoom: false,
      touchZoom: false,
      keyboard: false,
      boxZoom: false,
    })
    mapRef.current = map

    L.tileLayer(DARK_TILES, { maxZoom: 18, minZoom: 3 }).addTo(map)
    map.setView([lat, lon], zoom)

    L.circleMarker([lat, lon], {
      radius: 5,
      color: PIN_COLOR,
      weight: 2,
      fillColor: PIN_COLOR,
      fillOpacity: 0.9,
    }).addTo(map)

    return () => {
      map.remove()
      mapRef.current = null
    }
  }, [visible, lat, lon, zoom])

  // Isolate Leaflet pane z-indexes from surrounding page controls.
  return (
    <div
      ref={containerRef}
      className={`relative isolate shrink-0 overflow-hidden rounded-lg bg-slate-900/60 ring-1 ring-inset ring-white/5 ${className}`}
      role="img"
      aria-label="Charge location"
    />
  )
}
