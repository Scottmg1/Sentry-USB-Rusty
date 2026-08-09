import { useState, useEffect } from "react"
import { CheckCircleIcon, DownloadIcon, ErrorIcon, ProgressActivityIcon } from "@/components/icons"
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
  const [showRestartModal, setShowRestartModal] = useState(false)

  useEffect(() => {
    if (updateStatus === "restarting" || updateStatus === "reconnecting") {
      setShowRestartModal(true)
    }
  }, [updateStatus])

  useEffect(() => {
    if (!showRestartModal) return
    if (updateStatus === "done") {
      const t = setTimeout(() => setShowRestartModal(false), 3000)
      return () => clearTimeout(t)
    }
    if (updateStatus === "idle" || updateStatus === "error") {
      setShowRestartModal(false)
    }
  }, [showRestartModal, updateStatus])

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
    // Track the version we're installing so the success modal and message
    // can show it without trusting /api/system/version — the OLD daemon
    // answers that endpoint until reboot fires and can return a stale tag.
    const preUpdateVersion = version
    let newVersion: string | null = targetVersion ?? null
    setInstalledVersion(newVersion)

    const unsubscribe = wsClient.subscribe("update_status", (data: unknown) => {
      const msg = data as { status?: string; message?: string; error?: string; output?: string }
      if (msg.error) {
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
      if (msg.status === "complete" && msg.output) {
        const m = msg.output.match(/Updated to (\S+?)\.?\s*$/)
        if (m) {
          newVersion = m[1]
          setInstalledVersion(newVersion)
        }
      }
      if (msg.message) {
        setUpdateMessage(msg.message)
      }
    })

    try {
      const checkRes = await fetch("/api/system/check-internet")
      const checkData = await checkRes.json()
      if (!checkData.connected) {
        setUpdateStatus("error")
        setUpdateError("No internet connection. Connect to WiFi first.")
        setUpdateMessage(null)
        unsubscribe()
        return
      }

      const res = await fetch("/api/system/update", {
        method: "POST",
        headers: targetVersion ? { "Content-Type": "application/json" } : {},
        body: targetVersion ? JSON.stringify({ version: targetVersion }) : undefined,
      })
      if (!res.ok) throw new Error("Failed to start update")

      let reconnected = false
      setTimeout(() => {
        unsubscribe()
        setUpdateStatus("reconnecting")
        setUpdateMessage("Waiting for device to come back online...")

        const pollInterval = setInterval(async () => {
          try {
            const r = await fetch("/api/system/version")
            if (r.ok) {
              const data = await r.json()
              // Reject stale responses from the OLD daemon — it stays
              // responsive until `reboot` fires and may answer before
              // /opt/sentryusb/version has been rewritten with the new
              // tag. Wait for either the expected new version or any
              // version distinct from the pre-update one.
              const polled = (data.version || "").trim()
              const matchesNew = newVersion && polled === newVersion
              const differsFromOld = preUpdateVersion && polled && polled !== preUpdateVersion
              if (!matchesNew && !differsFromOld) return
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
            }
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
      }, 20000)
    } catch (err) {
      unsubscribe()
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
      <ErrorIcon className="h-3.5 w-3.5" />
    ) : updateStatus === "done" ? (
      <CheckCircleIcon className="h-3.5 w-3.5" />
    ) : installInProgress ? (
      <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />
    ) : (
      <DownloadIcon className="h-3.5 w-3.5" />
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
              <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" /> Checking
            </span>
          ) : (
            "Check for Updates"
          )}
        </button>
      </PrefCard>

      <PrefCard
        icon={<DownloadIcon className="h-3.5 w-3.5" />}
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

      {showRestartModal && (
        <Modal
          title="Restarting"
          onClose={() => setShowRestartModal(false)}
          dismissable={false}
          size="sm"
        >
          <div className="flex flex-col items-center gap-3 py-6 text-center">
            {updateStatus === "done" ? (
              <CheckCircleIcon className="h-12 w-12 text-emerald-400" />
            ) : (
              <ProgressActivityIcon className="h-12 w-12 animate-spin text-blue-400" />
            )}
            <h2 className="text-lg font-semibold text-slate-100">
              {updateStatus === "restarting" && "Restarting Pi"}
              {updateStatus === "reconnecting" && "Waiting for Pi to come back online"}
              {updateStatus === "done" && "Update complete"}
            </h2>
            <p className="text-sm text-slate-400">
              {updateStatus === "restarting" &&
                "Applying update — this takes about 30 seconds."}
              {updateStatus === "reconnecting" && "Don't close this tab."}
              {updateStatus === "done" && (
                <>
                  Now running <span className="font-mono text-slate-200">{installedVersion ?? version}</span>.
                </>
              )}
            </p>
          </div>
        </Modal>
      )}
    </>
  )
}
