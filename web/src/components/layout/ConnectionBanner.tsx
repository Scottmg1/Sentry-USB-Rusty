import { useEffect, useState } from "react"
import { Wifi, WifiOff, Loader2, X } from "lucide-react"
import { cn } from "@/lib/utils"
import { useConnectionStatus, type ConnectionState } from "@/hooks/useConnectionStatus"
import { getStoredAwayMode } from "@/hooks/useAwayMode"

export function ConnectionBanner() {
  const { state, retry } = useConnectionStatus()
  const [visible, setVisible] = useState(false)
  const [displayState, setDisplayState] = useState<ConnectionState | "connected-flash">(state)
  const [dismissed, setDismissed] = useState(false)
  const [prevState, setPrevState] = useState<ConnectionState>(state)

  useEffect(() => {
    if (state === prevState) return
    const wasDisconnected = prevState === "disconnected" || prevState === "reconnecting"
    setPrevState(state)
    setDismissed(false)

    if (state === "connected" && wasDisconnected) {
      // Show brief "Connected" flash
      setDisplayState("connected-flash")
      setVisible(true)
      const timer = setTimeout(() => setVisible(false), 3000)
      return () => clearTimeout(timer)
    } else if (state === "reconnecting" || state === "disconnected") {
      setDisplayState(state)
      setVisible(true)
    } else {
      setVisible(false)
    }
  }, [state])

  if (!visible || dismissed) return null

  // Check if Away Mode was recently enabled (stored in localStorage before connection dropped)
  const awayInfo = (displayState === "disconnected" || displayState === "reconnecting")
    ? getStoredAwayMode()
    : null

  if (awayInfo) {
    const enabledDate = new Date(awayInfo.enabled_at)
    const timeStr = enabledDate.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })

    return (
      <div className="mb-4 flex items-start gap-3 rounded-lg border border-amber-500/30 bg-amber-500/10 px-4 py-3 text-sm text-amber-300">
        <Wifi className="h-4 w-4 shrink-0 mt-0.5" />
        <div className="flex-1 space-y-1">
          <p className="font-medium">Away Mode is active</p>
          <p className="text-xs text-amber-400/70">
            Away Mode was enabled at {timeStr}. The Pi has disconnected from your home WiFi and will not reconnect until the timer expires.
            {awayInfo.ap_ssid && (
              <> Connect to <span className="font-medium text-amber-300">"{awayInfo.ap_ssid}"</span></>
            )}
            {awayInfo.ap_ip && (
              <> and open <span className="font-medium text-amber-300">http://{awayInfo.ap_ip}</span></>
            )}
            {(awayInfo.ap_ssid || awayInfo.ap_ip) && <> to access the web UI.</>}
          </p>
        </div>

        <button
          onClick={() => setDismissed(true)}
          className="shrink-0 rounded p-0.5 opacity-50 transition-opacity hover:opacity-100"
        >
          <X className="h-3.5 w-3.5" />
        </button>
      </div>
    )
  }

  return (
    <div
      className={cn(
        "mb-4 flex items-center gap-3 rounded-lg border px-4 py-2.5 text-sm transition-all",
        displayState === "disconnected" && "border-red-500/30 bg-red-500/10 text-red-300",
        displayState === "reconnecting" && "border-amber-500/30 bg-amber-500/10 text-amber-300",
        displayState === "connected-flash" && "border-emerald-500/30 bg-emerald-500/10 text-emerald-300",
      )}
    >
      {displayState === "reconnecting" && (
        <Loader2 className="h-4 w-4 shrink-0 animate-spin" />
      )}
      {displayState === "disconnected" && (
        <WifiOff className="h-4 w-4 shrink-0" />
      )}
      {displayState === "connected-flash" && (
        <Wifi className="h-4 w-4 shrink-0" />
      )}

      <span className="flex-1">
        {displayState === "reconnecting" && "Reconnecting to Sentry USB..."}
        {displayState === "disconnected" &&
          "Connection lost — check that Sentry USB is powered on and connected to the same network."}
        {displayState === "connected-flash" && "Connected"}
      </span>

      {displayState === "disconnected" && (
        <button
          onClick={retry}
          className="shrink-0 rounded-md border border-red-500/30 px-3 py-1 text-xs font-medium text-red-300 transition-colors hover:bg-red-500/20"
        >
          Retry
        </button>
      )}

      <button
        onClick={() => setDismissed(true)}
        className="shrink-0 rounded p-0.5 opacity-50 transition-opacity hover:opacity-100"
      >
        <X className="h-3.5 w-3.5" />
      </button>
    </div>
  )
}
