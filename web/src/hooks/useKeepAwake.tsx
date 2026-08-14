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
    // Ignore polls that began before the latest mutation.
    const lastMutation = useRef(0)

    useEffect(() => {
        fetch("/api/config/preference?key=keep_awake_webui_mode")
            .then((r) => r.json())
            .then((data) => {
                if ("value" in data) setMode(data.value || "")
            })
            .catch(() => { })
    }, [])

    useEffect(() => {
        let mounted = true

        async function poll() {
            const startedAt = Date.now()
            try {
                const res = await fetch("/api/keep-awake/status")
                const data: KeepAwakeStatus = await res.json()
                // Preserve the optimistic result against stale responses.
                if (mounted && startedAt >= lastMutation.current) setStatus(data)
            } catch { /* ignore */ }
        }

        poll()
        const iv = setInterval(poll, 5000)
        return () => { mounted = false; clearInterval(iv) }
    }, [])

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
            if (activityTimer.current) clearTimeout(activityTimer.current)
            activityTimer.current = setTimeout(() => {
                // The server expires keep-awake after ten idle minutes.
            }, 10 * 60 * 1000)

            sendHeartbeat()
        }

        const events = ["click", "keydown", "scroll", "touchstart", "mousemove"] as const
        events.forEach((e) => window.addEventListener(e, onActivity, { passive: true }))

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
        try {
            await fetch("/api/config/preference", {
                method: "PUT",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ key: "keep_awake_webui_mode", value: newMode }),
            })
        } catch { /* ignore */ }
        // Activate automatic mode without waiting for the next user event.
        if (newMode === "auto") {
            lastHeartbeat.current = 0
            try {
                const res = await fetch("/api/keep-awake/heartbeat", { method: "POST" })
                const data = await res.json()
                setStatus((prev) => ({ ...prev, state: data.state }))
            } catch { /* ignore */ }
        }
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
