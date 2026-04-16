import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react"

interface AwayModeStatus {
    state: "idle" | "active"
    has_rtc?: boolean
    ap_ssid?: string
    ap_ip?: string
    expires_at?: string
    remaining_sec?: number
    enabled_at?: string
}

interface AwayModeContextValue {
    status: AwayModeStatus
    enable: (durationMin: number) => Promise<void>
    disable: () => Promise<void>
}

const AwayModeContext = createContext<AwayModeContextValue>({
    status: { state: "idle" },
    enable: async () => { },
    disable: async () => { },
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
    const lastMutation = useRef(0)

    // Poll status
    useEffect(() => {
        let mounted = true

        async function poll() {
            const startedAt = Date.now()
            try {
                const res = await fetch("/api/away-mode/status")
                const data: AwayModeStatus = await res.json()
                if (mounted && startedAt >= lastMutation.current) {
                    setStatus(data)
                    // Keep localStorage in sync — clear it when idle
                    if (data.state === "idle") {
                        localStorage.removeItem(AWAY_MODE_LS_KEY)
                    }
                }
            } catch { /* ignore — connection may be lost due to Away Mode */ }
        }

        poll()
        const iv = setInterval(poll, 5000)
        return () => { mounted = false; clearInterval(iv) }
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
            // Store in localStorage so the connection banner can reference it
            // even after the API becomes unreachable
            localStorage.setItem(AWAY_MODE_LS_KEY, JSON.stringify({
                enabled_at: data.enabled_at ?? new Date().toISOString(),
                ap_ssid: data.ap_ssid ?? "",
                ap_ip: data.ap_ip ?? "",
            }))
        } catch { /* ignore */ }
    }, [])

    const disable = useCallback(async () => {
        lastMutation.current = Date.now()
        setStatus({ state: "idle" })
        localStorage.removeItem(AWAY_MODE_LS_KEY)
        try {
            await fetch("/api/away-mode", { method: "DELETE" })
        } catch { /* ignore */ }
    }, [])

    return (
        <AwayModeContext.Provider value={{ status, enable, disable }}>
            {children}
        </AwayModeContext.Provider>
    )
}
