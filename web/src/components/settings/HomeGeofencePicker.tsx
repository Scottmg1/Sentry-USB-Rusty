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
  // Number.isFinite, not `== null`: a NaN coordinate (the parent does
  // `Number(cfg)` on a junk config string, which yields NaN) is not null and
  // would reach `.toFixed(5)` below, rendering the literal text "NaN, ..."
  // in the field — while haveHome's own Number.isFinite check correctly
  // hides the "No home set" hint for the same value, so the two would
  // disagree.
  if (lat == null || lon == null || !Number.isFinite(lat) || !Number.isFinite(lon)) return ""
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
  // A geofence needs BOTH halves as real numbers: {lat: 53, lon: null} is not
  // a home, and neither is a NaN the parent produced by `Number(cfg)` on a
  // junk config string — `NaN != null` is true, so a null-only test would call
  // that "set", hide the hint, and render "NaN, ..." in the field.
  // `Number.isFinite` rejects null and NaN alike. Kept independent of
  // coordError — an unparseable entry and an unset home are different states
  // that can hold at the same time.
  const haveHome = Number.isFinite(values.homeLat) && Number.isFinite(values.homeLon)
  // Re-sync while rendering rather than from an effect: every map click and
  // pin drag pushes new coords through onChange, and an effect would make
  // each one cost a second render pass. This is React's documented "adjust
  // state when a prop changes" shape, and it keeps the file clear of the
  // repo's react-hooks/set-state-in-effect warning budget.
  // `Object.is`, not `!==`: a NaN coord (the parent does `Number(cfg)` on a
  // truthy config string, which yields NaN for junk) never equals itself
  // under `!==`, so the condition would hold on every render and re-enter
  // setState forever. `Object.is(NaN, NaN)` is true, so this converges.
  const [syncedFrom, setSyncedFrom] = useState({ lat: values.homeLat, lon: values.homeLon })
  if (!Object.is(values.homeLat, syncedFrom.lat) || !Object.is(values.homeLon, syncedFrom.lon)) {
    setSyncedFrom({ lat: values.homeLat, lon: values.homeLon })
    setCoordText(formatCoords(values.homeLat, values.homeLon))
    // Stale parse error must not survive a coordinate change from the map.
    setCoordError(null)
  }

  function commitCoords() {
    const trimmed = coordText.trim()
    if (trimmed === "") {
      setCoordError(null)
      setCoordText(formatCoords(values.homeLat, values.homeLon)) // revert to last good
      return
    }
    // Both halves must actually carry a number. `Number("")` is 0, and a
    // stray comma still splits into two parts, so "53.5461," / ", -113.4938"
    // / "," would each commit 0 for the missing side — and 0 passes every
    // finite/latitude check below.
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
    // Range-check what was TYPED. normalizeLon exists for Leaflet's repeated
    // world copies (a click on Japan can report -221.4), not for hand entry:
    // running it on a typo folds -976.1833 into a perfectly plausible +103.82
    // and stores the wrong side of the planet without a word. formatCoords
    // still normalizes for DISPLAY, so a geofence written out of range by an
    // older build stays readable and is rewritten canonically as soon as this
    // field is committed — only fresh input has to stay inside the real range.
    if (Math.abs(lat) > 90 || Math.abs(lon) > 180) {
      setCoordError("Out of range — latitude must be within ±90, longitude within ±180.")
      return
    }
    const normalizedLon = normalizeLon(lon)
    setCoordError(null)
    // Snap the field to the committed value ourselves. An entry that differs
    // only in formatting (" 30.5 , -97.6 " for a stored 30.50000, -97.60000)
    // parses to the value the parent already holds, so no prop changes and the
    // sync block above never runs — the field would keep the raw typed text.
    setCoordText(formatCoords(lat, normalizedLon))
    // Only persist a real change: a focus/blur with no edit must not fire a
    // PUT (which remounts the Pi's read-only root) or re-center the map, and
    // must not quietly round a stored value down to formatCoords' 5 decimals.
    // Mirrors the `clamped !== values.radiusM` guard on the radius field.
    if (lat !== values.homeLat || normalizedLon !== values.homeLon) {
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
      {!haveHome && (
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
