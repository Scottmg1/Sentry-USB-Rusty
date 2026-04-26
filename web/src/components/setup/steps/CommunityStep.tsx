import { useState, useEffect } from "react"
import { Users, Paintbrush, Volume2, Info } from "lucide-react"
import type { StepProps } from "../SetupWizard"

const DEFAULT_WRAPS_SIZE_GB = "4"

function isWrapsEnabled(data: StepProps["data"]): boolean {
  const v = data._community_wraps_enabled
  if (v === "true") return true
  if (v === "false") return false
  // Migration / first-load: derive from WRAPS_SIZE in the existing config.
  // If wraps was never configured (size empty or 0), default to enabled
  // so new-install users see the choice as opt-out, matching today's UX.
  const raw = (data.WRAPS_SIZE ?? "").replace(/[gGmM]/g, "").trim()
  if (!raw) return true
  return parseInt(raw, 10) > 0
}

function isChimesEnabled(data: StepProps["data"]): boolean {
  const v = data._community_chimes_enabled
  if (v === "true") return true
  if (v === "false") return false
  return true
}

function numericWrapsSize(data: StepProps["data"]): string {
  return (data.WRAPS_SIZE ?? "").replace(/[^0-9]/g, "")
}

export function CommunityStep({ data, onChange, onBatchChange }: StepProps) {
  const wrapsEnabled = isWrapsEnabled(data)
  const chimesEnabled = isChimesEnabled(data)
  const [sizeFocused, setSizeFocused] = useState(false)
  const [sizeLocal, setSizeLocal] = useState(numericWrapsSize(data))

  // Mirror size from formData when not actively editing (e.g., backup restore)
  useEffect(() => {
    if (!sizeFocused) setSizeLocal(numericWrapsSize(data))
  }, [data, sizeFocused])

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
    // Run only when undefined keys are first detected — derived values are
    // stable for a given config snapshot.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  function setWraps(enabled: boolean) {
    if (enabled) {
      const current = numericWrapsSize(data)
      const ensureSize = current && parseInt(current, 10) > 0 ? data.WRAPS_SIZE ?? "" : DEFAULT_WRAPS_SIZE_GB
      onBatchChange({
        _community_wraps_enabled: "true",
        WRAPS_SIZE: ensureSize,
      })
      setSizeLocal(ensureSize.replace(/[^0-9]/g, "") || DEFAULT_WRAPS_SIZE_GB)
    } else {
      onBatchChange({
        _community_wraps_enabled: "false",
        WRAPS_SIZE: "0",
      })
    }
  }

  function setChimes(enabled: boolean) {
    onChange("_community_chimes_enabled", enabled ? "true" : "false")
  }

  function commitSize(raw: string) {
    const cleaned = raw.replace(/[^0-9]/g, "")
    if (!cleaned || parseInt(cleaned, 10) === 0) {
      // Empty/zero in the size field while Wraps is enabled snaps back to default
      onChange("WRAPS_SIZE", DEFAULT_WRAPS_SIZE_GB)
      setSizeLocal(DEFAULT_WRAPS_SIZE_GB)
    } else {
      onChange("WRAPS_SIZE", cleaned)
      setSizeLocal(cleaned)
    }
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
                Browse and apply community-made wraps and license plates. Requires a dedicated drive partition.
              </p>
            </div>
          </label>

          {wrapsEnabled && (
            <div className="mt-4 ml-7">
              <div className="flex items-center justify-between">
                <label className="text-xs font-medium text-slate-300">Wraps Drive Size</label>
                <span className="font-mono text-xs text-blue-400">
                  {(sizeFocused ? sizeLocal : numericWrapsSize(data)) || DEFAULT_WRAPS_SIZE_GB} GB
                </span>
              </div>
              <div className="mt-2 flex gap-2">
                <input
                  type="text"
                  inputMode="numeric"
                  value={sizeFocused ? sizeLocal : numericWrapsSize(data)}
                  onFocus={() => { setSizeFocused(true); setSizeLocal(numericWrapsSize(data)) }}
                  onBlur={(e) => { setSizeFocused(false); commitSize(e.target.value) }}
                  onChange={(e) => setSizeLocal(e.target.value.replace(/[^0-9]/g, ""))}
                  placeholder={DEFAULT_WRAPS_SIZE_GB}
                  className="w-24 rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-sm text-slate-100 placeholder-slate-600 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"
                />
                <span className="self-center text-xs text-slate-500">GB</span>
              </div>
              <p className="mt-1.5 text-[11px] text-slate-600">
                Storage reserved on the USB drive for community wraps. 4 GB is plenty for most users.
              </p>
            </div>
          )}
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
