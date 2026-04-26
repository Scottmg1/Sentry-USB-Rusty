import { useState, useCallback, useEffect, useRef } from "react"
import { ChevronLeft, ChevronRight, Check, Loader2, AlertCircle, AlertTriangle } from "lucide-react"
import { cn } from "@/lib/utils"
import { SetupProgress } from "./SetupProgress"
import { WelcomeStep } from "./steps/WelcomeStep"
import { NetworkStep } from "./steps/NetworkStep"
import { StorageStep } from "./steps/StorageStep"
import { CommunityStep } from "./steps/CommunityStep"
import { ArchiveStep } from "./steps/ArchiveStep"
import { KeepAwakeStep } from "./steps/KeepAwakeStep"
import { NotificationsStep } from "./steps/NotificationsStep"
import { SecurityStep } from "./steps/SecurityStep"
import { AdvancedStep } from "./steps/AdvancedStep"
import { ReviewStep } from "./steps/ReviewStep"

export interface SetupFormData {
  [key: string]: string
}

interface StepDef {
  id: string
  title: string
  component: React.ComponentType<StepProps>
}

export interface StepProps {
  data: SetupFormData
  onChange: (key: string, value: string) => void
  onBatchChange: (updates: Record<string, string>) => void
}

function networkError(data: SetupFormData): string | null {
  if (data.AP_SSID && (data.AP_PASS ?? "").length < 8)
    return "WiFi Access Point password must be at least 8 characters."
  return null
}

function storageError(data: SetupFormData): string | null {
  // CAM_SIZE = 0 silently disables the dashcam drive — which is the entire
  // point of this device — and downstream phases happily proceed against
  // an empty cam disk image, leaving the user with a "complete" install
  // that does nothing. Treat it as a hard error so the user sees the
  // mistake before kicking off setup.
  const cam = parseFloat(data.CAM_SIZE ?? "0")
  if (!Number.isFinite(cam) || cam <= 0) {
    return "Dashcam drive size must be greater than 0 GB."
  }
  return null
}

function archiveError(data: SetupFormData): string | null {
  const system = data.ARCHIVE_SYSTEM ?? "cifs"
  if (system === "none") return null
  if (system === "cifs") {
    if (!data.ARCHIVE_SERVER?.trim()) return "Archive Server is required."
    if (!data.SHARE_NAME?.trim()) return "Share Name is required."
    if (!data.SHARE_USER?.trim()) return "Username is required."
    if (!data.SHARE_PASSWORD?.trim()) return "Password is required."
  } else if (system === "rsync") {
    if (!data.RSYNC_SERVER?.trim()) return "Server is required."
    if (!data.RSYNC_USER?.trim()) return "Username is required."
    if (!data.RSYNC_PATH?.trim()) return "Remote Path is required."
  } else if (system === "rclone") {
    if (!data.RCLONE_DRIVE?.trim()) return "Remote Name is required."
    if (!data.RCLONE_PATH?.trim()) return "Remote Path is required."
  } else if (system === "nfs") {
    if (!data.ARCHIVE_SERVER?.trim()) return "NFS Server is required."
    if (!data.SHARE_NAME?.trim()) return "Export Path is required."
  }
  return null
}

