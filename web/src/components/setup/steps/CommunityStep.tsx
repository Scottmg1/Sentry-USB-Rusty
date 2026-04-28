import { useEffect } from "react"
import { Users, Paintbrush, Volume2, Info } from "lucide-react"
import type { StepProps } from "../SetupWizard"

function isWrapsEnabled(data: StepProps["data"]): boolean {
  const v = data._community_wraps_enabled
  if (v === "true") return true
  if (v === "false") return false
  return true
}

function isChimesEnabled(data: StepProps["data"]): boolean {
  const v = data._community_chimes_enabled
  if (v === "true") return true
  if (v === "false") return false
  return true
}

export function CommunityStep({ data, onChange, onBatchChange }: StepProps) {
  const wrapsEnabled = isWrapsEnabled(data)
  const chimesEnabled = isChimesEnabled(data)

  // On mount, persist any derived defaults so the rest of the wizard
  // and the eventual save handler see explicit values rather than re-deriving.
  useEffect(() => {
    const updates: Record<string, string> = {}
    if (data._community_wraps_enabled === undefined) {
      updates._community_wraps_enabled = wrapsEnabled ? "true" : "false"
    }
    if (data._community_chimes_enabled === undefined) {
      updates._community_chimes_enabled = chimesEnabled ? "true" : "false"
    }
    if (Object.keys(updates).length > 0) onBatchChange(updates)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  function setWraps(enabled: boolean) {
    // WRAPS_SIZE is always 0 — Wraps & LicensePlate are folders on the cam
    // drive now, no dedicated partition. Writing "0" keeps any older code
    // path that still reads the key happy.
    onBatchChange({
      _community_wraps_enabled: enabled ? "true" : "false",
      WRAPS_SIZE: "0",
    })
  }

  function setChimes(enabled: boolean) {
    onChange("_community_chimes_enabled", enabled ? "true" : "false")
  }

  const noneSelected = !wrapsEnabled && !chimesEnabled

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-2">
        <Users className="h-4 w-4 text-blue-400" />
        <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
          Community Features
        </h3>
      </div>

      <p className="text-xs text-slate-500">
        Choose which community features to enable. You can change this anytime from Settings.
      </p>

      <div className="space-y-3">
        {/* Wraps */}
        <div className="rounded-lg border border-white/5 bg-white/[0.02] p-4">
          <label className="flex cursor-pointer items-start gap-3">
            <input
              type="checkbox"
              checked={wrapsEnabled}
              onChange={(e) => setWraps(e.target.checked)}
              className="mt-0.5 h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500"
            />
            <div className="flex-1">
              <div className="flex items-center gap-2">
                <Paintbrush className="h-4 w-4 text-blue-400" />
                <span className="text-sm font-medium text-slate-200">Wraps &amp; License Plates</span>
              </div>
              <p className="mt-1 text-xs text-slate-500">
                Browse and apply community-made wraps and license plates. Stored as folders on the cam drive — no extra partition.
              </p>
            </div>
          </label>
        </div>

        {/* Lock Chimes */}
        <div className="rounded-lg border border-white/5 bg-white/[0.02] p-4">
          <label className="flex cursor-pointer items-start gap-3">
            <input
              type="checkbox"
              checked={chimesEnabled}
              onChange={(e) => setChimes(e.target.checked)}
              className="mt-0.5 h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500"
            />
            <div className="flex-1">
              <div className="flex items-center gap-2">
                <Volume2 className="h-4 w-4 text-blue-400" />
                <span className="text-sm font-medium text-slate-200">Lock Chimes</span>
              </div>
              <p className="mt-1 text-xs text-slate-500">
                Replace the default Tesla lock chime with custom sounds. No extra partition required.
              </p>
            </div>
          </label>
        </div>
      </div>

      {noneSelected && (
        <div className="flex items-start gap-2 rounded-lg border border-blue-500/20 bg-blue-500/5 px-3 py-2">
          <Info className="mt-0.5 h-3.5 w-3.5 shrink-0 text-blue-400" />
          <p className="text-xs text-blue-300">
            Both features disabled. The Community tab will be hidden — you can re-enable either feature later from Settings.
          </p>
        </div>
      )}
    </div>
  )
}
