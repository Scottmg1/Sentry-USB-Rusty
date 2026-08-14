import { useState, useEffect, useRef, useCallback } from "react"
import {
  BluetoothIcon,
  CheckCircleIcon,
  CheckIcon,
  ErrorIcon,
  ExpandLessIcon,
  ExpandMoreIcon,
  MemoryIcon,
  ProgressActivityIcon,
  UsbIcon,
  VisibilityIcon,
  VisibilityOffIcon,
  WifiOffIcon,
} from "@/components/icons"
import { cn } from "@/lib/utils"
import { wsClient } from "@/lib/ws"
import { PrefCard } from "@/components/settings/PrefCard"
import { Pill, LiveDot } from "@/components/ui/Pill"
import { useUnits } from "@/lib/units"
import { presentBleHealth, type BleHealth } from "@/lib/bleHealth"

// Telemetry reports TPMS in PSI; bar is a display conversion (PRESSURE_UNIT).
const PSI_TO_BAR = 0.0689476

type BleState =
  | "loading"
  | "disabled"
  | "needs_vin"
  | "needs_install"
  | "installing"
  | "idle"
  | "initiating"
  | "waiting"
  | "polling"
  | "paired"
  | "error"

interface BleStatusResp {
  status: "not_paired" | "keys_generated" | "paired" | "repair_required"
  vin?: string
  binaries_installed?: boolean
  note?: string
}

interface BleEnabledResp {
  enabled: boolean
}

interface BleConnectedResp {
  last_success_ts: number
  seconds_ago: number | null
  sample_count_10min: number
  /** Current radio owner, or null when free. */
  radio_owner: string | null
  /** Whether the archive loop currently owns an active cycle. */
  archiving: boolean
  health?: BleHealth
}

interface ClockStatusResp {
  synced: boolean
  has_rtc: boolean
  ntp_synced: boolean
  /** True when a bad clock without RTC requires network synchronization. */
  show_warning: boolean
}

interface BleAdapter {
  id: string                          // hci0, hci1, ...
  source: "onboard" | "external"
  address: string | null
}

interface BleAdaptersResp {
  current: string
  default: string
  available: BleAdapter[]
}

interface BleLatestSample {
  ts: number | null
  seconds_ago?: number
  battery_pct?: number | null
  interior_temp_c?: number | null
  exterior_temp_c?: number | null
  hvac_on?: boolean | null
  tire_fl_psi?: number | null
  tire_fr_psi?: number | null
  tire_rl_psi?: number | null
  tire_rr_psi?: number | null
  odometer_mi?: number | null
  location_name?: string | null
  /** Live gate inputs; "unknown" and "absent" mean unread. */
  sentry_mode?: string | null
  charging_state?: string | null
  shift_state?: string | null
  source?: string
  /** Fresh body-controller with stale state indicates intentional Quiet mode. */
  body_controller_seconds_ago?: number | null
  /** Field age may exceed envelope age when only one poll source fails. */
  field_secs_ago?: {
    battery_pct?: number | null
    interior_temp_c?: number | null
    exterior_temp_c?: number | null
    tires?: number | null
  } | null
}

/**
 * BLE pairing with VIN validation and lazy binary installation. Pairing starts
 * after the install-complete event and follows WebSocket status with polling fallback.
 */
