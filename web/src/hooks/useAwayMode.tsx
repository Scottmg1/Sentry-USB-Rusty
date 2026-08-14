import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react"

export type AwayModeKind = "manual" | "auto"

interface AwayModeStatus {
    /** Backend automation mode; undefined until the first compatible response. */
    mode?: AwayModeKind
    state: "idle" | "active"
    has_rtc?: boolean
    /** False when no AP profile exists; undefined until the first response. */
    ap_configured?: boolean
    ap_ssid?: string
    ap_ip?: string
    expires_at?: string
    remaining_sec?: number
    enabled_at?: string
    /** Committed home/away decision: true=home, false=away, null=undecided. */
    is_home?: boolean | null
    /** Whether automatic mode currently commands the AP on. */
    ap_on?: boolean
    geofence_configured?: boolean
    /** Whether the BLE GPS source is enabled. */
    ble_ready?: boolean
    /** Last GPS fix is missing or too old to act on. */
    gps_stale?: boolean
    last_fix_age_sec?: number | null
    home_lat?: number | null
    home_lon?: number | null
    home_radius_m?: number
}

/** Home geofence values for the Automatic-mode editor. */
export interface AwayGeofenceValues {
    homeLat: number | null
    homeLon: number | null
    radiusM: number
}

interface AwayModeContextValue {
    status: AwayModeStatus
    enable: (durationMin: number) => Promise<void>
    disable: () => Promise<void>
    setMode: (mode: AwayModeKind) => Promise<void>
    config: AwayGeofenceValues
    updateConfig: (patch: Partial<AwayGeofenceValues>) => void
    /** Last geofence-save failure, cleared after a successful save. */
    saveError: string | null
    useCurrentLocation: () => Promise<{ lat: number; lon: number } | null>
}

const DEFAULT_CONFIG: AwayGeofenceValues = { homeLat: null, homeLon: null, radiusM: 120 }

const AwayModeContext = createContext<AwayModeContextValue>({
    status: { state: "idle" },
    enable: async () => { },
    disable: async () => { },
    setMode: async () => { },
    config: DEFAULT_CONFIG,
    updateConfig: () => { },
    saveError: null,
    useCurrentLocation: async () => null,
})

export function useAwayMode() {
    return useContext(AwayModeContext)
}

const AWAY_MODE_LS_KEY = "sentryusb_away_mode"

/** Returns locally stored Away Mode info (survives connection loss). */
export function getStoredAwayMode(): { enabled_at: string; ap_ssid: string; ap_ip: string } | null {
    try {
        const raw = localStorage.getItem(AWAY_MODE_LS_KEY)
        if (!raw) return null
        return JSON.parse(raw)
    } catch {
        return null
    }
}

export function AwayModeProvider({ children }: { children: React.ReactNode }) {
    const [status, setStatus] = useState<AwayModeStatus>({ state: "idle" })
    const [config, setConfig] = useState<AwayGeofenceValues>(DEFAULT_CONFIG)
    const [saveError, setSaveError] = useState<string | null>(null)
    const lastMutation = useRef(0)
    const saveTimer = useRef<number | null>(null)

    const refresh = useCallback(async () => {
        const startedAt = Date.now()
        try {
            const res = await fetch("/api/away-mode/status")
            const data: AwayModeStatus = await res.json()
            if (startedAt >= lastMutation.current) {
                setStatus(data)
                if (data.state === "idle" && data.mode !== "auto") {
                    localStorage.removeItem(AWAY_MODE_LS_KEY)
                }
            }
        } catch { /* ignore — connection may be lost due to Away Mode */ }
    }, [])

    useEffect(() => {
        let mounted = true
        const tick = () => { if (mounted) refresh() }
        tick()
        const iv = setInterval(tick, 5000)
        return () => { mounted = false; clearInterval(iv) }
    }, [refresh])

    useEffect(() => {
        let alive = true
        fetch("/api/away-mode/config")
            .then((r) => r.json())
            .then((d) => {
                if (!alive) return
                setConfig({
                    homeLat: typeof d.home_lat === "number" ? d.home_lat : null,
                    homeLon: typeof d.home_lon === "number" ? d.home_lon : null,
                    radiusM: typeof d.home_radius_m === "number" ? d.home_radius_m : 120,
                })
            })
            .catch(() => { })
        return () => { alive = false }
    }, [])

    useEffect(() => () => {
        if (saveTimer.current) clearTimeout(saveTimer.current)
    }, [])

    const enable = useCallback(async (durationMin: number) => {
        lastMutation.current = Date.now()
        try {
            const res = await fetch("/api/away-mode/enable", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ duration_min: durationMin }),
            })
            const data: AwayModeStatus = await res.json()
            setStatus(data)
            // Preserve the deadline for the offline connection banner.
            localStorage.setItem(AWAY_MODE_LS_KEY, JSON.stringify({
                enabled_at: data.enabled_at ?? new Date().toISOString(),
                ap_ssid: data.ap_ssid ?? "",
                ap_ip: data.ap_ip ?? "",
            }))
        } catch { /* ignore */ }
    }, [])

    const disable = useCallback(async () => {
        lastMutation.current = Date.now()
        setStatus((prev) => ({ ...prev, state: "idle" }))
        localStorage.removeItem(AWAY_MODE_LS_KEY)
        try {
            await fetch("/api/away-mode", { method: "DELETE" })
        } catch { /* ignore */ }
    }, [])

    const setMode = useCallback(async (mode: AwayModeKind) => {
        lastMutation.current = Date.now()
        setStatus((prev) => ({ ...prev, mode }))
        try {
            await fetch("/api/away-mode/mode", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ mode }),
            })
        } catch { /* ignore */ }
        // Keep the mutation timestamp so an older in-flight poll cannot revert
        // the optimistic mode; the full refresh also supplies AP metadata.
        refresh()
    }, [refresh])

    const updateConfig = useCallback((patch: Partial<AwayGeofenceValues>) => {
        setConfig((prev) => {
            const next = { ...prev, ...patch }
            if (saveTimer.current) clearTimeout(saveTimer.current)
            // Each PUT remounts the read-only root, so debounce field edits.
            saveTimer.current = window.setTimeout(() => {
                fetch("/api/away-mode/config", {
                    method: "PUT",
                    headers: { "Content-Type": "application/json" },
                    body: JSON.stringify({
                        home_lat: next.homeLat,
                        home_lon: next.homeLon,
                        home_radius_m: next.radiusM,
                    }),
                })
                    .then((res) => {
                        setSaveError(res.ok ? null : "Couldn't save the geofence — the Pi rejected the change.")
                    })
                    .catch(() => {
                        setSaveError("Couldn't save the geofence — the Pi is unreachable.")
                    })
            }, 600)
            return next
        })
    }, [])

    /** Fetch the car's last GPS fix (for the "Use current location" button). */
    const useCurrentLocation = useCallback(async (): Promise<{ lat: number; lon: number } | null> => {
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

    return (
        <AwayModeContext.Provider
            value={{ status, enable, disable, setMode, config, updateConfig, saveError, useCurrentLocation }}
        >
            {children}
        </AwayModeContext.Provider>
    )
}
