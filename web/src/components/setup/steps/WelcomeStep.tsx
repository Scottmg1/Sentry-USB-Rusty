import { useCallback, useEffect, useRef, useState } from "react"
import {
  ArchiveIcon,
  CheckCircleIcon,
  CloseIcon,
  DescriptionIcon,
  ExpandLessIcon,
  ExpandMoreIcon,
  ProgressActivityIcon,
  RotateLeftIcon,
  ShieldIcon,
  UploadFileIcon,
  UploadIcon,
} from "@/components/icons"
import type { StepProps } from "../SetupWizard"
import { cn } from "@/lib/utils"

/** Known config keys grouped by wizard step for display */
const CONFIG_GROUPS: Record<string, { label: string; keys: string[] }> = {
  network: {
    label: "Network",
    keys: ["SENTRYUSB_HOSTNAME", "AP_SSID", "AP_PASS", "AP_IP"],
  },
  storage: {
    label: "Storage",
    keys: [
      "camsize", "musicsize", "lightshowsize", "boomboxsize",
      "USE_NVME", "NVME_DEVICE", "USB_DRIVE",
    ],
  },
  archive: {
    label: "Archive",
    keys: [
      "ARCHIVE_SYSTEM", "archiveserver", "sharename", "shareuser",
      "sharepassword", "sharepath", "RCLONE_DRIVE", "RCLONE_PATH",
      "RSYNC_USER", "RSYNC_SERVER", "RSYNC_PATH",
      "NFS_SERVER", "NFS_PATH",
    ],
  },
  keepawake: {
    label: "Keep Awake",
    keys: [
      "TESLA_BLE_VIN", "TESLA_BLE_RETRY", "TESLAFI_TOKEN",
      "TESSIE_ACCESS_TOKEN", "WEBHOOK_URL",
    ],
  },
  notifications: {
    label: "Notifications",
    keys: [
      "PUSHOVER_ENABLED", "PUSHOVER_USER_KEY", "PUSHOVER_APP_KEY",
      "DISCORD_WEBHOOK_URL", "TELEGRAM_BOT_TOKEN", "TELEGRAM_CHAT_ID",
      "SLACK_WEBHOOK_URL", "SIGNAL_NUMBER", "GOTIFY_URL", "GOTIFY_TOKEN",
      "MATRIX_SERVER", "MATRIX_ROOM", "MATRIX_TOKEN",
      "AWS_SNS_TOPIC_ARN", "IFTTT_EVENT_NAME", "IFTTT_KEY",
      "NOTIFICATION_WEBHOOK_URL",
    ],
  },
  security: {
    label: "Security",
    keys: ["WEB_PASSWORD", "SSH_PUBLIC_KEY", "DISABLE_SSH_PASSWORD"],
  },
  advanced: {
    label: "Advanced",
    keys: [
      "timezone", "ARCHIVE_DELAY", "TEMP_WARN", "TEMP_CRIT",
      "CPU_GOVERNOR", "REPO", "BRANCH",
    ],
  },
}

/** Map legacy lowercase config keys to their current canonical names.
 *  Mirrors `migrate_legacy_config_keys` in crates/setup/src/env.rs so the
 *  wizard inputs (which read CAM_SIZE, ARCHIVE_SERVER, etc.) actually
 *  populate from teslausb-era .conf files that still use camsize,
 *  archiveserver, etc. Without this, uploading an old config silently
 *  drops every legacy key into an unread keyspace and the user sees
 *  blank inputs despite a successful "Imported N keys" toast.
 *  New-name wins: if both old and new are present, keep the new value.
 */