export function BlePairButton() {
  const [bleState, setBleState] = useState<BleState>("loading")
  const [bleMsg, setBleMsg] = useState("")
  const [vin, setVin] = useState("")
  const [savedVin, setSavedVin] = useState("")
  const [enabled, setEnabled] = useState<boolean | null>(null)
  const [binariesInstalled, setBinariesInstalled] = useState<boolean>(false)
  const [lastSuccessTs, setLastSuccessTs] = useState<number>(0)
  const [sampleCount10min, setSampleCount10min] = useState<number>(0)
  const [radioOwner, setRadioOwner] = useState<string | null>(null)
  const [archiving, setArchiving] = useState<boolean>(false)
  const [bleHealth, setBleHealth] = useState<BleHealth | null>(null)
  const [nowTs, setNowTs] = useState<number>(Math.floor(Date.now() / 1000))
  const [outputOpen, setOutputOpen] = useState(false)
  const [latestSample, setLatestSample] = useState<BleLatestSample | null>(null)
  const [sampleLoading, setSampleLoading] = useState(false)
  const [adapters, setAdapters] = useState<BleAdaptersResp | null>(null)
  const [adapterSwitching, setAdapterSwitching] = useState(false)
  const [adapterError, setAdapterError] = useState<string | null>(null)
  const [clockStatus, setClockStatus] = useState<ClockStatusResp | null>(null)
  const [vinRevealed, setVinRevealed] = useState(false)

  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const connPollRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const tickRef = useRef<ReturnType<typeof setInterval> | null>(null)
  const samplePollRef = useRef<ReturnType<typeof setInterval> | null>(null)

  const reloadStatus = useCallback(async () => {
    try {
      const [enabledRes, statusRes] = await Promise.all([
        fetch("/api/system/ble-enabled").then((r) => r.json() as Promise<BleEnabledResp>),
        fetch("/api/system/ble-status?quick=true").then((r) => r.json() as Promise<BleStatusResp>),
      ])
      const en = Boolean(enabledRes?.enabled)
      setEnabled(en)
      setBinariesInstalled(Boolean(statusRes?.binaries_installed))
      const fetchedVin = statusRes?.vin ?? ""
      setVin(fetchedVin)
      setSavedVin(fetchedVin)

      if (!en) {
        setBleState("disabled")
        setBleMsg("BLE is disabled. Enable it in the toggle above to pair.")
        return
      }
      if (statusRes?.status === "repair_required") {
        setBleState("paired")
        setBleHealth({
          severity: "red",
          code: "repair_required",
          since_ts: null,
          label: "Re-pair required",
          guidance:
            "Open Settings, select Re-pair, then tap your key card on the center console.",
        })
        setBleMsg(
          statusRes.note ||
            "The car rejected this SentryUSB key. Re-pair, then tap the key card on the center console.",
        )
        return
      }
      if (statusRes?.status === "paired") {
        setBleState("paired")
        setBleMsg("Paired — click to re-pair.")
        return
      }
      if (!fetchedVin) {
        setBleState("needs_vin")
        setBleMsg("Enter your Tesla's VIN to begin pairing.")
        return
      }
      if (!statusRes?.binaries_installed) {
        setBleState("needs_install")
        setBleMsg("BLE support not installed. Click to install + pair.")
        return
      }
      setBleState("idle")
      setBleMsg("")
    } catch {
      setEnabled(false)
      setBleState("error")
      setBleMsg("Could not load BLE status.")
    }
  }, [])

  useEffect(() => {
    reloadStatus()
  }, [reloadStatus])

  useEffect(() => {
    const unsub = wsClient.subscribe("ble_status", (data: unknown) => {
      const d = data as {
        status: string
        error?: string
        output?: string
        /** Successful key request followed by failed session probe, indicating dropped writes. */
        verify_failed?: boolean
      }
      if (d.status === "pairing") {
        setBleState("initiating")
        setBleMsg("Sending pairing request to car...")
      } else if (d.status === "error") {
        setBleState("error")
        const errMsg = d.error || "Unknown error"
        if (d.verify_failed) {
          // Surface the backend's hardware-specific write failure.
          setBleMsg(errMsg)
        } else if (errMsg.includes("maximum number of BLE")) {
          setBleMsg("Too many BLE devices active. Turn off Bluetooth on nearby phone keys and try again.")
        } else if (errMsg.includes("timed out")) {
          setBleMsg("BLE connection timed out. Make sure the Pi is near the car and try again.")
        } else {
          setBleMsg(errMsg)
        }
        cleanup()
      } else if (d.status === "waiting") {
        setBleState("waiting")
        setBleMsg("Tap your keycard on the center console to confirm pairing.")
        startPairingPoll()
      }
    })
    return () => {
      unsub()
      cleanup()
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    const unsub = wsClient.subscribe("ble_install_status", (data: unknown) => {
      const d = data as { status: string; message?: string; error?: string }
      if (d.status === "installing") {
        setBleState("installing")
        setBleMsg("Installing BLE support...")
      } else if (d.status === "progress") {
        setBleState("installing")
        if (d.message) setBleMsg(d.message)
      } else if (d.status === "done") {
        // Refresh installed state before continuing the deferred pair flow.
        setBinariesInstalled(true)
        runPairAfterInstall()
      } else if (d.status === "error") {
        setBleState("error")
        setBleMsg(d.error || "Install failed")
      }
    })
    return () => unsub()
  }, [])

  // Poll authenticated connection health every ten seconds; update age labels locally.
  useEffect(() => {
    let cancelled = false
    async function fetchConn() {
      try {
        const res = await fetch("/api/system/ble-connected")
        const d = (await res.json()) as BleConnectedResp
        if (!cancelled) {
          setLastSuccessTs(d?.last_success_ts || 0)
          setSampleCount10min(d?.sample_count_10min ?? 0)
          setRadioOwner(d?.radio_owner ?? null)
          setArchiving(Boolean(d?.archiving))
          setBleHealth(d?.health ?? null)
          if (d?.health?.code === "repair_required") setOutputOpen(false)
        }
      } catch {
        /* ignore */
      }
    }
    fetchConn()
    connPollRef.current = setInterval(fetchConn, 10_000)
    tickRef.current = setInterval(() => setNowTs(Math.floor(Date.now() / 1000)), 1_000)
    return () => {
      cancelled = true
      if (connPollRef.current) clearInterval(connPollRef.current)
      if (tickRef.current) clearInterval(tickRef.current)
    }
  }, [])

  // Poll the latest sample every five seconds while output is visible.
  const fetchLatestSample = useCallback(async () => {
    setSampleLoading(true)
    try {
      const res = await fetch("/api/system/ble-latest-sample")
      const d = (await res.json()) as BleLatestSample
      setLatestSample(d)
    } catch {
      /* leave previous value */
    } finally {
      setSampleLoading(false)
    }
  }, [])


  useEffect(() => {
    if (!outputOpen) {
      if (samplePollRef.current) {
        clearInterval(samplePollRef.current)
        samplePollRef.current = null
      }
      return
    }
    fetchLatestSample()
    samplePollRef.current = setInterval(fetchLatestSample, 5_000)
    return () => {
      if (samplePollRef.current) clearInterval(samplePollRef.current)
      samplePollRef.current = null
    }
  }, [outputOpen, fetchLatestSample])

  // Poll adapters every five seconds so hot-plugged radios appear without refresh.
  const fetchAdapters = useCallback(async () => {
    try {
      const res = await fetch("/api/system/ble-adapters")
      if (res.ok) {
        const d = (await res.json()) as BleAdaptersResp
        // Validate the array before render maps over it.
        if (Array.isArray(d?.available)) setAdapters(d)
      }
    } catch {
      /* leave previous value */
    }
  }, [])

  useEffect(() => {
    fetchAdapters()
    const iv = setInterval(fetchAdapters, 5_000)
    return () => clearInterval(iv)
  }, [fetchAdapters])

  // Stop polling after sync; the sampler pauses while timestamps cannot match drives.
  useEffect(() => {
    let stopped = false
    let iv: ReturnType<typeof setInterval> | null = null
    const fetchClock = async () => {
      try {
        const res = await fetch("/api/system/clock-status")
        if (!res.ok) return
        const d = (await res.json()) as ClockStatusResp
        if (stopped) return
        setClockStatus(d)
        if (d.synced && iv) {
          clearInterval(iv)
          iv = null
        }
      } catch {
        /* leave previous value */
      }
    }
    fetchClock()
    iv = setInterval(fetchClock, 5_000)
    return () => {
      stopped = true
      if (iv) clearInterval(iv)
    }
  }, [])

  const switchAdapter = useCallback(async (id: string) => {
    setAdapterSwitching(true)
    setAdapterError(null)
    try {
      const res = await fetch("/api/system/ble-adapter", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ adapter: id }),
      })
      if (!res.ok) {
        const err = await res.json().catch(() => null)
        setAdapterError(err?.error || `Switch failed (${res.status})`)
      } else {
        // The next adapter poll confirms the optimistic selection.
        setAdapters((prev) => prev ? { ...prev, current: id } : prev)
        // Refresh after both BLE services restart.
        setTimeout(() => {
          fetchAdapters()
        }, 2_000)
      }
    } catch (e) {
      setAdapterError(
        e instanceof Error ? e.message : "Could not reach server",
      )
    } finally {
      setAdapterSwitching(false)
    }
  }, [fetchAdapters])

  function cleanup() {
    if (pollRef.current) {
      clearInterval(pollRef.current)
      pollRef.current = null
    }
    if (timeoutRef.current) {
      clearTimeout(timeoutRef.current)
      timeoutRef.current = null
    }
  }

  function startPairingPoll() {
    cleanup()
    let count = 0
    pollRef.current = setInterval(async () => {
      count++
      try {
        const res = await fetch("/api/system/ble-status")
        if (res.ok) {
          const data = (await res.json()) as BleStatusResp
          if (data.status === "paired") {
            setBleState("paired")
            setBleHealth(null)
            setBleMsg("Successfully paired with car!")
            cleanup()
            return
          }
        }
      } catch {
        /* ignore */
      }
      if (count >= 12) {
        setBleState("error")
        setBleMsg(
          "Pairing timed out. Make sure you tapped your keycard on the center console, then try again.",
        )
        cleanup()
      }
    }, 5000)
    timeoutRef.current = setTimeout(() => {
      if (bleState !== "paired" && bleState !== "error") {
        setBleState("error")
        setBleMsg("Pairing timed out. Please try again.")
        cleanup()
      }
    }, 65_000)
  }

  function validVin(v: string): boolean {
    const trimmed = v.trim().toUpperCase()
    return trimmed.length === 17 && /^[A-Z0-9]+$/.test(trimmed)
  }

  // Installation completion resumes the deferred pair flow.
  async function runPairAfterInstall() {
    setBleState("initiating")
    setBleMsg("Install complete. Sending pairing request...")
    try {
      const res = await fetch("/api/system/ble-pair", { method: "POST" })
      if (!res.ok) {
        const data = await res.json().catch(() => ({}))
        throw new Error(data.error || "Failed to initiate BLE pairing")
      }
    } catch (err) {
      setBleState("error")
      setBleMsg(err instanceof Error ? err.message : "Failed to initiate pairing")
    }
  }

  async function handlePair() {
    if (!enabled) {
      setBleMsg("BLE is disabled. Enable it in the toggle above first.")
      return
    }
    const vinUpper = vin.trim().toUpperCase()
    if (!validVin(vinUpper)) {
      setBleState("error")
      setBleMsg("VIN must be exactly 17 alphanumeric characters.")
      return
    }

    // Persist a changed VIN before installing or pairing.
    if (vinUpper !== savedVin) {
      try {
        const res = await fetch("/api/system/ble-vin", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ vin: vinUpper }),
        })
        if (!res.ok) {
          const data = await res.json().catch(() => ({}))
          throw new Error(data.error || "Failed to save VIN")
        }
        setSavedVin(vinUpper)
        setVin(vinUpper)
      } catch (err) {
        setBleState("error")
        setBleMsg(err instanceof Error ? err.message : "Failed to save VIN")
        return
      }
    }

    // Defer pairing until an asynchronous installation completes.
    if (!binariesInstalled) {
      setBleState("installing")
      setBleMsg("Installing BLE support...")
      try {
        const res = await fetch("/api/system/ble-install", { method: "POST" })
        if (!res.ok) {
          const data = await res.json().catch(() => ({}))
          throw new Error(data.error || "Failed to start install")
        }
        const data = (await res.json()) as { already_installed: boolean }
        if (data.already_installed) {
          // An already-installed response needs no completion event.
          setBinariesInstalled(true)
          runPairAfterInstall()
        }
      } catch (err) {
        setBleState("error")
        setBleMsg(err instanceof Error ? err.message : "Failed to start install")
      }
      return
    }

    // Pair immediately when dependencies are present.
    runPairAfterInstall()
  }

  function handleReset() {
    cleanup()
    reloadStatus()
  }

  const isActive =
    bleState === "initiating" ||
    bleState === "waiting" ||
    bleState === "polling" ||
    bleState === "installing"

  const secondsAgo = lastSuccessTs > 0 ? Math.max(0, nowTs - lastSuccessTs) : null
  const showLive = bleState === "paired"
  const healthPresentation = presentBleHealth(bleHealth, secondsAgo)
  const repairRequired = showLive && healthPresentation.repairRequired
  const degraded = showLive && healthPresentation.severity === "yellow"

  const halo: "accent" | "red" | "amber" | "blue" | "slate" =
    bleState === "disabled"
      ? "slate"
      : repairRequired
        ? "red"
        : degraded
          ? "amber"
          : bleState === "paired"
            ? "accent"
            : bleState === "error"
              ? "red"
              : isActive
                ? "amber"
                : "blue"

  const icon =
    bleState === "loading" ? (
      <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />
    ) : isActive ? (
      <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />
    ) : repairRequired ? (
      <ErrorIcon className="h-3.5 w-3.5" />
    ) : degraded ? (
      <WifiOffIcon className="h-3.5 w-3.5" />
    ) : bleState === "paired" ? (
      <CheckCircleIcon className="h-3.5 w-3.5" />
    ) : bleState === "error" ? (
      <ErrorIcon className="h-3.5 w-3.5" />
    ) : (
      <BluetoothIcon className="h-3.5 w-3.5" />
    )

  // Authenticated telemetry outranks raw adapter connectivity.
  const liveKind = healthPresentation.severity === "green"
    ? "accent"
    : healthPresentation.severity === "red"
      ? "rose"
      : "amber"
  const liveIcon = healthPresentation.severity === "green" ? (
    <LiveDot />
  ) : healthPresentation.code === "paused_archiving" ||
      healthPresentation.code === "paused_keep_awake" ||
      healthPresentation.code === "reconnecting" ? (
    <ProgressActivityIcon className="h-3 w-3 animate-spin" />
  ) : healthPresentation.severity === "red" ? (
    <ErrorIcon className="h-3 w-3" />
  ) : (
    <WifiOffIcon className="h-3 w-3" />
  )

  const badge = (() => {
    if (bleState === "paired") {
      if (repairRequired) {
        return (
          <Pill kind="rose" className="flex items-center gap-1">
            <ErrorIcon className="h-3 w-3" />
            {healthPresentation.label}
          </Pill>
        )
      }
      return (
        <span className="flex items-center gap-1.5">
          <Pill kind="accent">Paired</Pill>
          {showLive && (
            <Pill kind={liveKind} className="flex items-center gap-1">
              {liveIcon}
              {healthPresentation.label}
            </Pill>
          )}
        </span>
      )
    }
    if (bleState === "disabled") return <Pill kind="slate">Disabled</Pill>
    if (bleState === "needs_install") return <Pill kind="amber">Install needed</Pill>
    if (bleState === "needs_vin") return <Pill kind="amber">VIN needed</Pill>
    return null
  })()

  const buttonLabel = (() => {
    if (bleState === "disabled") return "Disabled"
    if (bleState === "loading") return "Loading..."
    if (bleState === "installing") return "Installing..."
    if (bleState === "initiating") return "Starting..."
    if (bleState === "waiting") return "Waiting for keycard..."
    if (bleState === "polling") return "Pairing..."
    if (bleState === "paired") return "Re-pair"
    if (bleState === "error") return "Retry"
    if (bleState === "needs_install") return "Install + Pair"
    return "Pair BLE"
  })()

  const buttonDisabled =
    isActive ||
    bleState === "loading" ||
    bleState === "disabled" ||
    !validVin(vin)

  const buttonHandler = bleState === "error" ? handleReset : handlePair
  const statusMessage = showLive && healthPresentation.severity !== "green"
    ? healthPresentation.guidance
    : bleMsg || "Initiate Bluetooth Low Energy pairing with your car."

  return (
    <PrefCard icon={icon} halo={halo} title="BLE Pairing" badge={badge}>
      {/* VIN input — always visible so users can update it any time.
          Masked by default once a full 17-char VIN is set, since
          screenshots of the settings page (like the ones users share
          for troubleshooting) shouldn't leak the VIN. The eye toggle
          reveals it; partial values (mid-edit) always show in full so
          typing isn't surprising. */}
      <label className="flex flex-col gap-1">
        <span className="t-xs text-slate-500">Tesla VIN (17 chars)</span>
        <div className="relative">
          <input
            type="text"
            value={
              !vinRevealed && vin.length === 17
                ? `${vin.slice(0, 3)}${"•".repeat(11)}${vin.slice(-3)}`
                : vin
            }
            maxLength={17}
            autoCapitalize="characters"
            spellCheck={false}
            readOnly={!vinRevealed && vin.length === 17}
            disabled={bleState === "disabled" || isActive || bleState === "loading"}
            onChange={(e) => setVin(e.target.value.toUpperCase())}
            placeholder="5YJ3E1EA..."
            className={cn(
              "w-full rounded-lg border bg-white/5 px-3 py-1.5 pr-9 font-mono text-xs uppercase tracking-wider text-slate-100",
              "placeholder-slate-600 outline-none transition focus:ring-1",
              vin && !validVin(vin)
                ? "border-red-500/50 focus:border-red-500/50 focus:ring-red-500/25"
                : "border-white/10 focus:border-blue-500/50 focus:ring-blue-500/25",
              "disabled:opacity-50",
              !vinRevealed && vin.length === 17 && "cursor-default",
            )}
          />
          {vin.length === 17 && (
            <button
              type="button"
              onClick={() => setVinRevealed((v) => !v)}
              disabled={bleState === "disabled" || isActive || bleState === "loading"}
              title={vinRevealed ? "Hide VIN" : "Reveal VIN"}
              aria-label={vinRevealed ? "Hide VIN" : "Reveal VIN"}
              className="absolute right-1.5 top-1/2 -translate-y-1/2 rounded p-1 text-slate-500 transition-colors hover:bg-white/5 hover:text-slate-300 disabled:opacity-50"
            >
              {vinRevealed ? <VisibilityOffIcon className="h-3.5 w-3.5" /> : <VisibilityIcon className="h-3.5 w-3.5" />}
            </button>
          )}
        </div>
      </label>

      <p
        className={cn(
          "text-xs",
          repairRequired
            ? "text-red-400"
            : degraded
              ? "text-amber-400"
              : bleState === "paired"
                ? "text-emerald-400"
                : bleState === "error"
                  ? "text-red-400"
                  : bleState === "waiting"
                    ? "font-medium text-amber-400"
                    : "text-slate-500",
        )}
      >
        {statusMessage}
      </p>

      <div className="flex flex-wrap items-center gap-2">
        <button
          onClick={buttonHandler}
          disabled={buttonDisabled}
          className={cn(
            "self-start rounded-lg px-3 py-1.5 text-xs font-medium transition-colors disabled:opacity-50",
            repairRequired
              ? "bg-red-500/15 text-red-400 hover:bg-red-500/25"
              : bleState === "paired"
                ? "bg-white/5 text-slate-300 hover:bg-white/10"
                : bleState === "error"
                  ? "bg-red-500/15 text-red-400 hover:bg-red-500/25"
                  : "bg-blue-500/15 text-blue-400 hover:bg-blue-500/25",
          )}
        >
          {buttonLabel}
        </button>
        {/* "Show output" is only useful when there's actually data
            to show — gate on a recent successful round-trip OR a
            non-zero sample count in the last 10 min. */}
        {bleState === "paired" && !repairRequired &&
          ((secondsAgo !== null && secondsAgo < 600) || sampleCount10min > 0) && (
            <button
              onClick={() => setOutputOpen((v) => !v)}
              className="inline-flex items-center gap-1 self-start rounded-lg bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10"
            >
              {outputOpen ? <ExpandLessIcon className="h-3 w-3" /> : <ExpandMoreIcon className="h-3 w-3" />}
              {outputOpen ? "Hide output" : "Show output"}
            </button>
          )}
        {/* The "Diagnostics" button that used to live here was
            removed once the dedicated Logs → Bluetooth tab landed.
            That page (Logs.tsx) shows the same per-poll outcomes
            plus the full bundle download, so there's no reason to
            also embed a stripped-down version in the BLE pair card. */}
      </div>

      {/* Clock-not-synced info — only shown briefly during the
          window between boot and the first successful BLE/NTP sync.
          The telemetry sampler auto-corrects the Pi's clock from the
          first Tesla state response (Tesla embeds a GPS-derived
          timestamp), so this normally disappears within 30 seconds
          of first BLE contact. */}
      {clockStatus?.show_warning && (
        <div className="rounded-lg border border-blue-500/20 bg-blue-500/[0.04] p-3 text-xs">
          <div className="flex items-start gap-2">
            <ErrorIcon className="mt-0.5 h-4 w-4 shrink-0 text-blue-400" />
            <div>
              <p className="font-semibold text-blue-300">
                Setting Pi clock from car
              </p>
              <p className="mt-1 text-[11px] leading-relaxed text-slate-400">
                The Pi's clock isn't set yet. As soon as the car responds
                to one BLE poll, the Pi will pick up the correct time from
                Tesla's GPS clock — no WiFi or NTP needed.
                <br />
                <span className="text-slate-500">
                  Tip: a $5 coin-cell battery on the Pi's BAT pin keeps
                  the clock set across reboots so this doesn't happen at all.
                </span>
              </p>
            </div>
          </div>
        </div>
      )}

      {outputOpen && (
        <TelemetryOutputPanel
          sample={latestSample}
          loading={sampleLoading}
          onRefresh={async () => {
            // Also refetch the connection pill so the user sees
            // immediate visible feedback (Last seen Xm ago updates)
            // even when the sample row itself hasn't changed.
            await fetchLatestSample()
            try {
              const res = await fetch("/api/system/ble-connected")
              const d = (await res.json()) as BleConnectedResp
              setLastSuccessTs(d?.last_success_ts || 0)
              setSampleCount10min(d?.sample_count_10min ?? 0)
              setRadioOwner(d?.radio_owner ?? null)
              setArchiving(Boolean(d?.archiving))
              setBleHealth(d?.health ?? null)
              if (d?.health?.code === "repair_required") setOutputOpen(false)
            } catch { /* ignore */ }
          }}
          radioOwner={radioOwner}
          archiving={archiving}
        />
      )}

      {/* Adapter chooser — only shown when the kernel sees more than
          one BLE adapter (i.e. the user has plugged in a USB BLE
          dongle alongside the Pi's onboard radio). Single-adapter
          systems see nothing — no clutter. */}
      {adapters && adapters.available.length > 1 && (
        <AdapterPicker
          adapters={adapters}
          switching={adapterSwitching}
          error={adapterError}
          onSwitch={switchAdapter}
        />
      )}
    </PrefCard>
  )
}

