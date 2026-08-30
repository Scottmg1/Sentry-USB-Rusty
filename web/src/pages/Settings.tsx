import { useEffect, useRef, useState, lazy, Suspense } from "react"
import { useSearchParams } from "react-router-dom"
import {
  CachedIcon,
  PowerOffIcon,
  ProgressActivityIcon,
  RotateLeftIcon,
  SpeedIcon,
  StethoscopeIcon,
} from "@/components/icons"
import { api } from "@/lib/api"
import { HeaderStrip } from "@/components/settings/HeaderStrip"
import { ActionsRail, type ActionChipProps } from "@/components/settings/ActionsRail"
import { TabBar } from "@/components/ui/TabBar"
import { useVersion } from "@/hooks/useVersion"
import type { RawConfigEntry } from "@/components/settings/sections/RawConfigEditor"
import type { PiStatus } from "@/lib/api"

// Lazy tab chunks — only the active tab pays for its bundle. Visiting
// /settings without clicking a tab pulls just the shell + Device
// (default). Each tab module exports a named component, so wrap with
// `.then(m => ({ default: m.X }))` to satisfy React.lazy's
// default-export contract.
const DeviceTab = lazy(() => import("@/pages/settings/DeviceTab").then(m => ({ default: m.DeviceTab })))
const NetworkTab = lazy(() => import("@/pages/settings/NetworkTab").then(m => ({ default: m.NetworkTab })))
const NotificationsTab = lazy(() => import("@/pages/settings/NotificationsTab").then(m => ({ default: m.NotificationsTab })))
const SystemTab = lazy(() => import("@/pages/settings/SystemTab").then(m => ({ default: m.SystemTab })))

// Modals are only mounted while open — defer their bundles entirely
// until the user opens them. SetupWizard alone pulls in several
// step components + the BLE pairing flow.
const SetupWizard = lazy(() => import("@/components/setup/SetupWizard").then(m => ({ default: m.SetupWizard })))
const RawConfigEditor = lazy(() => import("@/components/settings/sections/RawConfigEditor").then(m => ({ default: m.RawConfigEditor })))
const HealthCheckModal = lazy(() => import("@/components/settings/sections/HealthCheckModal").then(m => ({ default: m.HealthCheckModal })))
const SpeedTestModal = lazy(() => import("@/components/settings/sections/SpeedTestModal").then(m => ({ default: m.SpeedTestModal })))

// Four task-based groups (consolidated from the original seven):
//   Device                  — keep-awake, units, keep-accessory, software updates
//   Car & Network           — WiFi/Eth, Tesla BLE, Away Mode, SentryCloud
//   Notifications & Community — mobile push + community features (wraps, chimes)
//   System                  — backups/export/raw-config, setup wizard, privacy
const TABS = [
  "Device",
  "Car & Network",
  "Notifications & Community",
  "System",
] as const
type TabName = (typeof TABS)[number]

function isTab(s: string | null): s is TabName {
  return !!s && (TABS as readonly string[]).includes(s)
}

