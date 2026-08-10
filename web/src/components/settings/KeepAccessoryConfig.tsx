import { useEffect, useState } from "react"
import { ProgressActivityIcon, WarningIcon } from "@/components/icons"
import { cn } from "@/lib/utils"
import { HomeGeofencePicker } from "@/components/settings/HomeGeofencePicker"

export interface KeepAccessoryValues {
  enabled: boolean
  homeLat: number | null
  homeLon: number | null
  radiusM: number
}

/**
 * Shared, controlled keep-accessory config form — used by both the setup
 * wizard and the Settings card. Pure presentation: the parent owns the
 * values and the persistence. `onUseCurrentLocation` is optional (the
 * setup wizard may run before BLE is paired); when provided it fetches the
 * car's last GPS fix to set the home geofence center.
 */
export function KeepAccessoryConfig({
  values,
  onChange,
  onUseCurrentLocation,
  saveError,
  checkKeepAwake = false,
}: {
  values: KeepAccessoryValues
  onChange: (patch: Partial<KeepAccessoryValues>) => void
  onUseCurrentLocation?: () => Promise<{ lat: number; lon: number } | null>
  /** Persistence failure from the owning hook — surfaced under the geofence map. */
  saveError?: string | null
  /**
   * When true (Settings context), live-check whether "Use BLE for
   * keep-awake" is on and warn if it isn't — the home→OFF release needs
   * the car reachable over BLE through the archive, or accessory power
   * can stay stuck ON at home. Off in the setup wizard (the BLE keep-awake
   * toggle lives right there in the same step).
   */
  checkKeepAwake?: boolean
}) {
  // Keep-awake dependency: null = unknown/loading, true/false = saved state.
  const [keepAwakeOn, setKeepAwakeOn] = useState<boolean | null>(null)
  const [enablingKa, setEnablingKa] = useState(false)

  useEffect(() => {
    if (!checkKeepAwake || !values.enabled) return
    let alive = true
    fetch("/api/system/ble-keep-awake-enabled")
      .then((r) => r.json())
      .then((d) => {
        if (alive) setKeepAwakeOn(Boolean(d?.enabled))
      })
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [checkKeepAwake, values.enabled])

  async function enableKeepAwake() {
    setEnablingKa(true)
    try {
      const r = await fetch("/api/system/ble-keep-awake-enabled", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: true }),
      })
      if (r.ok) setKeepAwakeOn(true)
    } catch {
      /* leave the warning up; user can retry */
    } finally {
      setEnablingKa(false)
    }
  }

  return (
    <div className="space-y-3">
      {/* 12V power gate — the whole feature is off unless this is on */}
      <label
        className={cn(
          "flex cursor-pointer items-start gap-3 rounded-lg border p-3 transition-colors",
          values.enabled
            ? "border-blue-500/40 bg-blue-500/10"
            : "border-white/5 bg-white/[0.02] hover:border-white/10",
        )}
      >
        <input
          type="checkbox"
          checked={values.enabled}
          onChange={(e) => onChange({ enabled: e.target.checked })}
          className="mt-0.5 accent-blue-500"
        />
        <div>
          <p className="text-sm font-medium text-slate-300">
            My Pi is powered from the 12V / cigarette-lighter outlet
          </p>
          <p className="mt-0.5 text-xs text-slate-600">
            Enable only if your Pi runs off the 12V accessory outlet — NOT the glovebox USB
            (the car powers that itself during Sentry). When on, the Pi automatically keeps
            "Keep Accessory Power" enabled while you're parked away from home, so Sentry keeps
            recording with the Pi alive.
          </p>
        </div>
      </label>

      {/* Keep-awake dependency warning — the home→OFF release needs the car
          reachable over BLE through the archive. */}
      {checkKeepAwake && values.enabled && keepAwakeOn === false && (
        <div className="flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3">
          <WarningIcon className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
          <div className="space-y-2">
            <p className="text-xs text-amber-200/90">
              <span className="font-medium">Turn on “Use BLE for keep-awake” too.</span> It keeps
              the car reachable over BLE through the archive so the Pi can power down cleanly at
              home. Without it, accessory power can stay stuck on at home (battery drain).
            </p>
            <button
              type="button"
              onClick={enableKeepAwake}
              disabled={enablingKa}
              className="inline-flex items-center gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/15 px-2.5 py-1 text-xs font-medium text-amber-100 transition-colors hover:bg-amber-500/25 disabled:opacity-50"
            >
              {enablingKa && <ProgressActivityIcon className="h-3 w-3 animate-spin" />}
              Turn on keep-awake
            </button>
          </div>
        </div>
      )}

      {/* Home geofence — only relevant once the feature is enabled */}
      {values.enabled && (
        <HomeGeofencePicker
          values={{ homeLat: values.homeLat, homeLon: values.homeLon, radiusM: values.radiusM }}
          onChange={(patch) => onChange(patch)}
          onUseCurrentLocation={onUseCurrentLocation}
          saveError={saveError}
          mapHint={
            <>
              Tap the map (or drag the pin) to set your home — the blue circle is your radius.
              Anywhere outside it counts as away → Keep Accessory Power turns on automatically.
              {onUseCurrentLocation
                ? " Or tap “Use current location” to use the car’s GPS."
                : ""}
            </>
          }
          radiusHint="The circle on the map resizes as you change this. Increase if your home sometimes reads as a neighbor's address."
        />
      )}
    </div>
  )
}