const LEGACY_KEY_MAP: Record<string, string> = {
  archiveserver: "ARCHIVE_SERVER",
  camsize: "CAM_SIZE",
  musicsize: "MUSIC_SIZE",
  lightshowsize: "LIGHTSHOW_SIZE",
  boomboxsize: "BOOMBOX_SIZE",
  sharename: "SHARE_NAME",
  musicsharename: "MUSIC_SHARE_NAME",
  shareuser: "SHARE_USER",
  sharepassword: "SHARE_PASSWORD",
  sharepath: "SHARE_PATH",
  tesla_email: "TESLA_EMAIL",
  tesla_password: "TESLA_PASSWORD",
  tesla_vin: "TESLA_VIN",
  timezone: "TIME_ZONE",
  usb_drive: "DATA_DRIVE",
  USB_DRIVE: "DATA_DRIVE",
  archivedelay: "ARCHIVE_DELAY",
  trigger_file_saved: "TRIGGER_FILE_SAVED",
  trigger_file_sentry: "TRIGGER_FILE_SENTRY",
  trigger_file_any: "TRIGGER_FILE_ANY",
  pushover_enabled: "PUSHOVER_ENABLED",
  pushover_user_key: "PUSHOVER_USER_KEY",
  pushover_app_key: "PUSHOVER_APP_KEY",
  gotify_enabled: "GOTIFY_ENABLED",
  gotify_domain: "GOTIFY_DOMAIN",
  gotify_app_token: "GOTIFY_APP_TOKEN",
  gotify_priority: "GOTIFY_PRIORITY",
  ifttt_enabled: "IFTTT_ENABLED",
  ifttt_event_name: "IFTTT_EVENT_NAME",
  ifttt_key: "IFTTT_KEY",
  sns_enabled: "SNS_ENABLED",
  aws_region: "AWS_REGION",
  aws_access_key_id: "AWS_ACCESS_KEY_ID",
  aws_secret_key: "AWS_SECRET_ACCESS_KEY",
  aws_sns_topic_arn: "AWS_SNS_TOPIC_ARN",
}

function migrateLegacyKeys(parsed: Record<string, string>): Record<string, string> {
  const out = { ...parsed }
  for (const [oldKey, newKey] of Object.entries(LEGACY_KEY_MAP)) {
    if (Object.prototype.hasOwnProperty.call(out, newKey)) continue
    if (Object.prototype.hasOwnProperty.call(out, oldKey)) {
      out[newKey] = out[oldKey]
      delete out[oldKey]
    }
  }
  return out
}

/** Parse a sentryusb.conf file (export KEY=VALUE lines) */
function parseConfFile(text: string): Record<string, string> {
  const result: Record<string, string> = {}
  const exportRegex = /^\s*export\s+([A-Za-z_][A-Za-z0-9_]*)=(.*)$/

  for (const line of text.split("\n")) {
    const match = line.match(exportRegex)
    if (match) {
      const key = match[1]
      let val = match[2].trim()
      // Unquote
      if (val.length >= 2) {
        if ((val.startsWith("'") && val.endsWith("'")) || (val.startsWith('"') && val.endsWith('"'))) {
          val = val.slice(1, -1)
        } else if (val.startsWith("$'") && val.endsWith("'")) {
          val = val.slice(2, -1)
        }
      }
      result[key] = val
    }
  }
  return migrateLegacyKeys(result)
}

/** Mask sensitive values for display */
function maskValue(key: string, value: string): string {
  const sensitiveKeys = [
    "WIFIPASS", "AP_PASS", "sharepassword", "WEB_PASSWORD",
    "PUSHOVER_APP_KEY", "PUSHOVER_USER_KEY", "TESLAFI_TOKEN",
    "TESSIE_ACCESS_TOKEN", "TELEGRAM_BOT_TOKEN", "GOTIFY_TOKEN",
    "MATRIX_TOKEN", "IFTTT_KEY", "AWS_SNS_TOPIC_ARN", "SSH_PUBLIC_KEY",
    "SLACK_WEBHOOK_URL", "DISCORD_WEBHOOK_URL", "NOTIFICATION_WEBHOOK_URL",
  ]
  if (sensitiveKeys.some((k) => k === key) && value.length > 0) {
    return value.slice(0, 2) + "•".repeat(Math.min(value.length - 2, 12))
  }
  return value
}

