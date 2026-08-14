import { useEffect, useState } from "react"

// Null represents an unresolved opt-in and prevents experimental UI flashing.
// A module cache shares the config request across consumers.
let cached: boolean | null = null
let inflight: Promise<boolean> | null = null

function readExperimental(): Promise<boolean> {
  if (cached !== null) return Promise.resolve(cached)
  if (inflight) return inflight
  inflight = fetch("/api/setup/config")
    .then((r) => r.json())
    .then((cfg) => {
      const entry = cfg?.SENTRYUSB_EXPERIMENTAL
      const raw =
        typeof entry === "object"
          ? entry?.active
            ? entry.value
            : null
          : entry
      const on =
        typeof raw === "string" &&
        ["yes", "true", "1"].includes(raw.trim().toLowerCase())
      cached = on
      return on
    })
    .catch(() => {
      cached = false
      return false
    })
    .finally(() => {
      inflight = null
    })
  return inflight
}

export function useExperimental(): boolean | null {
  const [enabled, setEnabled] = useState<boolean | null>(cached)
  useEffect(() => {
    let cancelled = false
    readExperimental().then((on) => {
      if (!cancelled) setEnabled(on)
    })
    return () => {
      cancelled = true
    }
  }, [])
  return enabled
}
