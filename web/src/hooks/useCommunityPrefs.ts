import { useState, useEffect, useCallback } from "react"

export type CommunityMode = "both" | "wraps-only" | "chimes-only" | "none"

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
  // Default to enabled while loading and on missing keys — matches the
  // legacy behavior where the Community tab was always visible. Users who
  // don't want a feature can disable it from Settings.
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

// Helper for callers that update prefs and want to broadcast the change
// so the sidebar / nav / Community page all re-fetch. The fetch is kept
// here so the consumer doesn't need to know the storage backend.
export async function setCommunityPref(
  key: "community_wraps_enabled" | "community_chimes_enabled",
  enabled: boolean,
): Promise<void> {
  await fetch("/api/config/preference", {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ key, value: enabled ? "enabled" : "disabled" }),
  })
  window.dispatchEvent(new CustomEvent("community-prefs-changed"))
}
