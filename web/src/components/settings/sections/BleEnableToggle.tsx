import { useState, useEffect } from "react"
import { GppMaybeIcon, VerifiedUserIcon } from "@/components/icons"
import { PrefCard } from "@/components/settings/PrefCard"
import { Toggle } from "@/components/ui/Toggle"
import { Pill } from "@/components/ui/Pill"

/**
 * Two independent BLE feature toggles for Pi → car:
 *   - Telemetry (battery, temps, TPMS, location, odometer)
 *   - Keep-awake nudge (prevents USB power-off during archive cycles)
 *
 * Both share the same paired BLE key and VIN; they just decide whether
 * each feature does anything. Turning everything off is the kill
 * switch — the iOS-app GATT peripheral is unaffected by either toggle.
 */
export function BleEnableToggle() {
  const [telemetry, setTelemetry] = useState<boolean | null>(null)
  const [keepAwake, setKeepAwake] = useState<boolean | null>(null)
  // Name of another keep-awake provider (Tessie/TeslaFi/Webhook) that's
  // already configured, or null. When set, BLE keep-awake can't be turned
  // on here — only one provider may be active, and switching belongs in
  // the wizard. Telemetry is unaffected (it can coexist with any provider).
  const [blockedBy, setBlockedBy] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [err, setErr] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    Promise.all([
      fetch("/api/system/ble-enabled").then((r) => r.json()),
      fetch("/api/system/ble-keep-awake-enabled").then((r) => r.json()),
    ])
      .then(([t, k]) => {
        if (cancelled) return
        setTelemetry(Boolean(t?.enabled))
        setKeepAwake(Boolean(k?.enabled))
        setBlockedBy(k?.blocked_by ?? null)
      })
      .catch(() => {
        if (cancelled) return
        setTelemetry(false)
        setKeepAwake(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  async function update(
    endpoint: string,
    next: boolean,
    setter: (v: boolean) => void,
  ) {
    setBusy(true)
    setErr(null)
    try {
      const res = await fetch(endpoint, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: next }),
      })
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        throw new Error(data.error || "Failed to update")
      }
      setter(next)
    } catch (e) {
      setErr(e instanceof Error ? e.message : "Failed to update")
    } finally {
      setBusy(false)
    }
  }

  const anyOn = telemetry === true || keepAwake === true
  const loaded = telemetry !== null && keepAwake !== null
  // Block turning BLE keep-awake ON when another provider owns it. If it's
  // somehow already on, leave the toggle enabled so it can be turned off.
  const keepAwakeBlocked = blockedBy !== null && keepAwake !== true
  const icon = anyOn ? (
    <VerifiedUserIcon className="h-3.5 w-3.5" />
  ) : (
    <GppMaybeIcon className="h-3.5 w-3.5" />
  )

  return (
    <PrefCard
      icon={icon}
      halo={anyOn ? "accent" : "amber"}
      title="Tesla BLE"
      badge={
        loaded ? (
          <Pill kind={anyOn ? "accent" : "amber"}>
            {anyOn ? "In use" : "Off"}
          </Pill>
        ) : null
      }
    >
      <div className="flex flex-col gap-3">
        <Toggle
          checked={telemetry === true}
          disabled={busy || !loaded}
          onChange={(v) =>
            update("/api/system/ble-enabled", v, setTelemetry)
          }
          label="Use BLE for telemetry"
          sub="Reads battery, temps, HVAC, TPMS, odometer, and location from your car."
        />
        <Toggle
          checked={keepAwake === true}
          disabled={busy || !loaded || keepAwakeBlocked}
          onChange={(v) =>
            update("/api/system/ble-keep-awake-enabled", v, setKeepAwake)
          }
          label="Use BLE for keep-awake"
          sub={
            keepAwakeBlocked
              ? `${blockedBy} is your keep-awake provider — change the method in Setup to use BLE instead (only one can be active at a time).`
              : "Nudges the car over BLE during archive cycles so USB power stays on."
          }
        />
        {err && <p className="text-xs text-red-400">{err}</p>}
      </div>
    </PrefCard>
  )
}