function keepAwakeError(data: SetupFormData): string | null {
  const method = data._KEEP_AWAKE_METHOD
    || (data.TESLA_BLE_VIN ? "ble"
      : data.TESLAFI_API_TOKEN ? "teslafi"
        : data.TESSIE_API_TOKEN ? "tessie"
          : data.KEEP_AWAKE_WEBHOOK_URL ? "webhook"
            : "none")
  if (method === "none") return null
  if (method === "ble" && !data.TESLA_BLE_VIN?.trim()) return "Vehicle VIN is required for Bluetooth LE."
  if (method === "teslafi" && !data.TESLAFI_API_TOKEN?.trim()) return "TeslaFi API Token is required."
  if (method === "tessie" && !data.TESSIE_API_TOKEN?.trim()) return "Tessie API Token is required."
  if (method === "tessie" && !data.TESSIE_VIN?.trim()) return "Vehicle VIN is required for Tessie."
  if (method === "webhook") {
    const url = data.KEEP_AWAKE_WEBHOOK_URL?.trim() ?? ""
    if (!url) return "Webhook URL is required."
    // Schemeless URLs ("homeassistant.local/api/webhook/foo") get curl-
    // interpreted as a file path at runtime, then the keep-awake job
    // silently does nothing. Catch it before the user submits.
    if (!/^https?:\/\//i.test(url)) return "Webhook URL must start with http:// or https://."
  }
  if (!data.SENTRY_CASE) return "Sentry Mode behavior must be selected."
  return null
}

function notificationsError(data: SetupFormData): string | null {
  const requiredPerProvider: [string, string, string[]][] = [
    ["Pushover", "PUSHOVER_ENABLED", ["PUSHOVER_USER_KEY", "PUSHOVER_APP_KEY"]],
    ["Gotify", "GOTIFY_ENABLED", ["GOTIFY_DOMAIN", "GOTIFY_APP_TOKEN"]],
    ["Discord", "DISCORD_ENABLED", ["DISCORD_WEBHOOK_URL"]],
    ["Telegram", "TELEGRAM_ENABLED", ["TELEGRAM_CHAT_ID", "TELEGRAM_BOT_TOKEN"]],
    ["IFTTT", "IFTTT_ENABLED", ["IFTTT_EVENT_NAME", "IFTTT_KEY"]],
    ["Slack", "SLACK_ENABLED", ["SLACK_WEBHOOK_URL"]],
    ["Signal", "SIGNAL_ENABLED", ["SIGNAL_URL", "SIGNAL_FROM_NUM", "SIGNAL_TO_NUM"]],
    ["Matrix", "MATRIX_ENABLED", ["MATRIX_SERVER_URL", "MATRIX_USERNAME", "MATRIX_PASSWORD", "MATRIX_ROOM"]],
    ["AWS SNS", "SNS_ENABLED", ["AWS_REGION", "AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SNS_TOPIC_ARN"]],
    ["Webhook", "WEBHOOK_ENABLED", ["WEBHOOK_URL"]],
  ]
  for (const [label, enableField, fields] of requiredPerProvider) {
    if (data[enableField] === "true" && fields.some(f => !data[f]?.trim()))
      return `Complete all required fields for ${label}.`
  }
  return null
}

function securityError(data: SetupFormData): string | null {
  // Both fields must be set together, or both must be empty (auth disabled).
  // Filling only one silently breaks login — username-only enables the auth
  // gate but leaves the user unable to authenticate; password-only is
  // ignored entirely because the backend keys auth on having a username.
  // Validate both directions so the user can't escape the Security step
  // in a half-configured state that locks them out post-setup.
  const u = data.WEB_USERNAME?.trim() ?? ""
  const p = data.WEB_PASSWORD?.trim() ?? ""
  if (u && !p) return "Web Password is required when a Web Username is set."
  if (p && !u) return "Web Username is required when a Web Password is set."
  return null
}

function getStepError(stepIdx: number, data: SetupFormData): string | null {
  switch (stepIdx) {
    case 1: return networkError(data)
    case 2: return storageError(data)
    // case 3 is the Community step — no validation needed (both can be unchecked)
    case 4: return archiveError(data)
    case 5: return keepAwakeError(data)
    case 6: return notificationsError(data)
    case 7: return securityError(data)
    default: return null
  }
}

// ── Destructive change detection ──
// These settings cause data loss when changed because the underlying disk
// images must be deleted and recreated with the new size/filesystem.
const DESTRUCTIVE_SIZE_KEYS: Record<string, string> = {
  CAM_SIZE: "Dashcam drive (clips & snapshots)",
  MUSIC_SIZE: "Music drive",
  LIGHTSHOW_SIZE: "Lightshow drive",
  BOOMBOX_SIZE: "Boombox drive",
  WRAPS_SIZE: "Wraps drive",
}

interface DestructiveChange {
  key: string
  label: string
  reason: string
}

function normalizeSizeValue(val: string | undefined): string {
  if (!val) return "0"
  return val.replace(/G$/i, "").trim() || "0"
}

function detectDestructiveChanges(
  current: SetupFormData,
  original: SetupFormData | undefined,
): DestructiveChange[] {
  // No original config = first-time setup, nothing to lose
  if (!original) return []

  const changes: DestructiveChange[] = []

  // Check if filesystem type changed — this forces ALL drives to be recreated
  const exfatChanged = (current.USE_EXFAT ?? "true") !== (original.USE_EXFAT ?? "true")
  if (exfatChanged) {
    const from = original.USE_EXFAT === "true" ? "exFAT" : "FAT32"
    const to = current.USE_EXFAT === "true" ? "exFAT" : "FAT32"
    changes.push({
      key: "USE_EXFAT",
      label: "All drives",
      reason: `Filesystem type changed from ${from} to ${to} — all drive images will be recreated`,
    })
    // When exFAT changes, all drives are affected so we don't need to list individual size changes
    return changes
  }

  // Check individual size changes
  for (const [key, label] of Object.entries(DESTRUCTIVE_SIZE_KEYS)) {
    const newVal = normalizeSizeValue(current[key])
    const oldVal = normalizeSizeValue(original[key])
    if (newVal !== oldVal) {
      changes.push({
        key,
        label,
        reason: `Size changed from ${oldVal || "0"}G to ${newVal}G — drive image will be recreated`,
      })
    }
  }

  return changes
}

const steps: StepDef[] = [
  { id: "welcome", title: "Welcome", component: WelcomeStep },
  { id: "network", title: "Network", component: NetworkStep },
  { id: "storage", title: "Storage", component: StorageStep },
  { id: "community", title: "Community", component: CommunityStep },
  { id: "archive", title: "Archive", component: ArchiveStep },
  { id: "keepawake", title: "Keep Awake", component: KeepAwakeStep },
  { id: "notifications", title: "Notifications", component: NotificationsStep },
  { id: "security", title: "Security", component: SecurityStep },
  { id: "advanced", title: "Advanced", component: AdvancedStep },
  { id: "review", title: "Review", component: ReviewStep },
]

interface SetupWizardProps {
  initialData?: SetupFormData
  onClose: () => void
}

type SetupPhase = "wizard" | "applying" | "running" | "rebooting" | "finalizing" | "complete" | "error"

export function SetupWizard({ initialData, onClose }: SetupWizardProps) {
  const [currentStep, setCurrentStep] = useState(0)
  // Defaults for fields that appear pre-selected in the UI but may not exist
  // in the config file yet. Without this, untouched defaults never get saved.
  const defaults: SetupFormData = {
    CAM_SIZE: "40",
    ARCHIVE_SYSTEM: "cifs",
    TEMPERATURE_UNIT: "C",
    ARCHIVE_SAVEDCLIPS: "true",
    ARCHIVE_SENTRYCLIPS: "true",
    ARCHIVE_RECENTCLIPS: "true",
    ARCHIVE_TRACKMODECLIPS: "true",
    DRIVE_MAP_ENABLED: "true",
    DRIVE_MAP_WHILE_AWAY: "true",
    DRIVE_MAP_UNIT: "mi",
    TEMPERATURE_POSTARCHIVE: "true",
    USE_EXFAT: "true",
    RTC_BATTERY_ENABLED: "false",
    RTC_TRICKLE_CHARGE: "false",
  }
  const [formData, setFormData] = useState<SetupFormData>({ ...defaults, ...(initialData ?? {}) })
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const [phase, setPhase] = useState<SetupPhase>("wizard")
  const [setupMessage, setSetupMessage] = useState("")
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null)
  // Snapshot of the config as it was when the wizard opened (for detecting destructive changes)
  const originalDataRef = useRef<SetupFormData | undefined>(initialData)
  const [destructiveWarning, setDestructiveWarning] = useState<DestructiveChange[] | null>(null)
  // Tracks whether the user restored from a backup (affects warning dialog wording)
  const isRestoreFlow = useRef(false)

  const handleChange = useCallback((key: string, value: string) => {
    setFormData((prev) => ({ ...prev, [key]: value }))
  }, [])

  const handleBatchChange = useCallback((updates: Record<string, string>) => {
    setFormData((prev) => ({ ...prev, ...updates }))
    // When restoring from a backup, update the baseline so destructive change
    // detection compares against the backup values (not the fresh SD card defaults).
    // The WelcomeStep sets _restore_baseline when a backup restore completes.
    if (updates._restore_baseline === "true") {
      const baseline = { ...updates }
      delete baseline._restore_baseline
      originalDataRef.current = { ...(originalDataRef.current ?? {}), ...baseline }
      isRestoreFlow.current = true
    }
  }, [])

  // Hydrate Community Features prefs from the preference store on mount.
  // initialData (passed by callers) only carries sentryusb.conf keys, so the
  // pref-store-backed _community_* keys must be loaded separately. Caller-
  // supplied values in initialData (e.g., from the Settings Wraps toggle)
  // take precedence and are not overwritten here.
  useEffect(() => {
    let cancelled = false
    Promise.all([
      fetch("/api/config/preference?key=community_wraps_enabled").then((r) => r.json()).catch(() => null),
      fetch("/api/config/preference?key=community_chimes_enabled").then((r) => r.json()).catch(() => null),
    ]).then(([wraps, chimes]) => {
      if (cancelled) return
      const updates: Record<string, string> = {}
      if (wraps && wraps.value !== null && wraps.value !== undefined) {
        updates._community_wraps_enabled = wraps.value === "disabled" ? "false" : "true"
      }
      if (chimes && chimes.value !== null && chimes.value !== undefined) {
        updates._community_chimes_enabled = chimes.value === "disabled" ? "false" : "true"
      }
      if (Object.keys(updates).length === 0) return
      setFormData((prev) => {
        const next = { ...prev }
        for (const [k, v] of Object.entries(updates)) {
          if (next[k] === undefined) next[k] = v
        }
        return next
      })
    })
    return () => { cancelled = true }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  // Poll setup status while running
  useEffect(() => {
    if (phase !== "running" && phase !== "rebooting") return
    pollRef.current = setInterval(async () => {
      try {
        const res = await fetch("/api/setup/status")
        const data = await res.json()
        if (data.setup_finished) {
          // Setup scripts are done — the Pi will do a final reboot.
          // Transition to "finalizing" which keeps the spinner and
          // waits for the server to come back before showing dashboard.
          setPhase("finalizing")
          setSetupMessage("Sentry USB has finished setting up. The device is now rebooting one last time...")
          if (pollRef.current) clearInterval(pollRef.current)
        } else if (data.setup_running && phase === "rebooting") {
          // Server is back and setup is still going — restore live progress view.
          // This recovers from transient blips (service restart, heavy I/O, etc.)
          // that previously left the UI permanently stuck in "rebooting".
          setPhase("running")
          setSetupMessage("Setup is running. The device will reboot several times during this process — this is normal.")
        } else if (!data.setup_running && phase === "running") {
          setPhase("rebooting")
          setSetupMessage("System is rebooting to continue setup. This page will reconnect automatically.")
        }
      } catch {
        // Server unreachable — likely rebooting, which is expected
        if (phase !== "rebooting") {
          setPhase("rebooting")
          setSetupMessage("Waiting for device to come back online after reboot...")
        }
      }
    }, 3000)
    return () => { if (pollRef.current) clearInterval(pollRef.current) }
  }, [phase])

  // Poll during finalizing — wait for server to go DOWN then come back UP.
  // Without the wentDown gate, the first poll can succeed while the Pi is
  // still shutting down (exec reboot takes a few seconds to kill the server),
  // causing a premature "Setup Complete!" before the Pi has actually rebooted.
  useEffect(() => {
    if (phase !== "finalizing") return
    let wentDown = false
    const poll = setInterval(async () => {
      try {
        const res = await fetch("/api/setup/status")
        if (res.ok && wentDown) {
          // Server is back up after confirmed reboot
          setPhase("complete")
          setSetupMessage("Setup completed successfully! Your device is ready.")
          clearInterval(poll)
        }
      } catch {
        // Server unreachable — Pi is rebooting
        wentDown = true
        setSetupMessage("Waiting for Sentry USB to come back online after final reboot...")
      }
    }, 3000)
    return () => clearInterval(poll)
  }, [phase])

  // Also listen to WebSocket for real-time updates (auto-reconnect on drop)
  useEffect(() => {
    if (phase !== "running" && phase !== "applying" && phase !== "rebooting") return
    let ws: WebSocket | null = null
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null
    let backoff = 2000
    let cancelled = false

    function connect() {
      if (cancelled) return
      try {
        const protocol = window.location.protocol === "https:" ? "wss:" : "ws:"
        ws = new WebSocket(`${protocol}//${window.location.host}/api/ws`)
        ws.onopen = () => { backoff = 2000 }
        ws.onmessage = (event) => {
          try {
            const msg = JSON.parse(event.data)
            if (msg.type === "setup_status") {
              const d = msg.data
              if (d.status === "running") {
                setPhase("running")
                setSetupMessage("Running setup... This may take several minutes.")
              } else if (d.status === "complete") {
                setPhase("finalizing")
                setSetupMessage("Sentry USB has finished setting up. The device is now rebooting one last time...")
              } else if (d.status === "rebooting") {
                setPhase("rebooting")
                setSetupMessage(d.message || "System is rebooting to continue setup...")
              } else if (d.status === "error") {
                setPhase("error")
                setSetupMessage(d.error || "Setup failed. Check logs for details.")
              }
            }
          } catch { /* ignore parse errors */ }
        }
        ws.onclose = () => {
          if (cancelled) return
          reconnectTimer = setTimeout(() => {
            backoff = Math.min(backoff * 1.5, 15000)
            connect()
          }, backoff)
        }
      } catch { /* ws not available */ }
    }

    connect()
    return () => {
      cancelled = true
      if (reconnectTimer) clearTimeout(reconnectTimer)
      ws?.close()
    }
  }, [phase])

  const StepComponent = steps[currentStep].component
  const currentStepError = getStepError(currentStep, formData)

  // Core apply logic — sends the given data to the server and triggers setup.
  async function doApply(dataToSave: SetupFormData) {
    setSaving(true)
    setSaveError(null)
    try {
      const sizeFields = new Set(["CAM_SIZE", "MUSIC_SIZE", "LIGHTSHOW_SIZE", "BOOMBOX_SIZE", "WRAPS_SIZE"])
      const configData = Object.fromEntries(
        Object.entries(dataToSave)
          .filter(([k, v]) => !k.startsWith("_") && v !== "")
          .map(([k, v]) => {
            if (sizeFields.has(k) && /^\d+$/.test(v)) {
              return [k, v + "G"]
            }
            if ((k === "TEMPERATURE_WARNING" || k === "TEMPERATURE_CAUTION") && v && !v.includes("000")) {
              const num = parseFloat(v)
              if (!isNaN(num)) return [k, String(Math.round(num * 1000))]
            }
            return [k, v]
          })
      )
      const res = await fetch("/api/setup/config", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(configData),
      })
      if (!res.ok) throw new Error("Failed to save configuration")

      // Save backup location preference (stored separately from config)
      if (dataToSave._BACKUP_LOCATION) {
        await fetch("/api/config/preference", {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ key: "backup_location", value: dataToSave._BACKUP_LOCATION }),
        }).catch(() => {}) // best-effort
      }

      // Save Community Features prefs (Wraps / Lock Chimes opt-in)
      const communityPrefPuts: Promise<unknown>[] = []
      if (dataToSave._community_wraps_enabled !== undefined) {
        communityPrefPuts.push(fetch("/api/config/preference", {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            key: "community_wraps_enabled",
            value: dataToSave._community_wraps_enabled === "true" ? "enabled" : "disabled",
          }),
        }).catch(() => {}))
      }
      if (dataToSave._community_chimes_enabled !== undefined) {
        communityPrefPuts.push(fetch("/api/config/preference", {
          method: "PUT",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            key: "community_chimes_enabled",
            value: dataToSave._community_chimes_enabled === "true" ? "enabled" : "disabled",
          }),
        }).catch(() => {}))
      }
      if (communityPrefPuts.length > 0) {
        await Promise.all(communityPrefPuts)
        window.dispatchEvent(new CustomEvent("community-prefs-changed"))
      }

      setPhase("applying")
      setSetupMessage("Configuration saved. Starting setup...")

      const runRes = await fetch("/api/setup/run", { method: "POST" })
      if (!runRes.ok) {
        const err = await runRes.json()
        throw new Error(err.error || "Failed to start setup")
      }

      setPhase("running")
      setSetupMessage("Setup is running. The device will reboot several times during this process — this is normal.")
    } catch (err) {
      setSaveError(err instanceof Error ? err.message : "Unknown error")
      setPhase("wizard")
    } finally {
      setSaving(false)
    }
  }

  // Called when user clicks "Apply & Run Setup" — checks for destructive changes first.
  async function handleApply() {
    const firstInvalidIdx = steps.findIndex((_, i) => getStepError(i, formData) !== null)
    if (firstInvalidIdx !== -1) {
      setCurrentStep(firstInvalidIdx)
      setSaveError(getStepError(firstInvalidIdx, formData))
      return
    }

    const changes = detectDestructiveChanges(formData, originalDataRef.current)
    if (changes.length > 0) {
      setDestructiveWarning(changes)
      return
    }

    doApply(formData)
  }

  // User confirmed: apply everything including destructive changes.
  function handleApplyAll() {
    setDestructiveWarning(null)
    doApply(formData)
  }

  // User chose to skip destructive changes: revert those fields to original values.
  function handleSkipDestructive() {
    if (!destructiveWarning || !originalDataRef.current) return
    const safeData = { ...formData }
    for (const change of destructiveWarning) {
      if (change.key === "USE_EXFAT") {
        // Revert filesystem type AND all size fields (since exFAT change affects all)
        safeData.USE_EXFAT = originalDataRef.current.USE_EXFAT ?? "true"
      } else {
        safeData[change.key] = originalDataRef.current[change.key] ?? ""
      }
    }
    setDestructiveWarning(null)
    doApply(safeData)
  }

  const isLast = currentStep === steps.length - 1
  const isFirst = currentStep === 0

  // ── Destructive change warning dialog ──
  if (destructiveWarning) {
    return (
      <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
        <div className="glass-card flex w-full max-w-lg flex-col gap-5 p-8">
          <div className="flex items-start gap-4">
            <div className="flex h-12 w-12 shrink-0 items-center justify-center rounded-full bg-amber-500/20">
              <AlertTriangle className="h-6 w-6 text-amber-400" />
            </div>
            <div>
              <h2 className="text-lg font-semibold text-slate-100">
                {isRestoreFlow.current ? "Drive Sizes Changed From Backup" : "Data Will Be Deleted"}
              </h2>
              <p className="mt-1 text-sm text-slate-400">
                {isRestoreFlow.current
                  ? "You changed drive sizes from what was in your backup. This will cause the SSD to be reformatted, which will erase all existing footage and data on the affected drives."
                  : "The following changes require drive images to be recreated. All data on the affected drives will be permanently lost."}
              </p>
            </div>
          </div>

          <div className="rounded-lg border border-amber-500/20 bg-amber-500/5 p-4">
            <ul className="space-y-3">
              {destructiveWarning.map((change) => (
                <li key={change.key} className="flex flex-col gap-0.5">
                  <span className="text-sm font-medium text-slate-200">{change.label}</span>
                  <span className="text-xs text-slate-400">{change.reason}</span>
                </li>
              ))}
            </ul>
          </div>

          <div className="flex flex-col gap-2 sm:flex-row sm:justify-end">
            <button
              onClick={() => setDestructiveWarning(null)}
              className="rounded-lg border border-white/10 bg-white/5 px-4 py-2 text-sm font-medium text-slate-300 transition-colors hover:bg-white/10"
            >
              Cancel
            </button>
            <button
              onClick={handleSkipDestructive}
              className="rounded-lg border border-blue-500/30 bg-blue-500/10 px-4 py-2 text-sm font-medium text-blue-400 transition-colors hover:bg-blue-500/20"
            >
              {isRestoreFlow.current ? "Restore Backup Sizes" : "Skip Data-Affecting Changes"}
            </button>
            <button
              onClick={handleApplyAll}
              className="rounded-lg bg-red-500 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-red-600"
            >
              {isRestoreFlow.current ? "Continue & Reformat" : "Delete Data & Apply All"}
            </button>
          </div>
        </div>
      </div>
    )
  }

  // ── Progress screen (shown after Apply) ──
  if (phase !== "wizard") {
    const isInProgress = phase === "applying" || phase === "running" || phase === "rebooting" || phase === "finalizing"
    return (
      <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
        <div className="glass-card flex w-full max-w-2xl flex-col gap-6 p-8 lg:max-w-5xl">
          {isInProgress ? (
            <>
              <div className="text-center">
                <h2 className="text-xl font-semibold text-slate-100">
                  {phase === "finalizing" ? "Almost Done!" : "Setting Up Sentry USB"}
                </h2>
                <p className="mt-2 text-sm text-slate-400">{setupMessage}</p>
                {phase !== "finalizing" && (
                  <p className="mt-2 text-xs text-slate-600">
                    The device will reboot multiple times — this is normal. Do not power off.
                  </p>
                )}
                {phase === "finalizing" && (
                  <p className="mt-2 text-xs text-slate-600">
                    Performing final reboot. This page will redirect automatically.
                  </p>
                )}
              </div>
              <SetupProgress phase={phase} />
            </>
          ) : phase === "complete" ? (
            <>
              <div className="text-center">
                <h2 className="text-xl font-semibold text-slate-100">
                  Setup Complete!
                </h2>
                <p className="mt-2 text-sm text-slate-400">{setupMessage}</p>
              </div>
              <SetupProgress complete phase="complete" />
              <div className="flex justify-center">
                <button
                  onClick={onClose}
                  className="rounded-xl bg-blue-500 px-6 py-2.5 text-sm font-medium text-white transition-colors hover:bg-blue-600"
                >
                  Go to Dashboard
                </button>
              </div>
            </>
          ) : (
            <>
              <div className="text-center">
                <div className="mx-auto mb-3 flex h-14 w-14 items-center justify-center rounded-full bg-red-500/20">
                  <AlertCircle className="h-7 w-7 text-red-400" />
                </div>
                <h2 className="text-xl font-semibold text-slate-100">Setup Error</h2>
                <p className="mt-2 text-sm text-red-400">{setupMessage}</p>
              </div>
              <SetupProgress phase="error" />
              <div className="flex justify-center gap-3">
                <button
                  onClick={() => { setPhase("wizard"); setCurrentStep(steps.length - 1) }}
                  className="rounded-xl border border-white/10 bg-white/5 px-4 py-2.5 text-sm font-medium text-slate-300 transition-colors hover:bg-white/10"
                >
                  Back to Wizard
                </button>
                <button
                  onClick={handleApply}
                  className="rounded-xl bg-blue-500 px-4 py-2.5 text-sm font-medium text-white transition-colors hover:bg-blue-600"
                >
                  Retry
                </button>
              </div>
            </>
          )}
        </div>
      </div>
    )
  }

  // ── Wizard steps ──
  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 backdrop-blur-sm">
      <div className="glass-card relative flex h-[90vh] w-full max-w-3xl flex-col overflow-hidden">
        {/* Header with step indicator */}
        <div className="shrink-0 border-b border-white/5 px-6 py-4">
          <div className="mb-3 flex items-center justify-between">
            <h2 className="text-lg font-semibold text-slate-100">
              Setup Wizard
            </h2>
            <button
              onClick={onClose}
              className="rounded-lg px-3 py-1 text-sm text-slate-500 hover:bg-white/5 hover:text-slate-300"
            >
              Cancel
            </button>
          </div>

          {/* Step progress bar */}
          <div className="flex gap-1">
            {steps.map((step, i) => (
              <button
                key={step.id}
                onClick={() => {
                  if (i > currentStep) {
                    for (let s = 0; s < i; s++) {
                      if (getStepError(s, formData) !== null) {
                        setCurrentStep(s)
                        return
                      }
                    }
                  }
                  setCurrentStep(i)
                }}
                className="group flex-1"
                title={step.title}
              >
                <div
                  className={cn(
                    "h-1 rounded-full transition-all",
                    i === currentStep
                      ? "bg-blue-400"
                      : i < currentStep && getStepError(i, formData) !== null
                        ? "bg-red-500/70"
                        : i < currentStep
                          ? "bg-blue-500"
                          : "bg-slate-800"
                  )}
                />
                <p
                  className={cn(
                    "mt-1 hidden text-[10px] font-medium sm:block",
                    i <= currentStep ? "text-slate-400" : "text-slate-700"
                  )}
                >
                  {step.title}
                </p>
              </button>
            ))}
          </div>
        </div>

        {/* Step content */}
        <div className="flex-1 overflow-y-auto px-6 py-5">
          <StepComponent
            data={formData}
            onChange={handleChange}
            onBatchChange={handleBatchChange}
          />
        </div>

        {/* Footer navigation */}
        <div className="shrink-0 border-t border-white/5 px-6 py-4">
          {saveError && (
            <p className="mb-2 text-sm text-red-400">{saveError}</p>
          )}
          {currentStepError && (
            <p className="mb-2 text-sm text-red-400">{currentStepError}</p>
          )}
          <div className="flex items-center justify-between">
            <button
              onClick={() => setCurrentStep((s) => s - 1)}
              disabled={isFirst}
              className={cn(
                "flex items-center gap-1.5 rounded-lg px-4 py-2 text-sm font-medium transition-colors",
                isFirst
                  ? "text-slate-700"
                  : "text-slate-400 hover:bg-white/5 hover:text-slate-200"
              )}
            >
              <ChevronLeft className="h-4 w-4" />
              Back
            </button>

            <span className="text-xs text-slate-600">
              {currentStep + 1} / {steps.length}
            </span>

            {isLast ? (
              <button
                onClick={handleApply}
                disabled={saving}
                className="flex items-center gap-1.5 rounded-lg bg-blue-500 px-5 py-2 text-sm font-medium text-white transition-colors hover:bg-blue-600 disabled:opacity-50"
              >
                {saving ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  <Check className="h-4 w-4" />
                )}
                Apply & Run Setup
              </button>
            ) : (
              <button
                onClick={() => setCurrentStep((s) => s + 1)}
                disabled={!!currentStepError}
                className="flex items-center gap-1.5 rounded-lg bg-blue-500/20 px-4 py-2 text-sm font-medium text-blue-400 transition-colors hover:bg-blue-500/30 disabled:opacity-40 disabled:cursor-not-allowed"
              >
                Next
                <ChevronRight className="h-4 w-4" />
              </button>
            )}
          </div>
        </div>
      </div>
    </div>
  )
}
