import { useState } from "react"
import { PowerIcon, PowerSettingsNewIcon } from "@/components/icons"
import { PrefCard } from "@/components/settings/PrefCard"
import { KeepAccessoryConfig } from "@/components/settings/KeepAccessoryConfig"
import { useKeepAccessory } from "@/hooks/useKeepAccessory"

interface Props {
  /** Re-launches the Setup Wizard so the user can enable the 12V step. */
  onOpenWizard?: () => void
}

/**
 * Settings card for the keep-accessory feature (12V-powered Pis): the 12V
 * gate, the home geofence (with "Use current location" + adjustable radius),
 * and a manual ON/OFF override that hits the car over BLE right now.
 */
export function KeepAccessorySection({ onOpenWizard }: Props = {}) {
  const { values, loaded, saving, saveError, update, useCurrentLocation, manualSet } = useKeepAccessory()
  const [msg, setMsg] = useState<string | null>(null)
  const [pending, setPending] = useState(false)

  // Niche, 12V-only feature for glovebox-USB setups. Once it's been enabled in
  // the session (everOn) we keep showing the full UI even if the user toggles
  // it back off mid-edit, so settings don't vanish mid-change.
  // Render-phase state adjustment (React's "storing information from
  // previous renders" pattern) — sticky for the component's lifetime.
  const [everOn, setEverOn] = useState(false)
  if (values.enabled && !everOn) setEverOn(true)
  const showDisabled = loaded && !values.enabled && !everOn

  async function manual(on: boolean) {
    setPending(true)
    setMsg(null)
    const ok = await manualSet(on)
    setPending(false)
    setMsg(
      ok
        ? `Sent: Keep Accessory ${on ? "ON" : "OFF"}`
        : "Couldn't reach the car — it may be asleep. Try again once it's awake.",
    )
  }

  return (
    <PrefCard
      icon={<PowerIcon className="h-3.5 w-3.5" />}
      halo="amber"
      title="Keep Accessory"
      disabled={
        showDisabled
          ? {
              reason:
                "Enable 'Keep Accessory' in the Setup Wizard. This feature is only useful for 12V-powered Pis.",
              cta: onOpenWizard
                ? { label: "Open Setup Wizard", onClick: onOpenWizard }
                : undefined,
            }
          : undefined
      }
    >
      {!loaded ? (
        <p className="t-xs">Loading…</p>
      ) : (
        <>
          <KeepAccessoryConfig
            values={values}
            onChange={update}
            onUseCurrentLocation={useCurrentLocation}
            saveError={saveError}
            checkKeepAwake
          />

          {values.enabled && (
            <div className="space-y-1.5 border-t border-white/5 pt-3">
              <p className="text-xs font-medium text-slate-400">Manual override</p>
              <p className="text-xs text-slate-600">
                Sets Keep Accessory Power on the car right now. The automatic home/away logic
                takes back over at its next check (about every 30s while the car's awake).
              </p>
              <div className="flex gap-2">
                <button
                  type="button"
                  onClick={() => manual(true)}
                  disabled={pending}
                  className="inline-flex items-center gap-1.5 rounded-md border border-white/10 bg-white/5 px-2.5 py-1 text-xs text-slate-200 transition-colors hover:border-emerald-500/40 disabled:opacity-50"
                >
                  <PowerSettingsNewIcon className="h-3 w-3" /> Force ON now
                </button>
                <button
                  type="button"
                  onClick={() => manual(false)}
                  disabled={pending}
                  className="inline-flex items-center gap-1.5 rounded-md border border-white/10 bg-white/5 px-2.5 py-1 text-xs text-slate-200 transition-colors hover:border-red-500/40 disabled:opacity-50"
                >
                  <PowerSettingsNewIcon className="h-3 w-3" /> Force OFF now
                </button>
              </div>
              {msg && <p className="text-xs text-slate-500">{msg}</p>}
            </div>
          )}

          {saving && <p className="text-xs text-slate-600">Saving…</p>}
        </>
      )}
    </PrefCard>
  )
}
