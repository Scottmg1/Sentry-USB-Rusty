import { useEffect, useState } from "react"

/** Installed version; null while loading and "unknown" after failure. */
export function useVersion(): string | null {
  const [version, setVersion] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    fetch("/api/system/version")
      .then((r) => r.json())
      .then((data) => {
        if (!cancelled) setVersion(data.version || "unknown")
      })
      .catch(() => {
        if (!cancelled) setVersion("unknown")
      })
    return () => {
      cancelled = true
    }
  }, [])

  return version
}
