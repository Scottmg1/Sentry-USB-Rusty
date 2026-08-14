import { useCallback, useEffect, useRef, useState } from "react"
import type { KeepAccessoryValues } from "@/components/settings/KeepAccessoryConfig"

const DEFAULT: KeepAccessoryValues = {
  enabled: false,
  homeLat: null,
  homeLon: null,
  radiusM: 120,
}

/** Keep-accessory state; writes are debounced because each PUT remounts root. */
export function useKeepAccessory() {
  const [values, setValues] = useState<KeepAccessoryValues>(DEFAULT)
  const [loaded, setLoaded] = useState(false)
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const saveTimer = useRef<number | null>(null)

  useEffect(() => {
    let alive = true
    fetch("/api/system/keep-accessory-config")
      .then((r) => r.json())
      .then((d) => {
        if (!alive) return
        setValues({
          enabled: !!d.enabled,
          homeLat: typeof d.home_lat === "number" ? d.home_lat : null,
          homeLon: typeof d.home_lon === "number" ? d.home_lon : null,
          radiusM: typeof d.home_radius_m === "number" ? d.home_radius_m : 120,
        })
        setLoaded(true)
      })
      .catch(() => setLoaded(true))
    return () => {
      alive = false
    }
  }, [])

  const update = useCallback((patch: Partial<KeepAccessoryValues>) => {
    setValues((prev) => {
      const next = { ...prev, ...patch }
      if (saveTimer.current) clearTimeout(saveTimer.current)
      saveTimer.current = window.setTimeout(() => {
        setSaving(true)
        fetch("/api/system/keep-accessory-config", {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            enabled: next.enabled,
            home_lat: next.homeLat,
            home_lon: next.homeLon,
            home_radius_m: next.radiusM,
          }),
        })
          .then((res) => {
            setSaveError(res.ok ? null : "Couldn't save — the Pi rejected the change.")
          })
          .catch(() => {
            setSaveError("Couldn't save — the Pi is unreachable.")
          })
          .finally(() => setSaving(false))
      }, 600)
      return next
    })
  }, [])

  /** Fetch the car's last GPS fix (for the "Use current location" button). */
  const useCurrentLocation = useCallback(async (): Promise<{
    lat: number
    lon: number
  } | null> => {
    try {
      const d = await fetch("/api/system/keep-accessory-gps").then((r) => r.json())
      if (typeof d.lat === "number" && typeof d.lon === "number") {
        return { lat: d.lat, lon: d.lon }
      }
      return null
    } catch {
      return null
    }
  }, [])

  /** Manual override — flip the car's Keep Accessory toggle right now over BLE. */
  const manualSet = useCallback(async (on: boolean): Promise<boolean> => {
    try {
      const r = await fetch("/api/system/keep-accessory", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ on }),
      })
      return r.ok
    } catch {
      return false
    }
  }, [])

  return { values, loaded, saving, saveError, update, useCurrentLocation, manualSet }
}