/** Live adapter picker. Only rendered when 2+ BLE adapters are
 *  detected, which in practice means the user has a USB BLE dongle
 *  plugged in alongside the Pi's onboard radio. Switching the
 *  selection writes BLE_ADAPTER to sentryusb.conf and restarts both
 *  BLE services (Tesla telemetry + iOS GATT) so the new adapter
 *  takes effect within a few seconds — no reboot. */
function AdapterPicker({
  adapters,
  switching,
  error,
  onSwitch,
}: {
  adapters: BleAdaptersResp
  switching: boolean
  error: string | null
  onSwitch: (id: string) => void
}) {
  const external = adapters.available.find((a) => a.source === "external")
  const onboard = adapters.available.find((a) => a.source === "onboard")
  const usingExternal = external && adapters.current === external.id

  return (
    <div className="rounded-lg border border-blue-500/20 bg-blue-500/[0.04] p-3 text-xs">
      <div className="mb-2 flex items-center gap-2">
        <UsbIcon className="h-3.5 w-3.5 text-blue-400" />
        <span className="font-semibold text-slate-200">Bluetooth adapter</span>
      </div>
      <p className="mb-2 text-[11px] leading-relaxed text-slate-400">
        Using a USB Bluetooth dongle instead of the Pi's built-in radio is
        more reliable — the dongle has its own antenna and isn't competing
        with Wi-Fi. The change applies to both your Tesla connection and the
        SentryUSB iOS app.
      </p>
      <div className="mb-2 flex flex-col gap-1.5">
        {adapters.available.map((a) => {
          const isCurrent = adapters.current === a.id
          return (
            <button
              key={a.id}
              onClick={() => !isCurrent && !switching && onSwitch(a.id)}
              disabled={isCurrent || switching}
              className={cn(
                "flex items-center justify-between gap-2 rounded-md border px-2.5 py-1.5 text-left transition-colors",
                isCurrent
                  ? "border-blue-500/40 bg-blue-500/15"
                  : "border-white/10 bg-white/[0.02] hover:border-white/20 hover:bg-white/[0.05]",
                switching && "opacity-50",
              )}
            >
              <span className="flex items-center gap-2">
                {a.source === "external" ? (
                  <UsbIcon className="h-3 w-3 text-blue-400" />
                ) : (
                  <MemoryIcon className="h-3 w-3 text-slate-400" />
                )}
                <span className="font-medium text-slate-200">
                  {a.source === "external" ? "USB Bluetooth dongle" : "Pi built-in Bluetooth"}
                </span>
                {a.address && (
                  <span className="text-[10px] text-slate-600">{a.address}</span>
                )}
              </span>
              {isCurrent && (
                <span className="inline-flex items-center gap-1 text-[10px] font-semibold text-blue-400">
                  <CheckIcon className="h-3 w-3" /> In use
                </span>
              )}
            </button>
          )
        })}
      </div>
      {!usingExternal && external && (
        <p className="mb-1 text-[10px] text-emerald-400/80">
          Tap the USB dongle above to start using it — recommended.
        </p>
      )}
      {usingExternal && onboard && (
        <p className="mb-1 text-[10px] text-slate-500">
          If you unplug the dongle, the Pi will switch back to its built-in
          Bluetooth automatically.
        </p>
      )}
      {switching && (
        <p className="mt-1.5 inline-flex items-center gap-1 text-[10px] text-slate-400">
          <ProgressActivityIcon className="h-3 w-3 animate-spin" /> Switching Bluetooth adapter…
        </p>
      )}
      {error && (
        <p className="mt-1.5 text-[10px] text-red-400">{error}</p>
      )}
    </div>
  )
}

