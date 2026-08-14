import { useState, useEffect } from "react"
import {
  CheckCircleIcon,
  ErrorIcon,
  ProgressActivityIcon,
  RotateLeftIcon,
  SaveIcon,
  WarningIcon,
} from "@/components/icons"
import { cn } from "@/lib/utils"
import { PrefCard } from "@/components/settings/PrefCard"

type ActionState = "idle" | "loading" | "success" | "error"

interface BackupEntry {
  date: string
  timestamp: string
  location: string
  size: number
  filename: string
}

const INLINE_CAP = 5

export function ConfigBackupSection() {
  const [backupLocation, setBackupLocation] = useState<string>("archive")
  const [lastBackup, setLastBackup] = useState<{ date: string; timestamp: string } | null>(null)
  const [backupState, setBackupState] = useState<ActionState>("idle")
  const [loaded, setLoaded] = useState(false)

  const [backups, setBackups] = useState<BackupEntry[]>([])
  const [showAll, setShowAll] = useState(false)
  const [restoreState, setRestoreState] = useState<
    "idle" | "confirm" | "restoring" | "success" | "error"
  >("idle")
  const [selectedBackup, setSelectedBackup] = useState<BackupEntry | null>(null)
  const [restoreResult, setRestoreResult] = useState<{
    date: string
    hostname: string
  } | null>(null)

  useEffect(() => {
    fetch("/api/config/preference?key=backup_location")
      .then((r) => r.json())
      .then((d) => {
        if (d.value) setBackupLocation(d.value)
      })
      .catch(() => {})

    fetch("/api/system/backups")
      .then((r) => r.json())
      .then((data: BackupEntry[]) => {
        const list = data || []
        setBackups(list)
        if (list.length > 0) {
          setLastBackup({ date: list[0].date, timestamp: list[0].timestamp })
        }
        setLoaded(true)
      })
      .catch(() => setLoaded(true))
  }, [])

  async function handleLocationChange(loc: string) {
    setBackupLocation(loc)
    await fetch("/api/config/preference", {
      method: "PUT",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ key: "backup_location", value: loc }),
    })
  }

  async function handleBackupNow() {
    setBackupState("loading")
    try {
      const res = await fetch("/api/system/backup?force=1", { method: "POST" })
      if (!res.ok) throw new Error("Backup failed")
      const result = await res.json()
      setLastBackup({ date: result.date, timestamp: new Date().toISOString() })
      try {
        const list = await fetch("/api/system/backups").then((r) => r.json())
        setBackups(list || [])
      } catch {
        /* ignore */
      }
      setBackupState("success")
      setTimeout(() => setBackupState("idle"), 3000)
    } catch {
      setBackupState("error")
      setTimeout(() => setBackupState("idle"), 3000)
    }
  }

  async function handleRestore() {
    if (!selectedBackup) return
    setRestoreState("restoring")
    try {
      const backupRes = await fetch(`/api/system/backup/${selectedBackup.date}`)
      if (!backupRes.ok) throw new Error("Failed to fetch backup")
      const backupData = await backupRes.json()

      const restoreRes = await fetch("/api/system/restore", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(backupData),
      })
      if (!restoreRes.ok) throw new Error("Restore failed")
      const result = await restoreRes.json()
      setRestoreResult({ date: result.date, hostname: result.hostname })
      setRestoreState("success")
    } catch {
      setRestoreState("error")
      setTimeout(() => {
        setRestoreState("idle")
        setSelectedBackup(null)
      }, 3000)
    }
  }

  const visibleBackups = showAll ? backups : backups.slice(0, INLINE_CAP)
  const hiddenCount = backups.length - INLINE_CAP

  return (
    <PrefCard
      icon={<SaveIcon className="h-3.5 w-3.5" />}
      halo="blue"
      title="Config Backup"
    >
      <div className="flex flex-wrap items-center gap-2">
        <span className="text-[10px] text-slate-500">Location:</span>
        <button
          onClick={() => handleLocationChange("archive")}
          className={cn(
            "rounded-lg border px-2.5 py-1 text-[11px] font-medium transition-all",
            backupLocation === "archive"
              ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
              : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
          )}
        >
          Archive Server
        </button>
        <button
          onClick={() => handleLocationChange("ssd")}
          className={cn(
            "rounded-lg border px-2.5 py-1 text-[11px] font-medium transition-all",
            backupLocation === "ssd"
              ? "border-blue-500/40 bg-blue-500/10 text-blue-400"
              : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05]"
          )}
        >
          Local SSD
        </button>
        <div className="ml-auto flex items-center gap-2">
          <span className="text-[10px] text-slate-600">
            {loaded && lastBackup
              ? new Date(lastBackup.timestamp).toLocaleDateString(undefined, {
                  month: "short",
                  day: "numeric",
                })
              : loaded
              ? "No backups"
              : ""}
          </span>
          <button
            onClick={handleBackupNow}
            disabled={backupState === "loading"}
            className={cn(
              "rounded-lg px-2.5 py-1 text-[11px] font-medium transition-all",
              backupState === "success"
                ? "bg-emerald-500/20 text-emerald-400"
                : backupState === "error"
                ? "bg-red-500/20 text-red-400"
                : backupState === "loading"
                ? "bg-blue-500/20 text-blue-400"
                : "bg-white/5 text-slate-300 hover:bg-white/10"
            )}
          >
            {backupState === "loading" && "Backing up..."}
            {backupState === "success" && "Done!"}
            {backupState === "error" && "Failed"}
            {backupState === "idle" && "Backup Now"}
          </button>
        </div>
      </div>

      {restoreState === "success" && restoreResult && (
        <div className="rounded-xl border border-emerald-500/20 bg-emerald-500/5 p-4">
          <div className="flex items-start gap-3">
            <CheckCircleIcon className="mt-0.5 h-5 w-5 shrink-0 text-emerald-400" />
            <div>
              <p className="text-sm font-medium text-emerald-300">Config Restored</p>
              <p className="mt-1 text-xs text-slate-400">
                Backup from {restoreResult.date} has been restored
                {restoreResult.hostname ? ` (${restoreResult.hostname})` : ""}. Run setup to
                apply the restored configuration.
              </p>
              <button
                onClick={() => {
                  setRestoreState("idle")
                  setSelectedBackup(null)
                  setRestoreResult(null)
                }}
                className="mt-3 rounded-lg bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10"
              >
                Done
              </button>
            </div>
          </div>
        </div>
      )}

      {restoreState === "confirm" && selectedBackup && (
        <div className="rounded-xl border border-amber-500/20 bg-amber-500/5 p-4">
          <div className="flex items-start gap-3">
            <WarningIcon className="mt-0.5 h-5 w-5 shrink-0 text-amber-400" />
            <div>
              <p className="text-sm font-medium text-amber-300">Confirm Restore</p>
              <p className="mt-1 text-xs text-slate-400">
                This will overwrite your current configuration with the backup from{" "}
                <span className="text-slate-300">
                  {new Date(selectedBackup.timestamp).toLocaleDateString(undefined, {
                    weekday: "short",
                    month: "short",
                    day: "numeric",
                    year: "numeric",
                  })}
                </span>
                . SSH keys, BLE pairing, and notification credentials will also be restored. You
                will need to run setup afterward to apply changes.
              </p>
              <div className="mt-3 flex gap-2">
                <button
                  onClick={() => {
                    setRestoreState("idle")
                    setSelectedBackup(null)
                  }}
                  className="rounded-lg border border-white/10 px-3 py-1.5 text-xs font-medium text-slate-400 transition-colors hover:bg-white/5"
                >
                  Cancel
                </button>
                <button
                  onClick={handleRestore}
                  className="rounded-lg bg-amber-500/20 px-3 py-1.5 text-xs font-medium text-amber-300 transition-colors hover:bg-amber-500/30"
                >
                  Restore Config
                </button>
              </div>
            </div>
          </div>
        </div>
      )}

      {restoreState === "error" && (
        <div className="flex items-center gap-2 rounded-lg bg-red-500/10 px-3 py-2 text-xs text-red-400">
          <ErrorIcon className="h-3.5 w-3.5 shrink-0" />
          Restore failed. Please try again.
        </div>
      )}

      {loaded && backups.length === 0 && restoreState === "idle" && (
        <p className="py-2 text-center text-xs text-slate-500">
          No backups yet. Backups are created automatically after each archive.
        </p>
      )}

      {backups.length > 0 && (
        <div className="space-y-1.5">
          <p className="section-label">Available Backups</p>
          {visibleBackups.map((b) => (
            <button
              key={b.date}
              onClick={() => {
                setSelectedBackup(b)
                setRestoreState("confirm")
              }}
              disabled={restoreState === "restoring"}
              className="flex w-full items-center justify-between rounded-lg border border-white/5 bg-white/[0.02] px-3 py-2.5 text-left transition-colors hover:border-white/10 hover:bg-white/[0.05] disabled:opacity-50"
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
              {restoreState === "restoring" && selectedBackup?.date === b.date ? (
                <ProgressActivityIcon className="h-4 w-4 animate-spin text-blue-400" />
              ) : (
                <RotateLeftIcon className="h-3.5 w-3.5 text-slate-500" />
              )}
            </button>
          ))}
          {!showAll && hiddenCount > 0 && (
            <button
              onClick={() => setShowAll(true)}
              className="w-full rounded-lg border border-white/5 bg-white/[0.01] py-1.5 text-[11px] text-slate-500 transition-colors hover:bg-white/[0.04] hover:text-slate-300"
            >
              Show all ({backups.length})
            </button>
          )}
          {showAll && hiddenCount > 0 && (
            <button
              onClick={() => setShowAll(false)}
              className="w-full rounded-lg border border-white/5 bg-white/[0.01] py-1.5 text-[11px] text-slate-500 transition-colors hover:bg-white/[0.04] hover:text-slate-300"
            >
              Show less
            </button>
          )}
        </div>
      )}
    </PrefCard>
  )
}
