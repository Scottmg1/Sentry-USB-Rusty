import { useState, useEffect } from "react"
import { BrushIcon, GroupIcon, VolumeUpIcon } from "@/components/icons"
import { PrefCard } from "@/components/settings/PrefCard"

export function CommunityFeaturesSection() {
  const [wrapsEnabled, setWrapsEnabled] = useState<boolean>(true)
  const [chimesEnabled, setChimesEnabled] = useState<boolean>(true)
  const [loaded, setLoaded] = useState(false)

  function refreshState() {
    Promise.all([
      fetch("/api/config/preference?key=community_wraps_enabled")
        .then((r) => r.json())
        .catch(() => ({ value: null })),
      fetch("/api/config/preference?key=community_chimes_enabled")
        .then((r) => r.json())
        .catch(() => ({ value: null })),
    ]).then(([wraps, chimes]) => {
      setWrapsEnabled(wraps?.value == null ? true : wraps.value !== "disabled")
      setChimesEnabled(chimes?.value == null ? true : chimes.value !== "disabled")
      setLoaded(true)
    })
  }

  useEffect(() => {
    refreshState()
    function onPrefsChanged() {
      refreshState()
    }
    window.addEventListener("community-prefs-changed", onPrefsChanged)
    return () => window.removeEventListener("community-prefs-changed", onPrefsChanged)
  }, [])

  async function setPref(
    key: "community_wraps_enabled" | "community_chimes_enabled",
    enabled: boolean
  ) {
    await fetch("/api/config/preference", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key, value: enabled ? "enabled" : "disabled" }),
    }).catch(() => {})
    window.dispatchEvent(new CustomEvent("community-prefs-changed"))
  }

  async function handleWrapsToggle(next: boolean) {
    setWrapsEnabled(next)
    await setPref("community_wraps_enabled", next)
  }

  async function handleChimesToggle(next: boolean) {
    setChimesEnabled(next)
    await setPref("community_chimes_enabled", next)
  }

  return (
    <PrefCard
      icon={<GroupIcon className="h-3.5 w-3.5" />}
      halo="violet"
      title="Community Features"
    >
      <label className="flex cursor-pointer items-start justify-between gap-3">
        <div className="flex items-start gap-2">
          <BrushIcon className="mt-0.5 h-3.5 w-3.5 shrink-0 text-blue-400" />
          <div>
            <span className="text-xs font-medium text-slate-200">
              Wraps &amp; License Plates
            </span>
            <span className="mt-0.5 block text-[10px] text-slate-500">
              {wrapsEnabled
                ? "Tab visible. Toggle off to hide."
                : "Hidden. Toggle on to show — your wraps and plates are preserved."}
            </span>
          </div>
        </div>
        <input
          type="checkbox"
          checked={wrapsEnabled}
          disabled={!loaded}
          onChange={(e) => handleWrapsToggle(e.target.checked)}
          className="toggle-switch mt-0.5"
        />
      </label>

      <label className="flex cursor-pointer items-start justify-between gap-3 border-t border-white/5 pt-3">
        <div className="flex items-start gap-2">
          <VolumeUpIcon className="mt-0.5 h-3.5 w-3.5 shrink-0 text-blue-400" />
          <div>
            <span className="text-xs font-medium text-slate-200">Lock Chimes</span>
            <span className="mt-0.5 block text-[10px] text-slate-500">
              Custom Tesla lock-chime sounds. No partition required — toggle freely.
            </span>
          </div>
        </div>
        <input
          type="checkbox"
          checked={chimesEnabled}
          disabled={!loaded}
          onChange={(e) => handleChimesToggle(e.target.checked)}
          className="toggle-switch mt-0.5"
        />
      </label>
    </PrefCard>
  )
}