export default function Settings() {
  const [params, setParams] = useSearchParams()
  const activeTab: TabName = isTab(params.get("tab")) ? (params.get("tab") as TabName) : "Device"

  const [status, setStatus] = useState<PiStatus | null>(null)
  const [piConfig, setPiConfig] = useState<{
    uses_ble?: string
    SENTRYUSB_HOSTNAME?: string
  } | null>(null)
  const [confirmReboot, setConfirmReboot] = useState(false)
  const [drivesConnected, setDrivesConnected] = useState<boolean | null>(null)
  // Local 1s tick so the header strip uptime advances between status polls
  // (status itself only refreshes every 4s — too jumpy for a wall clock).
  const [tickOffset, setTickOffset] = useState(0)

  // Modal state
  const [wizardOpen, setWizardOpen] = useState(false)
  const [wizardInitialData, setWizardInitialData] = useState<
    Record<string, string> | undefined
  >(undefined)
  const [wizardInitialStep, setWizardInitialStep] = useState<string | undefined>(undefined)
  const handledWizardDeepLink = useRef<string | null>(null)
  const [rawConfigOpen, setRawConfigOpen] = useState(false)
  const [rawConfig, setRawConfig] = useState<Record<string, RawConfigEntry> | null>(null)
  const [healthOpen, setHealthOpen] = useState(false)
  const [speedOpen, setSpeedOpen] = useState(false)

  // Version is included in the Export Config file header so the receiver
  // knows which schema produced it. HeaderStrip fetches its own copy via
  // the same hook — the underlying request is cheap and the hook caches
  // nothing across mounts, but /api/system/version itself is a static
  // string so duplicate calls are harmless.
  const version = useVersion()

  // Status poll (drives the actions rail USB toggle + header strip uptime)
  useEffect(() => {
    let mounted = true
    async function poll() {
      try {
        const data = await api.getStatus()
        if (mounted) {
          setStatus(data)
          setDrivesConnected(data.drives_active === "yes")
          setTickOffset(0) // reset local tick when we get a fresh server value
        }
      } catch {
        /* ignore */
      }
    }
    poll()
    const id = setInterval(poll, 4000)
    const tickId = setInterval(() => setTickOffset((t) => t + 1), 1000)
    return () => {
      mounted = false
      clearInterval(id)
      clearInterval(tickId)
    }
  }, [])

  // Pi config (uses_ble, SENTRYUSB_HOSTNAME). SBC model now comes from the
  // status payload (which already detects Pi 5 reliably) — the previous
  // rtc-status flag only flipped to is_pi5=true when a battery was present,
  // so a Pi 5 without an RTC battery was mis-labelled as "Pi 4 / earlier".
  useEffect(() => {
    fetch("/api/config")
      .then((r) => r.json())
      .then((data) => setPiConfig(data))
      .catch(() => {})
  }, [])

  const sbc = status?.sbc_model || null
  // /api/config returns each key as { value, active } OR as a raw string,
  // depending on shape — handle both.
  const hostnameEntry = piConfig?.SENTRYUSB_HOSTNAME as
    | { value?: string; active?: boolean }
    | string
    | undefined
  const hostname =
    typeof hostnameEntry === "string"
      ? hostnameEntry
      : hostnameEntry?.active
      ? hostnameEntry.value || null
      : null

  function setTab(next: TabName) {
    const p = new URLSearchParams(params)
    p.set("tab", next)
    setParams(p, { replace: true })
  }

  async function handleReboot(): Promise<string | void> {
    // First press arms the confirm; the parent re-renders the chip's label
    // ("Restart Pi" → "Confirm Restart"). Returning "confirm" tells the chip
    // not to flash a success state — the label change is the feedback.
    if (!confirmReboot) {
      setConfirmReboot(true)
      setTimeout(() => setConfirmReboot(false), 10000)
      return "confirm"
    }
    const res = await fetch("/api/system/reboot", { method: "POST" })
    setConfirmReboot(false)
    if (!res.ok) throw new Error("Reboot failed")
    return "Rebooting…"
  }

  async function handleToggleDrives(): Promise<string> {
    const res = await fetch("/api/system/toggle-drives", { method: "POST" })
    if (!res.ok) throw new Error("Toggle failed")
    // Eagerly refresh status so the chip label updates to the new state
    // ("USB · Connected" / "USB · Disconnected") on the next render.
    try {
      const data = await api.getStatus()
      setDrivesConnected(data.drives_active === "yes")
    } catch {
      /* non-critical */
    }
    return "Toggled"
  }

  async function handleArchiveSync(): Promise<string> {
    const res = await fetch("/api/system/trigger-sync", { method: "POST" })
    if (!res.ok) throw new Error("Sync failed")
    return "Triggered"
  }

  async function handleOpenRawConfig() {
    try {
      const res = await fetch("/api/setup/config")
      if (!res.ok) return
      const data = await res.json()
      setRawConfig(data)
      setRawConfigOpen(true)
    } catch {
      /* ignore */
    }
  }

  async function handleOpenWizard(initialStepId?: string) {
    setWizardInitialStep(initialStepId)
    try {
      const res = await fetch("/api/setup/config")
      if (res.ok) {
        const data = await res.json()
        const flat: Record<string, string> = {}
        for (const [k, v] of Object.entries(data)) {
          const entry = v as { value: string; active: boolean }
          if (entry.active) flat[k] = entry.value
        }
        setWizardInitialData(flat)
      }
    } catch {
      /* ignore */
    }
    setWizardOpen(true)
  }

  const requestedWizardStep = params.get("wizard")
  useEffect(() => {
    if (!requestedWizardStep || handledWizardDeepLink.current === requestedWizardStep) return
    handledWizardDeepLink.current = requestedWizardStep
    void handleOpenWizard(requestedWizardStep)
    const next = new URLSearchParams(params)
    next.delete("wizard")
    setParams(next, { replace: true })
  }, [requestedWizardStep, params, setParams])

  const actions: ActionChipProps[] = [
    {
      icon: CachedIcon,
      label: "Archive Sync",
      onClick: handleArchiveSync,
    },
    {
      icon: StethoscopeIcon,
      label: "Health Check",
      onClick: () => setHealthOpen(true),
    },
    {
      icon: SpeedIcon,
      label: "Speed Test",
      onClick: () => setSpeedOpen(true),
    },
    {
      icon: PowerOffIcon,
      label:
        drivesConnected === null
          ? "Toggle USB"
          : drivesConnected
          ? "USB · Connected"
          : "USB · Disconnected",
      onClick: handleToggleDrives,
    },
  ]
  const dangerActions: ActionChipProps[] = [
    {
      icon: RotateLeftIcon,
      label: confirmReboot ? "Confirm Restart" : "Restart Pi",
      variant: "danger",
      onClick: handleReboot,
    },
  ]

  const uptimeSec = status ? parseFloat(status.uptime) + tickOffset : null

  // ⚠️ Mobile / tab-bar — switch to scrollable variant under 640px.
  const [isMobile, setIsMobile] = useState(
    typeof window !== "undefined" && window.innerWidth < 640
  )
  useEffect(() => {
    const onResize = () => setIsMobile(window.innerWidth < 640)
    window.addEventListener("resize", onResize)
    return () => window.removeEventListener("resize", onResize)
  }, [])

  return (
    <div className="space-y-3">
      <HeaderStrip
        hostname={hostname}
        sbc={sbc}
        uptimeSec={uptimeSec}
      />

      <ActionsRail actions={actions} danger={dangerActions} />

      <TabBar tabs={TABS} active={activeTab} onSelect={setTab} scrollable={isMobile} />

      <Suspense fallback={<TabFallback />}>
        {activeTab === "Device" && <DeviceTab onOpenWizard={() => { void handleOpenWizard() }} />}
        {activeTab === "Car & Network" && (
          <NetworkTab status={status} onOpenWizard={() => { void handleOpenWizard() }} />
        )}
        {activeTab === "Notifications & Community" && <NotificationsTab />}
        {activeTab === "System" && (
          <SystemTab
            onOpenRawConfig={handleOpenRawConfig}
            onOpenWizard={() => { void handleOpenWizard() }}
            version={version}
            hostname={hostname}
          />
        )}
      </Suspense>

      {/* Modals — each wrapped in its own Suspense so a slow chunk for
          one doesn't block the rest of the page. */}
      {wizardOpen && (
        <Suspense fallback={null}>
          <SetupWizard
            initialData={wizardInitialData}
            initialStepId={wizardInitialStep}
            onClose={() => {
              setWizardOpen(false)
              setWizardInitialData(undefined)
              setWizardInitialStep(undefined)
            }}
          />
        </Suspense>
      )}
      {rawConfigOpen && rawConfig && (
        <Suspense fallback={null}>
          <RawConfigEditor
            config={rawConfig}
            onClose={() => {
              setRawConfigOpen(false)
              setRawConfig(null)
            }}
          />
        </Suspense>
      )}
      {healthOpen && (
        <Suspense fallback={null}>
          <HealthCheckModal onClose={() => setHealthOpen(false)} />
        </Suspense>
      )}
      {speedOpen && (
        <Suspense fallback={null}>
          <SpeedTestModal onClose={() => setSpeedOpen(false)} />
        </Suspense>
      )}
    </div>
  )
}

function TabFallback() {
  return (
    <div className="flex h-32 items-center justify-center">
      <ProgressActivityIcon className="h-5 w-5 animate-spin text-slate-500" />
    </div>
  )
}