/** Inline panel showing the most recent telemetry sample. Polls every
 *  5s while open via the parent component's interval. */
function TelemetryOutputPanel({
  sample,
  loading,
  onRefresh,
  radioOwner,
  archiving,
}: {
  sample: BleLatestSample | null
  loading: boolean
  onRefresh: () => void
  radioOwner: string | null
  archiving: boolean
}) {
  // Units from the shared store: temperature follows TEMPERATURE_UNIT,
  // odometer follows DRIVE_MAP_UNIT, pressure follows PRESSURE_UNIT — all
  // coherent with Settings → Display & Units (previously temp wrongly keyed
  // off the distance unit).
  const { pressureBar, tempF, km } = useUnits()
  const hasSample = sample !== null && sample.ts !== null
  // Sample is considered "stale" once it's older than 60s — visually
  // de-emphasize so the user understands they're looking at past
  // values, not a real-time read like the Tesla app shows.
  const isStale = hasSample && (sample?.seconds_ago ?? 0) > 60
  // Translate the (possibly-old) sample age into a friendlier
  // label. The raw "polled 240s ago" is true but reads like a
  // log line — these phrasings are how a non-engineer would say
  // the same thing.
  const ageLabel = sample?.seconds_ago != null ? friendlyAge(sample.seconds_ago) : null

  // "Poll now" sends SIGUSR1 to the sampler so it does a fresh
  // full state read immediately instead of waiting for the next
  // scheduled cycle. Useful for testing (just turned on Sentry,
  // just plugged in to charge, etc) and as a manual override
  // when the user wants to verify the connection works regardless
  // of what the phase machine thinks.
  const [polling, setPolling] = useState(false)
  const [pollMsg, setPollMsg] = useState<string | null>(null)
  const forcePoll = async () => {
    setPolling(true)
    setPollMsg(null)
    try {
      const res = await fetch("/api/system/ble-force-poll", { method: "POST" })
      if (!res.ok) {
        setPollMsg("Couldn't reach the Pi — try again in a moment.")
      } else {
        setPollMsg("Reading now…")
        // Give the sampler ~8s to do its thing, then refresh the
        // displayed values. Most state polls complete in 2-15s.
        setTimeout(() => {
          onRefresh()
          setPollMsg(null)
        }, 8_000)
      }
    } catch {
      setPollMsg("Couldn't reach the Pi — try again in a moment.")
    } finally {
      setPolling(false)
    }
  }

  return (
    <div className="rounded-lg border border-white/5 bg-black/20 p-3 text-xs">
      <div className="mb-2 flex items-center justify-between">
        <span className="font-semibold text-slate-300">Live data from car</span>
        <div className="flex items-center gap-2">
          {ageLabel && (
            <span className="text-[10px] text-slate-600">
              updated {ageLabel}
            </span>
          )}
          <button
            onClick={forcePoll}
            disabled={polling || loading}
            title="Ask the Pi to read your car's data right now"
            className="inline-flex items-center gap-1 rounded bg-blue-500/15 px-2 py-0.5 text-[10px] font-medium text-blue-400 hover:bg-blue-500/25 disabled:opacity-50"
          >
            {polling ? <ProgressActivityIcon className="h-3 w-3 animate-spin" /> : null}
            Poll now
          </button>
          <button
            onClick={onRefresh}
            disabled={loading}
            title="Reload the values from the Pi's database"
            className="inline-flex items-center gap-1 rounded bg-white/5 px-2 py-0.5 text-[10px] text-slate-400 hover:bg-white/10 disabled:opacity-50"
          >
            {loading ? <ProgressActivityIcon className="h-3 w-3 animate-spin" /> : null}
            Refresh
          </button>
        </div>
      </div>
      {pollMsg && (
        <p className="mb-2 text-[10px] text-blue-400/80">{pollMsg}</p>
      )}

      {!hasSample && (
        <p className="text-slate-500">
          Waiting for the first reading from your car. This usually takes
          15-30 seconds once the car is awake and within Bluetooth range.
        </p>
      )}

      {hasSample && sample && (
        <div className="space-y-1.5">
          <Row
            label="Battery"
            value={fmtPct(sample.battery_pct)}
            age={sample.field_secs_ago?.battery_pct}
          />
          {sample.odometer_mi != null && (
            <Row label="Odometer" value={fmtOdo(sample.odometer_mi, km)} />
          )}
          {sample.location_name && (
            <Row label="Location" value={sample.location_name} />
          )}
          {sample.sentry_mode && (
            <Row
              label="Sentry"
              value={sample.sentry_mode === "unknown" ? "Unknown" : sample.sentry_mode}
            />
          )}
          {sample.charging_state && (
            <Row
              label="Charging"
              value={sample.charging_state === "unknown" ? "Unknown" : sample.charging_state}
            />
          )}
          {sample.shift_state && (
            <Row
              label="Shift"
              value={sample.shift_state === "absent" ? "Not reported" : sample.shift_state}
            />
          )}
          {sample.shift_state === "Unknown" && (
            <p className="pt-1 text-[10px] text-amber-400/80">
              Shift reads <strong>Unknown</strong> — normal for a parked Tesla.
              The car powers down its drive computer shortly after you park, so
              it stops reporting the gear (this happens on both AMD- and
              Intel-MCU cars). We treat <strong>Unknown as Park</strong>: the car
              may sleep when idle, but stays awake while an archive is running.
            </p>
          )}
          <Row
            label="Interior temp"
            value={fmtTemp(sample.interior_temp_c, !tempF)}
            age={sample.field_secs_ago?.interior_temp_c}
          />
          <Row
            label="Exterior temp"
            value={fmtTemp(sample.exterior_temp_c, !tempF)}
            age={sample.field_secs_ago?.exterior_temp_c}
          />
          <Row label="HVAC" value={fmtBool(sample.hvac_on)} />
          {/* Battery cell temperature intentionally omitted: Tesla
              doesn't expose it via state-query APIs (BLE or REST).
              Only battery_heater_on (a boolean) is available. */}
          {/* TPMS — show the whole block only when at least one tire
              reading is present. Cars without TPMS, or runs where the
              `state tire-pressure` call failed, naturally hide it. */}
          {(sample.tire_fl_psi != null ||
            sample.tire_fr_psi != null ||
            sample.tire_rl_psi != null ||
            sample.tire_rr_psi != null) && (
            <>
              <div className="mt-1 border-t border-white/5 pt-1.5 text-[10px] uppercase tracking-wider text-slate-600">
                Tire pressure ({pressureBar ? "bar" : "psi"})
              </div>
              <Row label="Front left" value={fmtPressure(sample.tire_fl_psi, pressureBar)} age={sample.field_secs_ago?.tires} />
              <Row label="Front right" value={fmtPressure(sample.tire_fr_psi, pressureBar)} age={sample.field_secs_ago?.tires} />
              <Row label="Rear left" value={fmtPressure(sample.tire_rl_psi, pressureBar)} age={sample.field_secs_ago?.tires} />
              <Row label="Rear right" value={fmtPressure(sample.tire_rr_psi, pressureBar)} age={sample.field_secs_ago?.tires} />
            </>
          )}
          {/* Three mutually-exclusive context messages for the stale
              case. Priority order matters because more than one
              condition can be true at once (e.g. car is asleep AND
              keep-awake is running an archive). */}
          {/* 1. Keep-awake (archive cycle) holds the radio — pause. */}
          {isStale && radioOwner === "keep_awake" && (
            <p className="pt-1 text-[10px] text-amber-400/80">
              {archiving
                ? "Paused while we back up your dashcam clips. Live readings will resume once that finishes."
                : "Paused while another part of the Pi is using Bluetooth. Live readings will resume in a moment."}
            </p>
          )}
          {/* 2. Car is asleep — body-controller polls are landing but
              state polls aren't (by design — state polls would wake
              the car). Fresh body-controller + stale state is the
              telltale signature of Quiet mode. The orange "may have
              moved or charged" warning would be misleading here:
              a parked + asleep car isn't going anywhere. */}
          {isStale && radioOwner !== "keep_awake"
            && sample.body_controller_seconds_ago != null
            && sample.body_controller_seconds_ago < 90 && (
            <p className="pt-1 text-[10px] text-slate-500">
              Your car is asleep, so these are the last known values from {ageLabel}.
              They won't update until your car wakes up (driving, door, or
              Sentry trigger). Press <strong>Poll now</strong> above to ask
              the car for a fresh reading immediately.
            </p>
          )}
          {/* 3. Genuinely-stale: state poll old AND body-controller poll
              also old (or never happened). Something is actually
              wrong — BLE radio issue, car out of range, etc. The
              orange "may have moved" warning fits this case. */}
          {isStale && radioOwner !== "keep_awake"
            && (sample.body_controller_seconds_ago == null
                || sample.body_controller_seconds_ago >= 90) && (
            <p className="pt-1 text-[10px] text-amber-400/70">
              These values are from {ageLabel} — your car may have moved or
              charged since. The Tesla app uses a different connection (LTE)
              so small differences are normal.
            </p>
          )}
        </div>
      )}
    </div>
  )
}

