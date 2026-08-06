import { useEffect, useState, type ReactNode } from "react"
import { LocationOnIcon, ProgressActivityIcon } from "@/components/icons"
import { cn } from "@/lib/utils"
import { normalizeLon } from "@/lib/geo"
import { KeepAccessoryMap } from "@/components/settings/KeepAccessoryMap"

export interface HomeGeofenceValues {
  homeLat: number | null
  homeLon: number | null
  radiusM: number
}

const RADIUS_PRESETS = [50, 100, 200, 500]
const RADIUS_MIN = 20
const RADIUS_MAX = 2000

function formatCoords(lat: number | null, lon: number | null): string {
  if (lat == null || lon == null) return ""
  return `${lat.toFixed(5)}, ${normalizeLon(lon).toFixed(5)}`
}

/**
 * Shared home-geofence editor: interactive map (pin + radius circle), a
 * radius input with presets, and an optional "Use current location"
 * button. Pure presentation — the parent owns the values + persistence.
 * Used by both Keep Accessory (12V power geofence) and Away Mode
 * (Automatic AP geofence); the surrounding copy is passed in so neither
 * feature's wording leaks into the other.
 */
export function HomeGeofencePicker({
  values,
  onChange,
  onUseCurrentLocation,
  mapHint,
  radiusHint,
  saveError,
}: {
  values: HomeGeofenceValues
  onChange: (patch: Partial<HomeGeofenceValues>) => void
  /** Optional — fetch the car's last GPS fix to set the home center. */
  onUseCurrentLocation?: () => Promise<{ lat: number; lon: number } | null>
  /** Caption under the map (e.g. "outside the circle counts as away → …"). */
  mapHint?: ReactNode
  /** Caption under the radius input. */
  radiusHint?: ReactNode
  /** Persistence failure from the owning hook — shown so a failed PUT isn't silent. */
  saveError?: string | null
}) {
  const [locating, setLocating] = useState(false)
  const [locError, setLocError] = useState<string | null>(null)

  // Local text state so the radius field can be cleared/typed freely; we
  // only clamp to [20, 2000] on blur/Enter instead of fighting keystrokes.
  const [radiusText, setRadiusText] = useState(String(values.radiusM))
  useEffect(() => {
    setRadiusText(String(values.radiusM))
  }, [values.radiusM])

  function commitRadius() {
    const n = Math.round(Number(radiusText))
    if (!Number.isFinite(n) || radiusText.trim() === "") {
      setRadiusText(String(values.radiusM)) // revert junk/empty to last good
      return
    }
    const clamped = Math.min(RADIUS_MAX, Math.max(RADIUS_MIN, n))
    setRadiusText(String(clamped))
    if (clamped !== values.radiusM) onChange({ radiusM: clamped })
  }

  // Manual "lat, lon" entry — the map requires scrolling/zooming to find a
  // pin-accurate spot, which is slow with no street labels. Same free-typing
  // + commit-on-blur/Enter pattern as the radius field above.
  const [coordText, setCoordText] = useState(() => formatCoords(values.homeLat, values.homeLon))
  const [coordError, setCoordError] = useState<string | null>(null)
  // Re-sync while rendering rather than from an effect: every map click and
  // pin drag pushes new coords through onChange, and an effect would make
  // each one cost a second render pass. This is React's documented "adjust
  // state when a prop changes" shape, and it keeps the file clear of the
  // repo's react-hooks/set-state-in-effect warning budget.
  const [syncedFrom, setSyncedFrom] = useState({ lat: values.homeLat, lon: values.homeLon })
  if (values.homeLat !== syncedFrom.lat || values.homeLon !== syncedFrom.lon) {
    setSyncedFrom({ lat: values.homeLat, lon: values.homeLon })
    setCoordText(formatCoords(values.homeLat, values.homeLon))
  }

  function commitCoords() {
    const trimmed = coordText.trim()
    if (trimmed === "") {
      setCoordError(null)
      setCoordText(formatCoords(values.homeLat, values.homeLon)) // revert to last good
      return
    }
    const parts = trimmed.split(",")
    const lat = parts.length === 2 ? Number(parts[0].trim()) : NaN
    const lon = parts.length === 2 ? Number(parts[1].trim()) : NaN
    if (!Number.isFinite(lat) || !Number.isFinite(lon) || Math.abs(lat) > 90) {
      setCoordError(
        'Couldn\'t parse that — enter as "latitude, longitude" (e.g. 30.22214, -97.61833).',
      )
      return
    }
    setCoordError(null)
    onChange({ homeLat: lat, homeLon: normalizeLon(lon) })
  }

  async function useCurrent() {
    if (!onUseCurrentLocation) return
    setLocating(true)
    setLocError(null)
    try {
      const fix = await onUseCurrentLocation()
      if (fix) onChange({ homeLat: fix.lat, homeLon: fix.lon })
      else
        setLocError(
          "No GPS fix yet — make sure BLE is paired and the car has been polled, then park at home and try again.",
        )
    } catch {
      setLocError("Couldn't read the car's location.")
    } finally {
      setLocating(false)
    }
  }

  return (
    <div className="space-y-3 rounded-lg border border-white/5 bg-white/[0.02] p-3">
      <div className="flex items-center justify-between gap-2">
        <p className="text-sm font-medium text-slate-300">Home location</p>
        {onUseCurrentLocation && (
          <button
            type="button"
            onClick={useCurrent}
            disabled={locating}
            className="inline-flex items-center gap-1.5 rounded-md border border-white/10 bg-white/5 px-2.5 py-1 text-xs text-slate-200 transition-colors hover:border-blue-500/40 disabled:opacity-50"
          >
            {locating ? (
              <ProgressActivityIcon className="h-3 w-3 animate-spin" />
            ) : (
              <LocationOnIcon className="h-3 w-3" />
            )}
            Use current location
          </button>
        )}
      </div>
      <KeepAccessoryMap
        lat={values.homeLat}
        lon={values.homeLon}
        radiusM={values.radiusM}
        onPlace={(la, lo) => onChange({ homeLat: la, homeLon: lo })}
      />
      {mapHint && <p className="text-xs text-slate-600">{mapHint}</p>}

      {/* Manual coordinate entry — faster than hunting for a spot on an
          unlabeled map, and the only option where GPS ("Use current
          location") isn't available. */}
      <div>
        <label className="mb-1 block text-xs font-medium text-slate-400">
          Coordinates
        </label>
        <input
          type="text"
          inputMode="text"
          autoComplete="off"
          spellCheck={false}
          value={coordText}
          onChange={(e) => setCoordText(e.target.value)}
          onBlur={commitCoords}
          onKeyDown={(e) => {
            if (e.key === "Enter") (e.target as HTMLInputElement).blur()
          }}
          placeholder="30.22214, -97.61833"
          className="w-full rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-sm text-slate-100 outline-none transition placeholder:text-slate-600 focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"
        />
        <p className="mt-1 text-xs text-slate-600">
          Latitude, longitude — north &amp; east are positive, south &amp; west are negative.
        </p>
        {coordError && <p className="mt-1 text-xs text-red-400">{coordError}</p>}
      </div>
      {!coordError && values.homeLat == null && (
        <p className="text-xs text-amber-400/80">
          No home set — tap the map or enter coordinates above to drop your home pin.
        </p>
      )}

      {locError && <p className="text-xs text-red-400">{locError}</p>}
      {saveError && <p className="text-xs text-red-400">{saveError}</p>}

      {/* Adjustable radius — number input + quick presets */}
      <div>
        <label className="mb-1 block text-xs font-medium text-slate-400">
          Radius (meters)
        </label>
        <div className="flex flex-wrap items-center gap-2">
          <input
            type="number"
            inputMode="numeric"
            min={RADIUS_MIN}
            max={RADIUS_MAX}
            step={10}
            value={radiusText}
            onChange={(e) => setRadiusText(e.target.value)}
            onBlur={commitRadius}
            onKeyDown={(e) => {
              if (e.key === "Enter") (e.target as HTMLInputElement).blur()
            }}
            className="w-24 rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-sm text-slate-100 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"
          />
          <div className="flex gap-1">
            {RADIUS_PRESETS.map((r) => (
              <button
                key={r}
                type="button"
                onClick={() => onChange({ radiusM: r })}
                className={cn(
                  "rounded-md border px-2 py-1 text-xs transition-colors",
                  values.radiusM === r
                    ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                    : "border-white/10 bg-white/5 text-slate-400 hover:border-white/20",
                )}
              >
                {r}m
              </button>
            ))}
          </div>
        </div>
        {radiusHint && <p className="mt-1 text-xs text-slate-600">{radiusHint}</p>}
      </div>
    </div>
  )
}
