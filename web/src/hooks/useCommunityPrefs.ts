import { useState, useEffect, useCallback } from "react"

type CommunityMode = "both" | "wraps-only" | "chimes-only" | "none"

export interface CommunityPrefs {
  wrapsEnabled: boolean
  chimesEnabled: boolean
  mode: CommunityMode
  loading: boolean
  refresh: () => void
}

function computeMode(wraps: boolean, chimes: boolean): CommunityMode {
  if (wraps && chimes) return "both"
  if (wraps) return "wraps-only"
  if (chimes) return "chimes-only"
  return "none"
}

async function fetchPref(key: string): Promise<boolean | null> {
  try {
    const res = await fetch(`/api/config/preference?key=${encodeURIComponent(key)}`)
    if (!res.ok) return null
    const data = await res.json()
    if (data.value === null || data.value === undefined) return null
    return data.value !== "disabled"
  } catch {
    return null
  }
}

export function useCommunityPrefs(): CommunityPrefs {
  // Missing preferences retain the enabled default.
  const [wrapsEnabled, setWrapsEnabled] = useState(true)
  const [chimesEnabled, setChimesEnabled] = useState(true)
  const [loading, setLoading] = useState(true)
  const [reloadKey, setReloadKey] = useState(0)

  const refresh = useCallback(() => setReloadKey((k) => k + 1), [])

  useEffect(() => {
    let cancelled = false
    Promise.all([
      fetchPref("community_wraps_enabled"),
      fetchPref("community_chimes_enabled"),
    ]).then(([wraps, chimes]) => {
      if (cancelled) return
      setWrapsEnabled(wraps ?? true)
      setChimesEnabled(chimes ?? true)
      setLoading(false)
    })

    function onPrefsChanged() {
      if (!cancelled) refresh()
    }
    window.addEventListener("community-prefs-changed", onPrefsChanged)
    return () => {
      cancelled = true
      window.removeEventListener("community-prefs-changed", onPrefsChanged)
    }
  }, [reloadKey, refresh])

  return {
    wrapsEnabled,
    chimesEnabled,
    mode: computeMode(wrapsEnabled, chimesEnabled),
    loading,
    refresh,
  }
}