// A shown value older than this (seconds) gets an inline age badge, so a
// last-known reading can't pass for live. 10 min is well past the
// ~15-30s active poll cadence, so fresh values are never flagged.
const FIELD_STALE_AFTER_SECS = 600

function Row({
  label,
  value,
  age,
}: {
  label: string
  value: string
  // Per-field age (seconds) of `value`, when known.
  age?: number | null
}) {
  const stale =
    age != null && age >= FIELD_STALE_AFTER_SECS ? formatAgo(age) : null
  return (
    <div className="flex items-baseline justify-between">
      <span className="text-slate-500">{label}</span>
      <span className={"font-mono " + (stale ? "text-slate-500" : "text-slate-300")}>
        {value}
        {stale && (
          <span
            className="ml-1.5 text-[10px] text-amber-400/80"
            title="Last-known value — not a live reading"
          >
            · {stale} ago
          </span>
        )}
      </span>
    </div>
  )
}

/** Tesla's app shows battery as a whole-number percentage; matching
 *  that avoids the "Pi says 70%, app says 69%" confusion which is
 *  really just `.toFixed(1)` faking precision on a coarse integer
 *  value. */
function fmtPct(v: number | null | undefined): string {
  return v == null ? "—" : `${Math.round(v)}%`
}

/** Turn a "seconds ago" integer into a phrase a human would say.
 *  Used for the "updated X" line so the panel reads like natural
 *  English instead of a log message. */
