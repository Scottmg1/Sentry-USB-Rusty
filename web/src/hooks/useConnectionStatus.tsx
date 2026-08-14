import { createContext, useContext, useEffect, useRef, useState } from "react"
import { wsClient } from "@/lib/ws"

export type ConnectionState = "connected" | "reconnecting" | "disconnected"

interface ConnectionContextValue {
  state: ConnectionState
  /** Set when the server is up but its database is not ("database_unavailable"). */
  degraded: string | null
  retry: () => void
}

const ConnectionContext = createContext<ConnectionContextValue>({
  state: "connected",
  degraded: null,
  retry: () => {},
})

export function useConnectionStatus() {
  return useContext(ConnectionContext)
}

export function ConnectionProvider({ children }: { children: React.ReactNode }) {
  const [state, setState] = useState<ConnectionState>("connected")
  const [degraded, setDegraded] = useState<string | null>(null)
  const disconnectTimer = useRef<ReturnType<typeof setTimeout> | null>(null)
  const httpOk = useRef(true)
  const httpFailCount = useRef(0)

  // WebSockets cycle normally, so connectivity follows HTTP. Require two
  // failures for reconnecting and three for disconnected to absorb stalls.
  function evaluate() {
    if (httpOk.current) {
      if (disconnectTimer.current) {
        clearTimeout(disconnectTimer.current)
        disconnectTimer.current = null
      }
      httpFailCount.current = 0
      setState("connected")
    } else if (httpFailCount.current >= 3) {
      setState("disconnected")
    } else if (httpFailCount.current >= 2) {
      setState("reconnecting")
    }
  }

  useEffect(() => {
    wsClient.connect()
  }, [])

  useEffect(() => {
    let mounted = true
    // Prevent overlapping polls from double-counting one slow response.
    let inFlight = false

    async function poll() {
      if (inFlight) return
      inFlight = true
      try {
        const controller = new AbortController()
        // Match the BLE proxy timeout and allow for browser connection queues.
        const timeout = setTimeout(() => controller.abort(), 15000)
        const res = await fetch("/api/status", {
          signal: controller.signal,
        } as RequestInit)
        clearTimeout(timeout)
        if (mounted) {
          httpOk.current = res.ok
          if (res.ok) {
            httpFailCount.current = 0
            // A malformed status body is treated as normal rather than degraded.
            try {
              const body = await res.clone().json()
              setDegraded(typeof body?.degraded === "string" ? body.degraded : null)
            } catch {
              setDegraded(null)
            }
          } else httpFailCount.current++
          evaluate()
        }
      } catch {
        if (mounted) {
          httpOk.current = false
          httpFailCount.current++
          evaluate()
        }
      } finally {
        inFlight = false
      }
    }

    poll()
    const iv = setInterval(poll, 8000)
    return () => { mounted = false; clearInterval(iv) }
  }, [])

  function retry() {
    wsClient.reconnect()
    setState("reconnecting")
    fetch("/api/status")
      .then((res) => {
        httpOk.current = res.ok
        if (res.ok) httpFailCount.current = 0
        evaluate()
      })
      .catch(() => {
        httpOk.current = false
        httpFailCount.current++
        evaluate()
      })
  }

  return (
    <ConnectionContext.Provider value={{ state, degraded, retry }}>
      {children}
    </ConnectionContext.Provider>
  )
}
