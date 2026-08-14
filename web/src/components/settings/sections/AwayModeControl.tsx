import { useState, useRef, useEffect } from "react"
import {
  LocationOnIcon,
  ProgressActivityIcon,
  ScheduleIcon,
  TravelIcon,
  WarningIcon,
  WifiIcon,
} from "@/components/icons"
import { cn } from "@/lib/utils"
import { useAwayMode } from "@/hooks/useAwayMode"
import type { AwayModeKind } from "@/hooks/useAwayMode"
import { PrefCard } from "@/components/settings/PrefCard"
import { Pill, LiveDot } from "@/components/ui/Pill"
import { Row } from "@/components/ui/StatusTile"
import { SegPicker } from "@/components/ui/SegPicker"
import { HomeGeofencePicker } from "@/components/settings/HomeGeofencePicker"
import { TravelModeDialog } from "@/components/settings/TravelModeDialog"
import { api } from "@/lib/api"

const AWAY_PRESETS = [
  { value: 60, label: "1h" },
  { value: 120, label: "2h" },
  { value: 240, label: "4h" },
  { value: 480, label: "8h" },
]

const MODE_OPTIONS: { value: AwayModeKind; label: string }[] = [
  { value: "manual", label: "Manual" },
  { value: "auto", label: "Automatic" },
]

interface Props {
  /** Re-launches the Setup Wizard so the user can configure the AP. */
  onOpenWizard?: () => void
}

