import { createContext, useCallback, useContext, useEffect, useRef, useState } from "react"

interface KeepAwakeStatus {
    state: "idle" | "pending" | "active"
    mode: string
    expires_at?: string
    remaining_sec?: number
}

interface KeepAwakeContextValue {
    status: KeepAwakeStatus
    mode: string | null // user preference: "manual" | "auto" | null (not set)
    start: (durationMin: number) => Promise<void>
    stop: () => Promise<void>
    updateMode: (newMode: string) => Promise<void>
}

const KeepAwakeContext = createContext<KeepAwakeContextValue>({
    status: { state: "idle", mode: "" },
    mode: null,
    start: async () => { },
    stop: async () => { },
    updateMode: async () => { },
})

export function useKeepAwake() {
    return useContext(KeepAwakeContext)
}

export function KeepAwakeProvider({ children }: { children: React.ReactNode }) {
    const [status, setStatus] = useState<KeepAwakeStatus>({ state: "idle", mode: "" })
    const [mode, setMode] = useState<string | null>(null)
    const lastHeartbeat = useRef(0)
    const activityTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
    // Timestamp of the last start/stop mutation. Polls that were in-flight
    // before a mutation are ignored so they can't overwrite the fresh state.
    const lastMutation = useRef(0)

    // Load user preference
    useEffect(() => {
        fetch("/api/config/preference?key=keep_awake_webui_mode")
            .then((r) => r.json())
            .then((data) => {
                if ("value" in data) setMode(data.value || "")
            })
            .catch(() => { })
    }, [])

    // Poll status
    useEffect(() => {
        let mounted = true

        async function poll() {
            const startedAt = Date.now()
            try {
                const res = await fetch("/api/keep-awake/status")
                const data: KeepAwakeStatus = await res.json()
                // Ignore polls that started before the last mutation to prevent
                // stale responses from overwriting the optimistic UI update.
                if (mounted && startedAt >= lastMutation.current) setStatus(data)
            } catch { /* ignore */ }
        }

        poll()
        const iv = setInterval(poll, 5000)
        return () => { mounted = false; clearInterval(iv) }
    }, [])

    // Auto mode: send heartbeats on user activity
    useEffect(() => {
        if (mode !== "auto") return

        function sendHeartbeat() {
            const now = Date.now()
            if (now - lastHeartbeat.current < 30_000) return // debounce 30s
            lastHeartbeat.current = now

            fetch("/api/keep-awake/heartbeat", { method: "POST" })
                .then((r) => r.json())
                .then((data) => setStatus((prev) => ({ ...prev, state: data.state })))
                .catch(() => { })
        }

        function onActivity() {
            // Reset idle timer
            if (activityTimer.current) clearTimeout(activityTimer.current)
            activityTimer.current = setTimeout(() => {
                // User went idle — stop sending heartbeats (server will expire after 10 min)
            }, 10 * 60 * 1000)

            sendHeartbeat()
        }

        const events = ["click", "keydown", "scroll", "touchstart", "mousemove"] as const
        events.forEach((e) => window.addEventListener(e, onActivity, { passive: true }))

        // Send initial heartbeat
        sendHeartbeat()

        return () => {
            events.forEach((e) => window.removeEventListener(e, onActivity))
            if (activityTimer.current) clearTimeout(activityTimer.current)
        }
    }, [mode])

    const start = useCallback(async (durationMin: number) => {
        lastMutation.current = Date.now()
        try {
            const res = await fetch("/api/keep-awake/start", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ mode: "manual", duration_min: durationMin }),
            })
            const data: KeepAwakeStatus = await res.json()
            setStatus(data)
        } catch { /* ignore */ }
    }, [])

    const stop = useCallback(async () => {
        lastMutation.current = Date.now()
        setStatus({ state: "idle", mode: "" })
        try {
            await fetch("/api/keep-awake", { method: "DELETE" })
        } catch { /* ignore */ }
    }, [])

    const updateMode = useCallback(async (newMode: string) => {
        setMode(newMode)
        // Save preference to server
        try {
            await fetch("/api/config/preference", {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ key: "keep_awake_webui_mode", value: newMode }),
            })
        } catch { /* ignore */ }
        // When switching to auto, send an immediate heartbeat so the sidebar
        // updates right away instead of waiting for the effect + user activity.
        if (newMode === "auto") {
            lastHeartbeat.current = 0
            try {
                const res = await fetch("/api/keep-awake/heartbeat", { method: "POST" })
                const data = await res.json()
                setStatus((prev) => ({ ...prev, state: data.state }))
            } catch { /* ignore */ }
        }
        // When switching to off, stop any active keep-awake
        if (newMode === "") {
            lastMutation.current = Date.now()
            setStatus({ state: "idle", mode: "" })
            try {
                await fetch("/api/keep-awake", { method: "DELETE" })
            } catch { /* ignore */ }
        }
    }, [])

    return (
        <KeepAwakeContext.Provider value={{ status, mode, start, stop, updateMode }}>
            {children}
        </KeepAwakeContext.Provider>
    )
}
