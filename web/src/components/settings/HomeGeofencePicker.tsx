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
  // Reject NaN config values before formatting.
  if (lat == null || lon == null || !Number.isFinite(lat) || !Number.isFinite(lon)) return ""
  return `${lat.toFixed(5)}, ${normalizeLon(lon).toFixed(5)}`
}

/** Controlled home-geofence editor shared by Keep Accessory and Away Mode. */
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
  /** Fetch the car's last GPS fix for the home center. */
  onUseCurrentLocation?: () => Promise<{ lat: number; lon: number } | null>
  /** Caption below the map. */
  mapHint?: ReactNode
  /** Caption under the radius input. */
  radiusHint?: ReactNode
  /** Persistence failure supplied by the owning hook. */
  saveError?: string | null
}) {
  const [locating, setLocating] = useState(false)
  const [locError, setLocError] = useState<string | null>(null)

  // Clamp the free-form radius only on blur or Enter.
  const [radiusText, setRadiusText] = useState(String(values.radiusM))
  useEffect(() => {
    setRadiusText(String(values.radiusM))
  }, [values.radiusM])

  function commitRadius() {
    const n = Math.round(Number(radiusText))
    if (!Number.isFinite(n) || radiusText.trim() === "") {
      setRadiusText(String(values.radiusM))
      return
    }
    const clamped = Math.min(RADIUS_MAX, Math.max(RADIUS_MIN, n))
    setRadiusText(String(clamped))
    if (clamped !== values.radiusM) onChange({ radiusM: clamped })
  }

  // Coordinates remain free-form until blur or Enter.
  const [coordText, setCoordText] = useState(() => formatCoords(values.homeLat, values.homeLon))
  const [coordError, setCoordError] = useState<string | null>(null)
  // Both coordinates must be finite; parse errors and an unset home are distinct.
  const haveHome = Number.isFinite(values.homeLat) && Number.isFinite(values.homeLon)
  // Synchronize during render to avoid a second pass after every map move.
  // Object.is converges even when a malformed config produced NaN.
  const [syncedFrom, setSyncedFrom] = useState({ lat: values.homeLat, lon: values.homeLon })
  if (!Object.is(values.homeLat, syncedFrom.lat) || !Object.is(values.homeLon, syncedFrom.lon)) {
    setSyncedFrom({ lat: values.homeLat, lon: values.homeLon })
    setCoordText(formatCoords(values.homeLat, values.homeLon))
    // Clear parse errors after an external coordinate change.
    setCoordError(null)
  }

  function commitCoords() {
    const trimmed = coordText.trim()
    if (trimmed === "") {
      setCoordError(null)
      setCoordText(formatCoords(values.homeLat, values.homeLon))
      return
    }
    // Reject empty halves before Number("") can coerce them to zero.
    const parts = trimmed.split(",")
    const rawLat = parts.length === 2 ? parts[0].trim() : ""
    const rawLon = parts.length === 2 ? parts[1].trim() : ""
    const lat = rawLat === "" ? NaN : Number(rawLat)
    const lon = rawLon === "" ? NaN : Number(rawLon)
    if (!Number.isFinite(lat) || !Number.isFinite(lon)) {
      setCoordError(
        'Couldn\'t parse that — enter as "latitude, longitude" (e.g. 30.22214, -97.61833).',
      )
      return
    }
    // Validate typed longitude before normalization so a typo cannot wrap to
    // another valid location. Existing world-copy values remain displayable.
    if (Math.abs(lat) > 90 || Math.abs(lon) > 180) {
      setCoordError("Out of range — latitude must be within ±90, longitude within ±180.")
      return
    }
    const normalizedLon = normalizeLon(lon)
    setCoordError(null)
    // Canonicalize formatting even when the parsed value did not change.
    const committedText = formatCoords(lat, normalizedLon)
    setCoordText(committedText)
    // Compare canonical display precision so focus/blur cannot trigger a PUT
    // or truncate the backend's extra decimal.
    if (committedText !== formatCoords(values.homeLat, values.homeLon)) {
      onChange({ homeLat: lat, homeLon: normalizedLon })
    }
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

      {/* Manual coordinates remain available without GPS. */}
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
      {!haveHome && (
        <p className="text-xs text-amber-400/80">
          No home set — tap the map or enter coordinates above to drop your home pin.
        </p>
      )}

      {locError && <p className="text-xs text-red-400">{locError}</p>}
      {saveError && <p className="text-xs text-red-400">{saveError}</p>}

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