export function AwayModeControl({ onOpenWizard }: Props = {}) {
  const { status, enable, disable, setMode, config, updateConfig, saveError, useCurrentLocation } = useAwayMode()
  // Avoid a disabled-state flash before the first compatible response.
  const apConfigured = status.ap_configured !== false
  const mode: AwayModeKind = status.mode ?? "manual"
  const [selectedDuration, setSelectedDuration] = useState(240)
  const [customHours, setCustomHours] = useState("")
  const [customMinutes, setCustomMinutes] = useState("")
  const [useCustom, setUseCustom] = useState(false)
  const [enabling, setEnabling] = useState(false)
  const [confirmOpen, setConfirmOpen] = useState(false)
  const [enablingBle, setEnablingBle] = useState(false)
  // Five icon taps open Travel Mode.
  const [secretOpen, setSecretOpen] = useState(false)
  const [travelOn, setTravelOn] = useState(false)
  const tapCount = useRef(0)
  const tapTimer = useRef<ReturnType<typeof setTimeout> | null>(null)

  // Fetch Travel Mode for the card badge.
  useEffect(() => {
    api.getTravelMode().then((r) => setTravelOn(r.enabled)).catch(() => {})
  }, [])

  useEffect(
    () => () => {
      if (tapTimer.current) clearTimeout(tapTimer.current)
    },
    [],
  )

  function handleSecretTap() {
    tapCount.current += 1
    if (tapTimer.current) clearTimeout(tapTimer.current)
    if (tapCount.current >= 5) {
      tapCount.current = 0
      setSecretOpen(true)
      return
    }
    tapTimer.current = setTimeout(() => {
      tapCount.current = 0
    }, 600)
  }

  const isActive = status.state === "active"
  // The AP is up during a manual timer or an automatic away decision.
  const apUp = mode === "auto" ? status.ap_on === true : isActive

  function getCustomMinutes() {
    const h = parseInt(customHours) || 0
    const m = parseInt(customMinutes) || 0
    return h * 60 + m
  }

  function handleEnableClick() {
    const durationMin = useCustom
      ? Math.min(Math.max(getCustomMinutes(), 1), 1440)
      : selectedDuration
    if (isNaN(durationMin) || durationMin <= 0) return
    setConfirmOpen(true)
  }

  async function handleConfirmEnable() {
    const durationMin = useCustom
      ? Math.min(Math.max(getCustomMinutes(), 1), 1440)
      : selectedDuration
    setConfirmOpen(false)
    setEnabling(true)
    await enable(durationMin)
    setEnabling(false)
  }

  function formatRemaining(sec: number) {
    const h = Math.floor(sec / 3600)
    const m = Math.floor((sec % 3600) / 60)
    if (h > 0) return `${h}h ${m}m remaining`
    return `${m}m remaining`
  }

  function getProgress() {
    // Zero remaining is a real timer value, not missing data.
    if (!status.enabled_at || !status.expires_at || status.remaining_sec == null) return 0
    const total =
      (new Date(status.expires_at).getTime() - new Date(status.enabled_at).getTime()) / 1000
    if (total <= 0) return 0
    return Math.max(0, Math.min(100, ((total - status.remaining_sec) / total) * 100))
  }

  async function enableBle() {
    setEnablingBle(true)
    try {
      await fetch("/api/system/ble-enabled", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ enabled: true }),
      })
    } catch {
      /* leave the warning up; user can retry */
    } finally {
      setEnablingBle(false)
    }
  }

  function autoStatusLine() {
    if (status.geofence_configured === false) {
      return <p className="text-xs text-amber-400/80">Set your home location below to start.</p>
    }
    if (status.gps_stale) {
      return (
        <p className="text-xs text-slate-500">
          Waiting for a fresh location (the car may be asleep)…
        </p>
      )
    }
    if (status.is_home == null) {
      return <p className="text-xs text-slate-500">Waiting for the car’s location…</p>
    }
    return (
      <div className="flex items-center gap-2 text-xs">
        <LocationOnIcon className="h-3 w-3 text-blue-400" />
        <span className="text-slate-300">
          Currently <span className="font-medium">{status.is_home ? "home" : "away"}</span> —
          access point{" "}
          <span className={status.ap_on ? "font-medium text-blue-400" : "text-slate-400"}>
            {status.ap_on ? "on" : "off"}
          </span>
        </span>
      </div>
    )
  }

  return (
    <PrefCard
      icon={
        <span onClick={handleSecretTap} className="cursor-default select-none" role="presentation">
          <WifiIcon className="h-3.5 w-3.5" />
        </span>
      }
      halo="blue"
      title="Away Mode"
      badge={
        apUp || travelOn ? (
          <span className="flex items-center gap-1.5">
            {travelOn && (
              <Pill kind="accent">
                <TravelIcon className="h-3 w-3" /> Travel
              </Pill>
            )}
            {apUp && (
              <Pill kind="sky">
                <LiveDot /> Active
              </Pill>
            )}
          </span>
        ) : null
      }
      disabled={
        !apConfigured
          ? {
              reason:
                "Enable the WiFi Access Point in the Setup Wizard to use Away Mode.",
              cta: onOpenWizard
                ? { label: "Open Setup Wizard", onClick: onOpenWizard }
                : undefined,
            }
          : undefined
      }
    >
      <SegPicker<AwayModeKind>
        options={MODE_OPTIONS}
        value={mode}
        onChange={(m) => setMode(m)}
      />

      {mode === "auto" ? (
        <>
          <p className="t-xs">
            The hotspot turns on by itself when the car leaves your home area, and off when it
            returns (so the Pi rejoins your home WiFi). Uses the car’s location over BLE.
          </p>

          {/* Automatic mode requires the BLE GPS source. */}
          {status.ble_ready === false && (
            <div className="flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3">
              <WarningIcon className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
              <div className="space-y-2">
                <p className="text-xs text-amber-200/90">
                  <span className="font-medium">Automatic needs BLE telemetry.</span> The car’s
                  location comes over BLE, which needs the Pi paired as a key to your car. Turn it
                  on, then pair from the Bluetooth card.
                </p>
                <button
                  type="button"
                  onClick={enableBle}
                  disabled={enablingBle}
                  className="inline-flex items-center gap-1.5 rounded-md border border-amber-500/40 bg-amber-500/15 px-2.5 py-1 text-xs font-medium text-amber-100 transition-colors hover:bg-amber-500/25 disabled:opacity-50"
                >
                  {enablingBle && <ProgressActivityIcon className="h-3 w-3 animate-spin" />}
                  Turn on BLE telemetry
                </button>
              </div>
            </div>
          )}

          {autoStatusLine()}

          <HomeGeofencePicker
            values={config}
            onChange={updateConfig}
            onUseCurrentLocation={useCurrentLocation}
            saveError={saveError}
            mapHint={
              <>
                Tap the map (or drag the pin) to set your home — the blue circle is your radius.
                Anywhere outside it counts as away → the hotspot turns on automatically. Or tap
                “Use current location” to use the car’s GPS.
              </>
            }
            radiusHint="Smaller = the hotspot comes on sooner after you pull away from home."
          />
        </>
      ) : (
        <>
          {status.has_rtc === false && (
            <div className="flex gap-2 rounded-lg border border-amber-500/20 bg-amber-500/5 p-2 text-[10px] text-amber-400/80">
              <WarningIcon className="mt-0.5 h-3 w-3 shrink-0" />
              <p>No RTC detected — timer saved every 30s, may lose accuracy on reboot.</p>
            </div>
          )}

          {isActive ? (
            <>
              <div className="flex items-center justify-between">
                <div className="flex items-center gap-2 text-xs text-blue-400">
                  <ScheduleIcon className="h-3 w-3" />
                  <span className="font-medium">{formatRemaining(status.remaining_sec ?? 0)}</span>
                </div>
                <button
                  onClick={disable}
                  className="rounded-lg border border-red-500/30 bg-red-500/10 px-3 py-1.5 text-[11px] font-medium text-red-400 transition-colors hover:bg-red-500/20"
                >
                  Disable
                </button>
              </div>
              <div className="h-1 overflow-hidden rounded-full bg-white/5">
                <div
                  className="h-full rounded-full bg-blue-500/60 transition-all duration-1000"
                  style={{ width: `${getProgress()}%` }}
                />
              </div>
            </>
          ) : (
            <>
              <div className="flex items-center gap-1.5">
                {AWAY_PRESETS.map((p) => (
                  <button
                    key={p.value}
                    onClick={() => {
                      setSelectedDuration(p.value)
                      setUseCustom(false)
                    }}
                    className={cn(
                      "rounded-lg border px-2.5 py-1.5 text-xs font-medium transition-all",
                      !useCustom && selectedDuration === p.value
                        ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                        : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
                    )}
                  >
                    {p.label}
                  </button>
                ))}
                <button
                  onClick={() => setUseCustom(true)}
                  className={cn(
                    "rounded-lg border px-2.5 py-1.5 text-xs font-medium transition-all",
                    useCustom
                      ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
                      : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
                  )}
                >
                  Custom
                </button>
                <button
                  onClick={handleEnableClick}
                  disabled={enabling || (useCustom && getCustomMinutes() <= 0)}
                  className="ml-auto rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 transition-colors hover:bg-blue-500/25 disabled:opacity-40"
                >
                  {enabling ? "Enabling..." : "Enable"}
                </button>
              </div>

              {useCustom && (
                <div className="flex items-center gap-1.5">
                  <input
                    type="number"
                    min="0"
                    max="24"
                    step="1"
                    placeholder="0"
                    value={customHours}
                    onChange={(e) => setCustomHours(e.target.value)}
                    className="w-14 rounded-lg border border-white/10 bg-white/5 px-2 py-1.5 text-xs text-slate-200 placeholder:text-slate-600 focus:border-blue-500/40 focus:outline-none"
                  />
                  <span className="text-[10px] text-slate-500">hrs</span>
                  <input
                    type="number"
                    min="0"
                    max="59"
                    step="1"
                    placeholder="0"
                    value={customMinutes}
                    onChange={(e) => setCustomMinutes(e.target.value)}
                    className="w-14 rounded-lg border border-white/10 bg-white/5 px-2 py-1.5 text-xs text-slate-200 placeholder:text-slate-600 focus:border-blue-500/40 focus:outline-none"
                  />
                  <span className="text-[10px] text-slate-500">min</span>
                </div>
              )}

              {confirmOpen && (
                <div className="space-y-3 rounded-xl border border-amber-500/30 bg-amber-500/5 p-4">
                  <div className="flex items-start gap-2">
                    <WarningIcon className="mt-0.5 h-4 w-4 shrink-0 text-amber-400" />
                    <div className="space-y-1 text-xs text-amber-300/90">
                      <p className="font-semibold">You may lose connection to this page</p>
                      <p className="text-amber-400/70">
                        Enabling Away Mode will start the WiFi hotspot and disconnect from your home
                        network.
                        {status.ap_ssid && (
                          <>
                            {" "}
                            To continue using the web UI, connect your device to{" "}
                            <span className="font-medium text-amber-300">"{status.ap_ssid}"</span>
                          </>
                        )}
                        {status.ap_ip && (
                          <>
                            {" "}
                            and navigate to{" "}
                            <span className="font-medium text-amber-300">
                              http://{status.ap_ip}
                            </span>
                          </>
                        )}
                        .
                      </p>
                    </div>
                  </div>
                  <div className="flex gap-2">
                    <button
                      onClick={handleConfirmEnable}
                      className="flex-1 rounded-xl border border-amber-500/30 bg-amber-500/10 px-3 py-2 text-xs font-medium text-amber-400 transition-colors hover:bg-amber-500/20"
                    >
                      Enable Away Mode
                    </button>
                    <button
                      onClick={() => setConfirmOpen(false)}
                      className="flex-1 rounded-xl border border-white/10 bg-white/[0.02] px-3 py-2 text-xs font-medium text-slate-400 transition-colors hover:bg-white/[0.05]"
                    >
                      Cancel
                    </button>
                  </div>
                </div>
              )}
            </>
          )}
        </>
      )}

      <div className="space-y-1.5 border-t border-white/5 pt-3">
        <p className="text-xs font-medium text-slate-400">Access point</p>
        {apUp ? (
          status.ap_ssid || status.ap_ip ? (
            <>
              {status.ap_ssid && <Row label="SSID" value={status.ap_ssid} />}
              {status.ap_ip && (
                <Row
                  label="IP"
                  value={<span className="t-mono">{status.ap_ip}</span>}
                />
              )}
              <p className="text-xs text-slate-500">
                Connect to this network to reach the UI from inside the car.
              </p>
            </>
          ) : (
            <p className="text-xs text-slate-600">Bringing up the access point…</p>
          )
        ) : (
          <p className="text-xs text-slate-600">
            {mode === "auto"
              ? "While you’re away, the Pi broadcasts its own WiFi network. The SSID and IP to join will appear here."
              : "Once Away Mode is on, the Pi broadcasts its own WiFi network. The SSID and IP to join will appear here."}
          </p>
        )}
      </div>

      {secretOpen && (
        <TravelModeDialog
          onClose={() => setSecretOpen(false)}
          onChange={setTravelOn}
        />
      )}
    </PrefCard>
  )
}