function friendlyAge(seconds: number): string {
  if (seconds < 5) return "just now"
  if (seconds < 60) return `${seconds} seconds ago`
  const minutes = Math.round(seconds / 60)
  if (minutes < 60) return minutes === 1 ? "1 minute ago" : `${minutes} minutes ago`
  const hours = Math.round(minutes / 60)
  if (hours < 24) return hours === 1 ? "1 hour ago" : `${hours} hours ago`
  const days = Math.round(hours / 24)
  return days === 1 ? "1 day ago" : `${days} days ago`
}

/** Backend always stores Celsius (single source of truth). Convert
 *  for display when the user prefers Fahrenheit. */
function fmtTemp(v: number | null | undefined, metric: boolean): string {
  if (v == null) return "—"
  return metric ? `${v.toFixed(1)}°C` : `${(v * 9 / 5 + 32).toFixed(1)}°F`
}

function fmtBool(v: boolean | null | undefined): string {
  if (v == null) return "—"
  return v ? "on" : "off"
}

/** Tesla reports TPMS in PSI. The display unit follows the Display & Units
 *  "Tire pressure" toggle (PRESSURE_UNIT): psi shows whole numbers, bar to
 *  two decimals. */
function fmtPressure(v: number | null | undefined, bar: boolean): string {
  if (v == null) return "—"
  return bar ? `${(v * PSI_TO_BAR).toFixed(2)} bar` : `${Math.round(v)} psi`
}

/** Odometer in miles (Tesla's native unit). When the user prefers
 *  metric, convert at display time so the source-of-truth in the DB
 *  stays single-unit. */
function fmtOdo(v: number | null | undefined, metric: boolean): string {
  if (v == null) return "—"
  return metric
    ? `${(v * 1.609344).toFixed(1)} km`
    : `${v.toFixed(1)} mi`
}

/** Compact relative-time formatter for the live indicator. */
function formatAgo(seconds: number): string {
  if (seconds < 60) return `${seconds}s`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m`
  return `${Math.floor(seconds / 3600)}h`
}
