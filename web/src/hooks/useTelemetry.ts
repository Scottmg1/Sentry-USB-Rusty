import { useState, useEffect, useCallback, useRef } from "react"
import { api } from "@/lib/api"
import type { ClipTelemetry, TelemetryFrame } from "@/lib/api"

export function useTelemetry(clipPath: string | null, frontFile: string | null) {
  const [telemetry, setTelemetry] = useState<ClipTelemetry | null>(null)
  const [loading, setLoading] = useState(false)
  const cacheRef = useRef<Map<string, ClipTelemetry>>(new Map())

  useEffect(() => {
    if (!clipPath || !frontFile) {
      setTelemetry(null)
      return
    }
    const key = `${clipPath}/${frontFile}`
    const cached = cacheRef.current.get(key)
    if (cached) {
      setTelemetry(cached)
      return
    }

    let cancelled = false
    setLoading(true)
    api.getClipTelemetry(clipPath, frontFile)
      .then((data) => {
        if (cancelled) return
        cacheRef.current.set(key, data)
        setTelemetry(data)
      })
      .catch(() => { if (!cancelled) setTelemetry(null) })
      .finally(() => { if (!cancelled) setLoading(false) })

    return () => { cancelled = true }
  }, [clipPath, frontFile])

  // Binary search for nearest frame at a given time
  const frameAtTime = useCallback((seconds: number): TelemetryFrame | null => {
    if (!telemetry || !telemetry.frames.length) return null
    const frames = telemetry.frames
    if (seconds <= frames[0].t) return frames[0]
    if (seconds >= frames[frames.length - 1].t) return frames[frames.length - 1]

    let lo = 0
    let hi = frames.length - 1
    while (lo < hi - 1) {
      const mid = (lo + hi) >> 1
      if (frames[mid].t <= seconds) lo = mid
      else hi = mid
    }
    // Return whichever is closer
    return (seconds - frames[lo].t) <= (frames[hi].t - seconds) ? frames[lo] : frames[hi]
  }, [telemetry])

  return { telemetry, loading, frameAtTime }
}
