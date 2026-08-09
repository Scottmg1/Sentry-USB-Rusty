import { useState, useEffect } from "react"
import {
  Download,
  Loader2,
  CheckCircle,
  AlertCircle,
} from "lucide-react"
import { cn } from "@/lib/utils"
import { wsClient } from "@/lib/ws"
import { useVersion } from "@/hooks/useVersion"
import { PrefCard } from "@/components/settings/PrefCard"
import { Pill } from "@/components/ui/Pill"
import { Toggle } from "@/components/ui/Toggle"
import { Modal } from "@/components/ui/Modal"

type UpdateStatus =
  | "idle"
  | "checking_internet"
  | "checking"
  | "downloading"
  | "installing"
  | "updating_scripts"
  | "restarting"
  | "reconnecting"
  | "done"
  | "error"

type ReleaseInfo = {
  version: string
  release_url: string
  release_notes: string
}

interface Props {
  /** Optional callback so the parent can react when an install kicks off. */
  onInstallStart?: () => void
}

export function UpdateSection({ onInstallStart }: Props) {
  const version = useVersion()
  const [updateStatus, setUpdateStatus] = useState<UpdateStatus>("idle")
  const [updateError, setUpdateError] = useState<string | null>(null)
  const [updateMessage, setUpdateMessage] = useState<string | null>(null)
  const [installedVersion, setInstalledVersion] = useState<string | null>(null)
  const [isCheckingUpdate, setIsCheckingUpdate] = useState(false)
  const [stableUpdate, setStableUpdate] = useState<ReleaseInfo | null>(null)
  const [prereleaseUpdate, setPrereleaseUpdate] = useState<ReleaseInfo | null>(null)
  const [revertStable, setRevertStable] = useState<ReleaseInfo | null>(null)
  const [autoUpdateEnabled, setAutoUpdateEnabled] = useState(true)
  const [includePrerelease, setIncludePrerelease] = useState(false)
  const [showUpdateModal, setShowUpdateModal] = useState(false)
  const [downloadPercent, setDownloadPercent] = useState<number | null>(null)

  useEffect(() => {
    if (!showUpdateModal) return
    if (updateStatus === "done") {
      const t = setTimeout(() => setShowUpdateModal(false), 3000)
      return () => clearTimeout(t)
    }
    // On error the modal closes so the card's inline error is visible.
    if (updateStatus === "idle" || updateStatus === "error") {
      setShowUpdateModal(false)
    }
  }, [showUpdateModal, updateStatus])

  useEffect(() => {
    fetch("/api/system/update-status")
      .then((r) => r.json())
      .then((data) => {
        if (data.stable?.available) {
          setStableUpdate({
            version: data.stable.version,
            release_url: data.stable.release_url,
            release_notes: data.stable.release_notes,
          })
        } else if (data.update_available) {
          setStableUpdate({
            version: data.latest_version,
            release_url: data.release_url,
            release_notes: data.release_notes,
          })
        }
        if (data.prerelease?.available) {
          setPrereleaseUpdate({
            version: data.prerelease.version,
            release_url: data.prerelease.release_url,
            release_notes: data.prerelease.release_notes,
          })
        }
        if (data.revert_stable) {
          setRevertStable({
            version: data.revert_stable.version,
            release_url: data.revert_stable.release_url,
            release_notes: data.revert_stable.release_notes,
          })
        }
      })
      .catch(() => {})
    fetch("/api/config/preference?key=auto_update_check")
      .then((r) => r.json())
      .then((data) => setAutoUpdateEnabled(data.value !== "disabled"))
      .catch(() => {})
    fetch("/api/config/preference?key=update_channel")
      .then((r) => r.json())
      .then((data) => setIncludePrerelease(data.value === "prerelease"))
      .catch(() => {})
  }, [])

  async function handleCheckForUpdate(oneTimePrerelease = false) {
    setIsCheckingUpdate(true)
    setStableUpdate(null)
    setPrereleaseUpdate(null)
    setRevertStable(null)
    setUpdateError(null)
    try {
      const wantPrerelease = includePrerelease || oneTimePrerelease
      const url =
        "/api/system/check-update" + (wantPrerelease ? "?include_prerelease=true" : "")
      const res = await fetch(url, { method: "POST" })
      if (!res.ok) throw new Error("Failed to check for updates")
      const data = await res.json()
      if (data.error) {
        setUpdateError(data.error)
      } else {
        let foundAny = false
        if (data.stable?.available) {
          setStableUpdate({
            version: data.stable.version,
            release_url: data.stable.release_url,
            release_notes: data.stable.release_notes,
          })
          foundAny = true
        } else if (data.update_available) {
          setStableUpdate({
            version: data.latest_version,
            release_url: data.release_url,
            release_notes: data.release_notes,
          })
          foundAny = true
        }
        if (data.prerelease?.available) {
          setPrereleaseUpdate({
            version: data.prerelease.version,
            release_url: data.prerelease.release_url,
            release_notes: data.prerelease.release_notes,
          })
          foundAny = true
        }
        if (data.revert_stable) {
          setRevertStable({
            version: data.revert_stable.version,
            release_url: data.revert_stable.release_url,
            release_notes: data.revert_stable.release_notes,
          })
          foundAny = true
        }
        if (!foundAny) {
          setUpdateStatus("done")
          setUpdateMessage(`You're up to date (${data.current_version || version})`)
          setTimeout(() => {
            setUpdateStatus("idle")
            setUpdateMessage(null)
          }, 4000)
        }
      }
    } catch (err) {
      setUpdateError(err instanceof Error ? err.message : "Failed to check for updates")
    } finally {
      setIsCheckingUpdate(false)
    }
  }

  async function handleInstallUpdate(targetVersion?: string) {
    onInstallStart?.()
    setUpdateStatus("checking_internet")
    setUpdateError(null)
    setUpdateMessage("Checking internet connection...")
    setShowUpdateModal(true)
    setDownloadPercent(null)
    // Track the version we're installing so the success modal and message
    // can show it without trusting /api/system/version — the OLD daemon
    // answers that endpoint until reboot fires and can return a stale tag.
    const preUpdateVersion = version
    let newVersion: string | null = targetVersion ?? null
    setInstalledVersion(newVersion)

    // Baseline for the reboot gate. The version tag can't prove a reboot
    // (the old daemon rewrites /opt/sentryusb/version BEFORE `reboot`
    // fires and keeps serving it — the source of the rare premature
    // "Update complete"), so success requires the kernel boot_id to have
    // changed. A null boot_id on either side never counts as proof; the
    // watchdog handles that case.
    let preBootId: string | null = null
    try {
      const r = await fetch("/api/system/version", { cache: "no-store" })
      if (r.ok) preBootId = (await r.json()).boot_id || null
    } catch {
      /* gate refuses success without a baseline */
    }

    let sawRestartPending = false
    let enteredReconnect = false
    let reconnected = false
    let fallbackTimer: ReturnType<typeof setTimeout> | null = null

    const unsubscribe = wsClient.subscribe("update_status", (data: unknown) => {
      const msg = data as {
        status?: string
        message?: string
        error?: string
        output?: string
        percent?: number | null
      }
      if (msg.error) {
        cleanup()
        setUpdateStatus("error")
        setUpdateError(msg.error)
        setUpdateMessage(null)
        return
      }
      if (msg.status) {
        const statusMap: Record<string, UpdateStatus> = {
          checking_internet: "checking_internet",
          checking: "checking",
          remounting: "installing",
          downloading: "downloading",
          installing: "installing",
          updating_scripts: "updating_scripts",
          restarting: "restarting",
        }
        setUpdateStatus(statusMap[msg.status] || "installing")
      }
      setDownloadPercent(
        msg.status === "downloading" && typeof msg.percent === "number"
          ? msg.percent
          : null
      )
      if (msg.status === "complete") {
        sawRestartPending = true
        if (msg.output) {
          const m = msg.output.match(/Updated to (\S+?)\.?\s*$/)
          if (m) {
            newVersion = m[1]
            setInstalledVersion(newVersion)
          }
        }
      }
      if (msg.status === "restarting") {
        sawRestartPending = true
        // Reboot fires ~3 s after this event; polls that land earlier are
        // rejected by the boot_id gate anyway.
        setTimeout(enterReconnect, 3000)
      }
      if (msg.message) {
        setUpdateMessage(msg.message)
      }
    })

    // If the WS drops after the backend announced the restart, don't wait
    // for the fallback timer — the reboot is what killed the socket.
    const unsubStatus = wsClient.onStatusChange((connected) => {
      if (!connected && sawRestartPending) enterReconnect()
    })

    function cleanup() {
      unsubscribe()
      unsubStatus()
      if (fallbackTimer) clearTimeout(fallbackTimer)
    }

    function enterReconnect() {
      if (enteredReconnect) return
      enteredReconnect = true
      cleanup()
      setUpdateStatus("reconnecting")
      setUpdateMessage("Waiting for device to come back online...")

      const pollInterval = setInterval(async () => {
        try {
          const r = await fetch("/api/system/version", { cache: "no-store" })
          if (!r.ok) return
          const data = await r.json()
          const polled = (data.version || "").trim()
          const bootId = data.boot_id || null
          // Success gate: proven reboot + expected version.
          if (!preBootId || !bootId || bootId === preBootId) return
          const norm = (s: string) => s.replace(/^v/, "")
          const versionOk = newVersion
            ? norm(polled) === norm(newVersion)
            : Boolean(polled) && polled !== preUpdateVersion
          if (!versionOk) return
          reconnected = true
          clearInterval(pollInterval)
          setStableUpdate(null)
          setPrereleaseUpdate(null)
          setRevertStable(null)
          setUpdateStatus("done")
          setUpdateMessage(`Update complete — now running ${newVersion || polled || "latest"}`)
          setTimeout(() => {
            setUpdateStatus("idle")
            setUpdateMessage(null)
            setInstalledVersion(null)
            // Hard reload so every cached chunk and hook (useVersion,
            // feature-gated UI) picks up against the freshly installed
            // backend instead of holding the pre-update snapshot.
            window.location.reload()
          }, 6000)
        } catch {
          /* Still restarting */
        }
      }, 3000)
      setTimeout(() => {
        if (!reconnected) {
          clearInterval(pollInterval)
          setUpdateStatus("idle")
          setUpdateMessage(null)
          setInstalledVersion(null)
          setUpdateError("Update may still be in progress. Refresh the page in a moment.")
        }
      }, 180000)
    }

    try {
      const checkRes = await fetch("/api/system/check-internet")
      const checkData = await checkRes.json()
      if (!checkData.connected) {
        cleanup()
        setUpdateStatus("error")
        setUpdateError("No internet connection. Connect to WiFi first.")
        setUpdateMessage(null)
        return
      }

      const res = await fetch("/api/system/update", {
        method: "POST",
        headers: targetVersion ? { "Content-Type": "application/json" } : {},
        body: targetVersion ? JSON.stringify({ version: targetVersion }) : undefined,
      })
      if (!res.ok) throw new Error("Failed to start update")

      // Last-resort entry into reconnect mode for a WS that was dead the
      // whole time (no progress events, no restarting signal). Generous —
      // a slow download alone can exceed the old 20 s guess.
      fallbackTimer = setTimeout(enterReconnect, 120000)
    } catch (err) {
      cleanup()
      setUpdateStatus("error")
      setUpdateError(err instanceof Error ? err.message : "Update failed")
      setUpdateMessage(null)
      setInstalledVersion(null)
      setRevertStable(null)
    }
  }

  const installInProgress =
    updateStatus !== "idle" &&
    updateStatus !== "error" &&
    updateStatus !== "done"

  const headerIcon =
    updateStatus === "error" ? (
      <AlertCircle className="h-3.5 w-3.5" />
    ) : updateStatus === "done" ? (
      <CheckCircle className="h-3.5 w-3.5" />
    ) : installInProgress ? (
      <Loader2 className="h-3.5 w-3.5 animate-spin" />
    ) : (
      <Download className="h-3.5 w-3.5" />
    )

  const headerHalo =
    updateStatus === "error"
      ? "red"
      : updateStatus === "done"
      ? "accent"
      : stableUpdate || prereleaseUpdate
      ? "accent"
      : "slate"

  return (
    <>
      <PrefCard
        icon={headerIcon}
        halo={headerHalo}
        title="Software Updates"
        badge={
          // Always show the *current* installed version here. The available
          // update's version is shown in the "Stable:"/"Pre-release:" card
          // below; surfacing it in the badge made it look like the pending
          // release was already installed. Accent just flags that one is waiting.
          <Pill kind={stableUpdate || prereleaseUpdate ? "accent" : "slate"}>
            {version ?? "…"}
          </Pill>
        }
      >
        <p className="t-xs">
          {updateStatus === "idle" && !updateError && !stableUpdate && !prereleaseUpdate &&
            "Check for and install the latest version."}
          {updateStatus === "idle" && updateError && (
            <span className="text-red-400">{updateError}</span>
          )}
          {updateStatus === "error" && (
            <span className="text-red-400">{updateError || "Update failed."}</span>
          )}
          {updateStatus === "done" && (
            <span className="text-emerald-400">{updateMessage || "Update complete!"}</span>
          )}
          {installInProgress && (updateMessage || "Installing…")}
        </p>

        {stableUpdate && updateStatus === "idle" && (
          <div className="rounded-lg border border-emerald-500/20 bg-emerald-500/5 p-3">
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0">
                <p className="text-xs font-semibold text-emerald-300">
                  Stable: {stableUpdate.version}
                </p>
                <p className="mt-0.5 text-[11px] text-slate-400">
                  Updates server, scripts &amp; BLE daemon.{" "}
                  <a
                    href={stableUpdate.release_url}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="text-blue-400 underline hover:text-blue-300"
                  >
                    Notes
                  </a>
                </p>
              </div>
              <button
                onClick={() => handleInstallUpdate(stableUpdate.version)}
                className="shrink-0 rounded-lg bg-emerald-500 px-3 py-1.5 text-[11px] font-medium text-white hover:bg-emerald-600"
              >
                Install
              </button>
            </div>
          </div>
        )}

        {prereleaseUpdate && updateStatus === "idle" && (
          <div className="rounded-lg border border-amber-500/20 bg-amber-500/5 p-3">
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0">
                <p className="text-xs font-semibold text-amber-300">
                  Pre-release: {prereleaseUpdate.version}
                </p>
                <p className="mt-0.5 text-[11px] text-slate-400">
                  Test build — may contain bugs.{" "}
                  <a
                    href={prereleaseUpdate.release_url}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="text-blue-400 underline hover:text-blue-300"
                  >
                    Notes
                  </a>
                </p>
              </div>
              <button
                onClick={() => handleInstallUpdate(prereleaseUpdate.version)}
                className="shrink-0 rounded-lg bg-amber-500 px-3 py-1.5 text-[11px] font-medium text-white hover:bg-amber-600"
              >
                Install
              </button>
            </div>
          </div>
        )}

        {revertStable && updateStatus === "idle" && (
          <div className="rounded-lg border border-blue-500/20 bg-blue-500/5 p-3">
            <div className="flex items-center justify-between gap-2">
              <div className="min-w-0">
                <p className="text-xs font-semibold text-blue-300">
                  Revert to Stable: {revertStable.version}
                </p>
                <p className="mt-0.5 text-[11px] text-slate-400">
                  Downgrade from pre-release to latest stable.{" "}
                  <a
                    href={revertStable.release_url}
                    target="_blank"
                    rel="noopener noreferrer"
                    className="text-blue-400 underline hover:text-blue-300"
                  >
                    Notes
                  </a>
                </p>
              </div>
              <button
                onClick={() => handleInstallUpdate(revertStable.version)}
                className="shrink-0 rounded-lg bg-blue-500 px-3 py-1.5 text-[11px] font-medium text-white hover:bg-blue-600"
              >
                Revert
              </button>
            </div>
          </div>
        )}

        <button
          onClick={() => handleCheckForUpdate()}
          disabled={isCheckingUpdate || installInProgress}
          className={cn(
            "self-start rounded-lg px-3 py-1.5 text-xs font-medium transition-colors disabled:opacity-50",
            "bg-emerald-500/15 text-emerald-400 hover:bg-emerald-500/25"
          )}
        >
          {isCheckingUpdate ? (
            <span className="inline-flex items-center gap-1.5">
              <Loader2 className="h-3.5 w-3.5 animate-spin" /> Checking
            </span>
          ) : (
            "Check for Updates"
          )}
        </button>
      </PrefCard>

      <PrefCard
        icon={<Download className="h-3.5 w-3.5" />}
        halo="slate"
        title="Update Preferences"
      >
        <Toggle
          checked={autoUpdateEnabled}
          onChange={async (next) => {
            setAutoUpdateEnabled(next)
            await fetch("/api/config/preference", {
              method: "PUT",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({
                key: "auto_update_check",
                value: next ? "enabled" : "disabled",
              }),
            }).catch(() => {})
          }}
          label="Auto-check after each archive"
          sub="Polls GitHub releases on every archive cycle"
        />
        <Toggle
          checked={includePrerelease}
          onChange={async (next) => {
            setIncludePrerelease(next)
            await fetch("/api/config/preference", {
              method: "PUT",
              headers: { "Content-Type": "application/json" },
              body: JSON.stringify({
                key: "update_channel",
                value: next ? "prerelease" : "stable",
              }),
            }).catch(() => {})
          }}
          label="Include pre-releases"
          sub="Test builds may contain bugs"
        />
      </PrefCard>

      {showUpdateModal && (
        <Modal
          title={updateStatus === "done" ? "Update Complete" : "Installing Update"}
          onClose={() => setShowUpdateModal(false)}
          dismissable={false}
          size="sm"
        >
          <div className="flex flex-col items-center gap-3 py-6 text-center">
            {updateStatus === "done" ? (
              <CheckCircle className="h-12 w-12 text-emerald-400" />
            ) : (
              <Loader2 className="h-12 w-12 animate-spin text-blue-400" />
            )}
            <h2 className="text-lg font-semibold text-slate-100">
              {updateStatus === "checking_internet" && "Checking connection"}
              {updateStatus === "checking" && "Checking release"}
              {updateStatus === "downloading" && "Downloading update"}
              {updateStatus === "installing" && "Installing update"}
              {updateStatus === "updating_scripts" && "Updating scripts"}
              {updateStatus === "restarting" && "Restarting Pi"}
              {updateStatus === "reconnecting" && "Waiting for Pi to come back online"}
              {updateStatus === "done" && "Update complete"}
            </h2>
            {updateStatus === "downloading" && (
              <div className="w-full px-4">
                <div className="h-2 w-full overflow-hidden rounded-full bg-slate-700/50">
                  <div
                    className={cn(
                      "h-full rounded-full bg-blue-400 transition-[width] duration-300",
                      downloadPercent === null && "animate-pulse"
                    )}
                    style={{ width: `${downloadPercent ?? 100}%` }}
                  />
                </div>
                {downloadPercent !== null && (
                  <p className="mt-1.5 text-xs text-slate-400">{downloadPercent}%</p>
                )}
              </div>
            )}
            <p className="text-sm text-slate-400">
              {updateStatus === "restarting" &&
                "Applying update — this takes about 30 seconds."}
              {updateStatus === "reconnecting" && "Don't close this tab."}
              {updateStatus === "done" ? (
                <>
                  Now running <span className="font-mono text-slate-200">{installedVersion ?? version}</span>.
                </>
              ) : (
                updateStatus !== "restarting" &&
                updateStatus !== "reconnecting" &&
                (updateMessage || "Don't close this tab.")
              )}
            </p>
          </div>
        </Modal>
      )}
    </>
  )
}
