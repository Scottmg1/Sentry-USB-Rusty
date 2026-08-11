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

  // HTTP is the primary connectivity signal. WebSocket connections cycle
  // naturally (server timeouts, keepalive, etc.) and don't indicate a real
  // connectivity problem. Only show "reconnecting"/"disconnected" when
  // HTTP polls actually fail.
  //
  // Hysteresis: one failed poll is noise — a status handler held up by a
  // busy disk, or this fetch queuing behind video streams on the
  // browser's per-host connection limit. Two consecutive failures show
  // "reconnecting", three show "disconnected".
  function evaluate() {
    if (httpOk.current) {
      if (disconnectTimer.current) {
        clearTimeout(disconnectTimer.current)
        disconnectTimer.current = null
      }
      httpFailCount.current = 0
      setState("connected")
    } else if (httpFailCount.current >= 3) {
      // Multiple HTTP failures — truly disconnected
      setState("disconnected")
    } else if (httpFailCount.current >= 2) {
      setState("reconnecting")
    }
  }

  // Ensure WebSocket stays connected (it handles its own reconnection)
  useEffect(() => {
    wsClient.connect()
  }, [])

  // HTTP heartbeat poll — primary connectivity signal
  useEffect(() => {
    let mounted = true
    // The 15s abort outlives the 8s interval — without this guard a slow
    // window runs overlapping polls, double-counting one stall as two
    // consecutive failures (and holding two connection slots).
    let inFlight = false

    async function poll() {
      if (inFlight) return
      inFlight = true
      try {
        const controller = new AbortController()
        // 15s: over BLE the proxy itself allows 15s, and a poll that
        // queues behind video streams counts its queue time here too.
        const timeout = setTimeout(() => controller.abort(), 15000)
        const res = await fetch("/api/status", {
          signal: controller.signal,
        } as RequestInit)
        clearTimeout(timeout)
        if (mounted) {
          httpOk.current = res.ok
          if (res.ok) {
            httpFailCount.current = 0
            // Degraded mode: server reachable, DB not. The status body
            // carries the marker; a parse failure just means normal mode.
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
    // Immediate HTTP check
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