/** Get a friendly display label for a config key */
function friendlyLabel(key: string): string {
  const labels: Record<string, string> = {
    SSID: "WiFi SSID",
    WIFIPASS: "WiFi Password",
    SENTRYUSB_HOSTNAME: "Hostname",
    AP_SSID: "AP SSID",
    AP_PASS: "AP Password",
    AP_IP: "AP IP Address",
    camsize: "Dashcam Size",
    musicsize: "Music Size",
    lightshowsize: "Light Show Size",
    boomboxsize: "Boombox Size",
    ARCHIVE_SYSTEM: "Archive Method",
    archiveserver: "Archive Server",
    sharename: "Share Name",
    shareuser: "Share User",
    sharepassword: "Share Password",
    sharepath: "Share Path",
    TESLA_BLE_VIN: "Tesla BLE VIN",
    timezone: "Timezone",
  }
  return labels[key] || key
}

interface BackupEntry {
  date: string
  timestamp: string
  location: string
  size: number
  filename: string
}

export function WelcomeStep({ data: _data, onChange: _onChange, onBatchChange }: StepProps) {
  const fileInputRef = useRef<HTMLInputElement>(null)
  const [imported, setImported] = useState<Record<string, string> | null>(null)
  const [fileName, setFileName] = useState<string | null>(null)
  const [expandedGroups, setExpandedGroups] = useState<Set<string>>(new Set())
  const [dragOver, setDragOver] = useState(false)

  const backupFileInputRef = useRef<HTMLInputElement>(null)

  // Restore from backup state
  const [showRestore, setShowRestore] = useState(false)
  const [backups, setBackups] = useState<BackupEntry[]>([])
  const [loadingBackups, setLoadingBackups] = useState(false)
  const [restoringDate, setRestoringDate] = useState<string | null>(null)
  const [restoringUpload, setRestoringUpload] = useState(false)
  const [uploadError, setUploadError] = useState<string | null>(null)
  const [restoreSource, setRestoreSource] = useState<string | null>(null)

  const handleFile = useCallback(
    (file: File) => {
      if (!file.name.endsWith(".conf")) return
      const reader = new FileReader()
      reader.onload = (e) => {
        const text = e.target?.result as string
        if (!text) return
        const parsed = parseConfFile(text)
        setImported(parsed)
        setFileName(file.name)
        setRestoreSource(null)
        onBatchChange(parsed)
        // Expand all groups that have keys
        const groups = new Set<string>()
        for (const [groupId, group] of Object.entries(CONFIG_GROUPS)) {
          if (group.keys.some((k) => k in parsed)) {
            groups.add(groupId)
          }
        }
        setExpandedGroups(groups)
      }
      reader.readAsText(file)
    },
    [onBatchChange]
  )

  const handleDrop = useCallback(
    (e: React.DragEvent) => {
      e.preventDefault()
      setDragOver(false)
      const file = e.dataTransfer.files?.[0]
      if (file) handleFile(file)
    },
    [handleFile]
  )

  const handleClear = useCallback(() => {
    setImported(null)
    setFileName(null)
    setRestoreSource(null)
    if (fileInputRef.current) fileInputRef.current.value = ""
  }, [])

  const toggleGroup = (groupId: string) => {
    setExpandedGroups((prev) => {
      const next = new Set(prev)
      if (next.has(groupId)) next.delete(groupId)
      else next.add(groupId)
      return next
    })
  }

  // Load available backups when restore panel is opened
  useEffect(() => {
    if (!showRestore) return
    setLoadingBackups(true)
    fetch("/api/system/backups")
      .then((r) => r.json())
      .then((data: BackupEntry[]) => {
        setBackups(data || [])
        setLoadingBackups(false)
      })
      .catch(() => {
        setBackups([])
        setLoadingBackups(false)
      })
  }, [showRestore])

  // Handle restoring from a backup
  async function handleRestore(backup: BackupEntry) {
    setRestoringDate(backup.date)
    try {
      // Fetch the full backup data
      const backupRes = await fetch(`/api/system/backup/${backup.date}`)
      if (!backupRes.ok) throw new Error("Failed to fetch backup")
      const backupData = await backupRes.json()

      // Send to restore endpoint
      const restoreRes = await fetch("/api/system/restore", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(backupData),
      })
      if (!restoreRes.ok) throw new Error("Restore failed")
      const result = await restoreRes.json()

      // Parse the config to populate wizard fields
      const parsed = result.config as Record<string, string>
      setImported(parsed)
      setFileName(null)
      setRestoreSource(backup.date)
      setShowRestore(false)
      // Signal to SetupWizard that this is a restore — update the destructive
      // change baseline so drive size comparisons use the backup values.
      onBatchChange({ ...parsed, _restore_baseline: "true" })

      // Expand all groups that have keys
      const groups = new Set<string>()
      for (const [groupId, group] of Object.entries(CONFIG_GROUPS)) {
        if (group.keys.some((k) => k in parsed)) {
          groups.add(groupId)
        }
      }
      setExpandedGroups(groups)
    } catch {
      // error handled silently, user sees button reset
    } finally {
      setRestoringDate(null)
    }
  }

  // Handle uploading a backup JSON file from the user's local machine.
  // Needed during first-run on a fresh image: the archive isn't mounted
  // yet (its credentials live inside the backup), so /api/system/backups
  // returns an empty list and the user has no way to pick a backup. POST
  // the file straight to /api/system/restore — the backend writes config,
  // SSH keys, rclone config, BLE keys, and notification creds back.
  async function handleBackupFileUpload(file: File) {
    if (!file.name.endsWith(".json")) {
      setUploadError("Please select a .json backup file")
      return
    }
    setRestoringUpload(true)
    setUploadError(null)
    try {
      const text = await file.text()

      let backupData: Record<string, unknown>
      try {
        backupData = JSON.parse(text)
      } catch {
        setUploadError("File is not valid JSON")
        return
      }

      if (!backupData.version || !backupData.config) {
        setUploadError("Invalid backup file — missing version or config data")
        return
      }

      const restoreRes = await fetch("/api/system/restore", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: text,
      })
      if (!restoreRes.ok) throw new Error("Restore failed")
      const result = await restoreRes.json()

      const parsed = result.config as Record<string, string>
      setImported(parsed)
      setFileName(null)
      setRestoreSource((backupData.date as string) || "uploaded file")
      setShowRestore(false)
      onBatchChange({ ...parsed, _restore_baseline: "true" })

      const groups = new Set<string>()
      for (const [groupId, group] of Object.entries(CONFIG_GROUPS)) {
        if (group.keys.some((k) => k in parsed)) {
          groups.add(groupId)
        }
      }
      setExpandedGroups(groups)
    } catch {
      setUploadError("Failed to restore from uploaded backup")
    } finally {
      setRestoringUpload(false)
      if (backupFileInputRef.current) backupFileInputRef.current.value = ""
    }
  }

  // Categorize imported keys
  const groupedEntries: { groupId: string; label: string; entries: [string, string][] }[] = []
  const ungroupedEntries: [string, string][] = []

  if (imported) {
    const allGroupedKeys = new Set(
      Object.values(CONFIG_GROUPS).flatMap((g) => g.keys)
    )

    for (const [groupId, group] of Object.entries(CONFIG_GROUPS)) {
      const entries = group.keys
        .filter((k) => k in imported)
        .map((k) => [k, imported[k]] as [string, string])
      if (entries.length > 0) {
        groupedEntries.push({ groupId, label: group.label, entries })
      }
    }

    for (const [k, v] of Object.entries(imported)) {
      if (!allGroupedKeys.has(k)) {
        ungroupedEntries.push([k, v])
      }
    }
  }

  const totalImported = imported ? Object.keys(imported).length : 0

  return (
    <div className="flex flex-col items-center py-6 text-center">
      <div className="mb-6 flex h-20 w-20 items-center justify-center rounded-2xl bg-blue-500/15">
        <ShieldIcon className="h-10 w-10 text-blue-400" />
      </div>
      <h2 className="text-2xl font-bold text-slate-100">
        Welcome to Sentry USB Setup
      </h2>
      <p className="mt-3 max-w-md text-sm leading-relaxed text-slate-400">
        This wizard will guide you through configuring your Sentry USB device.
        You&apos;ll set up storage, archive destinations, notifications,
        and more — all from this interface. WiFi should be configured
        in Raspberry Pi Imager before flashing your SD card.
      </p>

      {/* Upload .conf file or Restore from backup */}
      <div className="mt-8 w-full max-w-md">
        {!imported ? (
          <div className="space-y-3">
            {/* Drag-and-drop config import */}
            <div
              onDragOver={(e) => { e.preventDefault(); setDragOver(true) }}
              onDragLeave={() => setDragOver(false)}
              onDrop={handleDrop}
              onClick={() => fileInputRef.current?.click()}
              className={cn(
                "flex cursor-pointer flex-col items-center gap-3 rounded-xl border-2 border-dashed p-6 transition-colors",
                dragOver
                  ? "border-blue-400/60 bg-blue-500/10"
                  : "border-white/10 bg-white/[0.02] hover:border-white/20 hover:bg-white/[0.04]"
              )}
            >
              <UploadIcon className="h-8 w-8 text-slate-500" />
              <div>
                <p className="text-sm font-medium text-slate-300">
                  Import existing config
                </p>
                <p className="mt-1 text-xs text-slate-500">
                  Drop a <code className="rounded bg-white/5 px-1 py-0.5 text-slate-400">.conf</code> file here or click to browse
                </p>
              </div>
              <input
                ref={fileInputRef}
                type="file"
                accept=".conf"
                className="hidden"
                onChange={(e) => {
                  const file = e.target.files?.[0]
                  if (file) handleFile(file)
                }}
              />
            </div>

            {/* Restore from backup button / panel */}
            {!showRestore ? (
              <button
                onClick={() => setShowRestore(true)}
                className="flex w-full items-center justify-center gap-2 rounded-xl border border-white/10 bg-white/[0.02] px-4 py-3 text-sm text-slate-400 transition-colors hover:border-white/20 hover:bg-white/[0.04] hover:text-slate-300"
              >
                <RotateLeftIcon className="h-4 w-4" />
                Restore from backup
              </button>
            ) : (
              <div className="rounded-xl border border-blue-500/20 bg-blue-500/5 p-4">
                <div className="mb-3 flex items-center justify-between">
                  <div className="flex items-center gap-2">
                    <ArchiveIcon className="h-4 w-4 text-blue-400" />
                    <span className="text-sm font-medium text-blue-300">Available Backups</span>
                  </div>
                  <button
                    onClick={() => setShowRestore(false)}
                    className="rounded-lg p-1 text-slate-500 transition-colors hover:bg-white/5 hover:text-slate-300"
                  >
                    <CloseIcon className="h-4 w-4" />
                  </button>
                </div>

                {loadingBackups ? (
                  <div className="flex items-center justify-center py-4">
                    <ProgressActivityIcon className="h-5 w-5 animate-spin text-blue-400" />
                    <span className="ml-2 text-xs text-slate-500">Scanning for backups...</span>
                  </div>
                ) : backups.length === 0 ? (
                  <div className="space-y-3">
                    <p className="py-1 text-center text-xs text-slate-500">
                      No backups found on this device.
                    </p>
                    <button
                      onClick={() => backupFileInputRef.current?.click()}
                      disabled={restoringUpload}
                      className="flex w-full items-center justify-center gap-2 rounded-lg border border-dashed border-blue-400/30 bg-blue-500/5 px-3 py-3 text-sm text-blue-300 transition-colors hover:border-blue-400/50 hover:bg-blue-500/10 disabled:opacity-50"
                    >
                      {restoringUpload ? (
                        <ProgressActivityIcon className="h-4 w-4 animate-spin" />
                      ) : (
                        <UploadFileIcon className="h-4 w-4" />
                      )}
                      {restoringUpload ? "Restoring..." : "Upload backup file from your computer"}
                    </button>
                    <p className="text-center text-[10px] text-slate-600">
                      Select a <code className="rounded bg-white/5 px-1 py-0.5 text-slate-500">sentryusb-backup-*.json</code> file
                    </p>
                    {uploadError && (
                      <p className="text-center text-xs text-red-400">{uploadError}</p>
                    )}
                  </div>
                ) : (
                  <div className="space-y-3">
                    <div className="max-h-48 space-y-1.5 overflow-y-auto">
                      {backups.map((b) => (
                        <button
                          key={b.date}
                          onClick={() => handleRestore(b)}
                          disabled={restoringDate !== null || restoringUpload}
                          className="flex w-full items-center justify-between rounded-lg border border-white/5 bg-white/[0.02] px-3 py-2.5 text-left transition-colors hover:bg-white/[0.05] hover:border-white/10 disabled:opacity-50"
                        >
                          <div>
                            <p className="text-xs font-medium text-slate-300">
                              {new Date(b.timestamp).toLocaleDateString(undefined, {
                                weekday: "short",
                                month: "short",
                                day: "numeric",
                                year: "numeric",
                              })}
                            </p>
                            <p className="text-[10px] text-slate-500">
                              {new Date(b.timestamp).toLocaleTimeString(undefined, {
                                hour: "2-digit",
                                minute: "2-digit",
                              })}
                              {" · "}
                              {b.location === "archive" ? "Archive server" : "Local SSD"}
                              {" · "}
                              {(b.size / 1024).toFixed(1)} KB
                            </p>
                          </div>
                          {restoringDate === b.date ? (
                            <ProgressActivityIcon className="h-4 w-4 animate-spin text-blue-400" />
                          ) : (
                            <RotateLeftIcon className="h-3.5 w-3.5 text-slate-500" />
                          )}
                        </button>
                      ))}
                    </div>
                    <div className="border-t border-white/5 pt-2">
                      <button
                        onClick={() => backupFileInputRef.current?.click()}
                        disabled={restoringUpload || restoringDate !== null}
                        className="flex w-full items-center justify-center gap-2 rounded-lg border border-dashed border-white/10 px-3 py-2 text-xs text-slate-500 transition-colors hover:border-white/20 hover:text-slate-400 disabled:opacity-50"
                      >
                        {restoringUpload ? (
                          <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />
                        ) : (
                          <UploadFileIcon className="h-3.5 w-3.5" />
                        )}
                        {restoringUpload ? "Restoring..." : "Or upload a backup file from your computer"}
                      </button>
                    </div>
                    {uploadError && (
                      <p className="text-center text-xs text-red-400">{uploadError}</p>
                    )}
                  </div>
                )}
                <input
                  ref={backupFileInputRef}
                  type="file"
                  accept=".json"
                  className="hidden"
                  onChange={(e) => {
                    const file = e.target.files?.[0]
                    if (file) handleBackupFileUpload(file)
                  }}
                />
              </div>
            )}
          </div>
        ) : (
          <div className="rounded-xl border border-emerald-500/20 bg-emerald-500/5 p-4">
            {/* Header */}
            <div className="flex items-center justify-between">
              <div className="flex items-center gap-2.5">
                <CheckCircleIcon className="h-5 w-5 text-emerald-400" />
                <div className="text-left">
                  <p className="text-sm font-medium text-emerald-300">
                    {restoreSource ? "Backup restored" : "Config imported"}
                  </p>
                  <p className="text-xs text-slate-500">
                    {restoreSource ? (
                      <>
                        <RotateLeftIcon className="mr-1 inline h-3 w-3" />
                        Backup from {restoreSource} — {totalImported} setting{totalImported !== 1 ? "s" : ""} restored
                      </>
                    ) : (
                      <>
                        <DescriptionIcon className="mr-1 inline h-3 w-3" />
                        {fileName} — {totalImported} setting{totalImported !== 1 ? "s" : ""} loaded
                      </>
                    )}
                  </p>
                </div>
              </div>
              <button
                onClick={handleClear}
                className="rounded-lg p-1.5 text-slate-500 transition-colors hover:bg-white/5 hover:text-slate-300"
                title="Remove imported config"
              >
                <CloseIcon className="h-4 w-4" />
              </button>
            </div>

            {/* Grouped config summary */}
            <div className="mt-4 space-y-1 text-left">
              {groupedEntries.map(({ groupId, label, entries }) => (
                <div key={groupId}>
                  <button
                    onClick={() => toggleGroup(groupId)}
                    className="flex w-full items-center justify-between rounded-lg px-2 py-1.5 text-left transition-colors hover:bg-white/5"
                  >
                    <span className="text-xs font-semibold uppercase tracking-wider text-slate-400">
                      {label}
                      <span className="ml-1.5 font-normal text-slate-600">
                        ({entries.length})
                      </span>
                    </span>
                    {expandedGroups.has(groupId) ? (
                      <ExpandLessIcon className="h-3.5 w-3.5 text-slate-600" />
                    ) : (
                      <ExpandMoreIcon className="h-3.5 w-3.5 text-slate-600" />
                    )}
                  </button>
                  {expandedGroups.has(groupId) && (
                    <div className="mb-2 ml-2 space-y-0.5 border-l border-white/5 pl-3">
                      {entries.map(([key, val]) => (
                        <div
                          key={key}
                          className="flex items-baseline justify-between gap-4 py-0.5"
                        >
                          <span className="text-xs text-slate-400">
                            {friendlyLabel(key)}
                          </span>
                          <span className="truncate text-xs font-mono text-slate-300">
                            {maskValue(key, val)}
                          </span>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              ))}

              {ungroupedEntries.length > 0 && (
                <div>
                  <button
                    onClick={() => toggleGroup("_other")}
                    className="flex w-full items-center justify-between rounded-lg px-2 py-1.5 text-left transition-colors hover:bg-white/5"
                  >
                    <span className="text-xs font-semibold uppercase tracking-wider text-slate-400">
                      Other
                      <span className="ml-1.5 font-normal text-slate-600">
                        ({ungroupedEntries.length})
                      </span>
                    </span>
                    {expandedGroups.has("_other") ? (
                      <ExpandLessIcon className="h-3.5 w-3.5 text-slate-600" />
                    ) : (
                      <ExpandMoreIcon className="h-3.5 w-3.5 text-slate-600" />
                    )}
                  </button>
                  {expandedGroups.has("_other") && (
                    <div className="mb-2 ml-2 space-y-0.5 border-l border-white/5 pl-3">
                      {ungroupedEntries.map(([key, val]) => (
                        <div
                          key={key}
                          className="flex items-baseline justify-between gap-4 py-0.5"
                        >
                          <span className="text-xs text-slate-400">
                            {friendlyLabel(key)}
                          </span>
                          <span className="truncate text-xs font-mono text-slate-300">
                            {maskValue(key, val)}
                          </span>
                        </div>
                      ))}
                    </div>
                  )}
                </div>
              )}
            </div>
          </div>
        )}
      </div>

      {/* Info cards */}
      <div className="mt-6 grid w-full max-w-md gap-3 text-left">
        <InfoCard
          title="No SSH Required"
          desc="Everything is configured right here in your browser."
        />
        <InfoCard
          title="Safe to Re-run"
          desc="You can re-run this wizard anytime to change settings."
        />
        <InfoCard
          title="Preserves Comments"
          desc="Your existing config file comments are preserved."
        />
      </div>
      <p className="mt-6 text-xs text-slate-600">
        Click <span className="text-slate-400">Next</span> to continue to the
        privacy disclosure
        {imported ? " — your imported settings are pre-filled in each step" : ""}.
      </p>
    </div>
  )
}

function InfoCard({ title, desc }: { title: string; desc: string }) {
  return (
    <div className="rounded-lg border border-white/5 bg-white/[0.02] px-4 py-3">
      <p className="text-sm font-medium text-slate-200">{title}</p>
      <p className="mt-0.5 text-xs text-slate-500">{desc}</p>
    </div>
  )
}
