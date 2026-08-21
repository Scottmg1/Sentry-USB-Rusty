import { useCallback, useEffect, useRef, useState } from "react"
import {
  CheckCircleIcon,
  ErrorIcon,
  ProgressActivityIcon,
  WarningIcon,
  WifiIcon,
} from "@/components/icons"
import { Modal } from "@/components/ui/Modal"
import { wsClient } from "@/lib/ws"
import { cn } from "@/lib/utils"

export interface WifiFirmwareStatus {
  eligible: boolean
  supported_board: boolean
  model: string
  running_version: string | null
  installed_version: string | null
  target_version: string
  up_to_date: boolean
  symptom_detected: boolean
  symptom_detail: string | null
  can_rollback: boolean
  /** Held in place with dpkg-divert so an apt upgrade can't revert it. */
  pinned: boolean
  install: InstallState
}

interface InstallState {
  state: "idle" | "running" | "success" | "failed" | "rolled_back"
  step: string
  progress: number
  message: string
  updated_at: number
}

const IDLE: InstallState = {
  state: "idle",
  step: "",
  progress: 0,
  message: "",
  updated_at: 0,
}

/**
 * Offers the newer Broadcom Wi-Fi firmware on a Pi 5.
 *
 * The install reloads the radio, so Wi-Fi — and therefore this page's own
 * connection — drops for roughly 20 seconds partway through. The backend runs
 * detached and persists each step, so the UI keeps polling through the outage
 * and picks the result back up instead of showing a dead progress bar.
 */
export function WifiFirmwareModal({
  status,
  onClose,
  onRefresh,
}: {
  status: WifiFirmwareStatus
  onClose: () => void
  onRefresh: () => void
}) {
  const [install, setInstall] = useState<InstallState>(status.install ?? IDLE)
  const [starting, setStarting] = useState(false)
  const [startError, setStartError] = useState("")
  const [rebooting, setRebooting] = useState(false)
  const pollRef = useRef<number | null>(null)

  // Trust the server's view as well as our local one: if the parent polled and
  // found an install still in flight, this modal must stay locked even if our
  // own copy of the state is momentarily stale.
  const running =
    install.state === "running" || starting || status.install?.state === "running"
  const finished =
    install.state === "success" ||
    install.state === "failed" ||
    install.state === "rolled_back"

  // Live updates while the connection is up…
  useEffect(() => {
    return wsClient.subscribe("wifi_firmware_status", (msg: unknown) => {
      setInstall(msg as InstallState)
    })
  }, [])

  // …and polling to carry us across the Wi-Fi drop, when the WebSocket is gone.
  useEffect(() => {
    if (!running) {
      if (pollRef.current) {
        window.clearInterval(pollRef.current)
        pollRef.current = null
      }
      return
    }
    pollRef.current = window.setInterval(() => {
      fetch("/api/system/wifi-firmware")
        .then((r) => r.json())
        .then((d: WifiFirmwareStatus) => {
          if (d.install) setInstall(d.install)
        })
        .catch(() => {
          /* expected while the radio is down — keep polling */
        })
    }, 2000)
    return () => {
      if (pollRef.current) window.clearInterval(pollRef.current)
      pollRef.current = null
    }
  }, [running])

  // Refresh the parent's banner once the dust settles.
  useEffect(() => {
    if (finished) onRefresh()
  }, [finished, onRefresh])

  const start = useCallback(() => {
    setStarting(true)
    setStartError("")
    fetch("/api/system/wifi-firmware/install", { method: "POST" })
      .then(async (r) => {
        if (!r.ok) {
          const body = await r.json().catch(() => ({}))
          throw new Error(body.error || "Could not start the update.")
        }
        setInstall({ ...IDLE, state: "running", message: "Starting…" })
      })
      .catch((e: Error) => setStartError(e.message))
      .finally(() => setStarting(false))
  }, [])

  const rollback = useCallback(() => {
    setStarting(true)
    fetch("/api/system/wifi-firmware/rollback", { method: "POST" })
      .then(() => setInstall({ ...IDLE, state: "running", message: "Restoring…" }))
      .catch(() => {})
      .finally(() => setStarting(false))
  }, [])

  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <WifiIcon className="h-4 w-4 text-amber-400" />
          <span>Wi-Fi firmware update</span>
        </span>
      }
      // Never let a click on the backdrop, Esc, or the close button dismiss
      // this mid-install: the radio reload takes the page's own connection
      // down, and a user who loses the progress view assumes it finished.
      onClose={running ? () => {} : onClose}
      dismissable={!running}
      size="md"
      footer={
        <div className="flex items-center justify-between gap-3">
          <span className="text-[11px] text-slate-500">
            {status.running_version
              ? `Running ${status.running_version} → ${status.target_version}`
              : `Target ${status.target_version}`}
          </span>
          <div className="flex gap-2">
            {!running && install.state !== "success" && (
              <button
                onClick={onClose}
                className="rounded-lg bg-slate-500/15 px-3 py-1.5 text-xs font-medium text-slate-300 hover:bg-slate-500/25"
              >
                Not now
              </button>
            )}
            {install.state === "success" ? (
              <button
                onClick={onClose}
                className="rounded-lg bg-emerald-500/15 px-3 py-1.5 text-xs font-medium text-emerald-400 hover:bg-emerald-500/25"
              >
                Done
              </button>
            ) : install.state === "rolled_back" || install.state === "failed" ? (
              <button
                onClick={start}
                disabled={running}
                className="rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 hover:bg-blue-500/25 disabled:opacity-50"
              >
                Try again
              </button>
            ) : (
              <button
                onClick={start}
                disabled={running}
                className="rounded-lg bg-amber-500/15 px-3 py-1.5 text-xs font-medium text-amber-400 hover:bg-amber-500/25 disabled:opacity-50"
              >
                {running ? "Updating…" : "Update firmware"}
              </button>
            )}
          </div>
        </div>
      }
    >
      <div className="flex flex-col gap-3 text-xs leading-relaxed text-slate-400">
        {install.state === "idle" && (
          <>
            <p>
              Your {status.model || "Pi"} is running the Wi-Fi firmware that ships with
              Raspberry Pi OS
              (<span className="text-slate-300">{status.running_version ?? "unknown"}</span>).
              A newer build from Infineon
              (<span className="text-slate-300">{status.target_version}</span>) fixes a fault
              seen during long archive transfers.
            </p>

            <div className="rounded-lg bg-slate-500/10 p-2.5">
              <p className="mb-1 font-medium text-slate-300">Update if you have seen any of this:</p>
              <ul className="list-disc space-y-0.5 pl-4">
                <li>Archiving suddenly crawls — a few Mbit/s — while Wi-Fi still shows connected with a strong signal.</li>
                <li>Bluetooth keep-awake starts failing at the same moment, so the car falls asleep mid-archive.</li>
                <li>Only a reboot brings the speed back, and it comes back slower than it used to be.</li>
              </ul>
            </div>

            {status.symptom_detected && (
              <div className="flex items-start gap-2 rounded-lg bg-amber-500/10 p-2.5 text-amber-300">
                <WarningIcon className="mt-0.5 h-3.5 w-3.5 shrink-0" />
                <span>
                  We found this on your device: {status.symptom_detail}. Updating is recommended.
                </span>
              </div>
            )}

            <p className="text-[11px] text-slate-500">
              Wi-Fi drops for about 20 seconds while the radio reloads, so this page may briefly
              lose connection — it will reconnect on its own. If the new firmware does not work,
              the previous one is restored automatically. Recording is not interrupted. The
              firmware is held in place so a later system update can't revert it, and you can
              undo it here at any time.
            </p>
          </>
        )}

        {(running || finished) && (
          <div className="flex flex-col gap-2.5 py-2">
            <div className="flex items-center gap-2">
              {install.state === "success" ? (
                <CheckCircleIcon className="h-4 w-4 shrink-0 text-emerald-400" />
              ) : install.state === "failed" ? (
                <ErrorIcon className="h-4 w-4 shrink-0 text-red-400" />
              ) : install.state === "rolled_back" ? (
                <WarningIcon className="h-4 w-4 shrink-0 text-amber-400" />
              ) : (
                <ProgressActivityIcon className="h-4 w-4 shrink-0 animate-spin text-blue-400" />
              )}
              <span
                className={cn(
                  "text-xs",
                  install.state === "success" && "text-emerald-300",
                  install.state === "failed" && "text-red-300",
                  install.state === "rolled_back" && "text-amber-300",
                  install.state === "running" && "text-slate-300"
                )}
              >
                {install.message || "Working…"}
              </span>
            </div>

            <div className="h-1.5 w-full overflow-hidden rounded-full bg-slate-500/20">
              <div
                className={cn(
                  "h-full rounded-full transition-all duration-500",
                  install.state === "success" && "bg-emerald-400",
                  install.state === "failed" && "bg-red-400",
                  install.state === "rolled_back" && "bg-amber-400",
                  install.state === "running" && "bg-blue-400"
                )}
                style={{ width: `${Math.max(install.progress, 4)}%` }}
              />
            </div>

            {running && (
              <p className="text-[11px] text-slate-500">
                Do not power off the Pi. If this page stops responding for a moment, that is the
                radio reloading — it will come back.
              </p>
            )}

            {install.state === "success" && (
              // Reloading the radio in place can leave it transmitting well
              // below normal until the chip is actually power-cycled, and that
              // outcome varies run to run. A reboot is the reliable finish.
              <div className="rounded-lg bg-amber-500/10 p-2.5 text-amber-300">
                <p className="mb-2">
                  Reboot to finish. Reloading the radio in place can leave Wi-Fi slower than
                  normal until the Pi restarts.
                </p>
                <button
                  onClick={() => {
                    setRebooting(true)
                    fetch("/api/system/reboot", { method: "POST" }).catch(() => {})
                  }}
                  disabled={rebooting}
                  className="rounded-lg bg-amber-500/20 px-3 py-1.5 text-xs font-medium text-amber-300 hover:bg-amber-500/30 disabled:opacity-50"
                >
                  {rebooting ? "Rebooting…" : "Reboot now"}
                </button>
              </div>
            )}

            {install.state === "success" && status.can_rollback && (
              <button
                onClick={rollback}
                className="self-start text-[11px] text-slate-500 underline hover:text-slate-300"
              >
                Revert to the previous firmware
              </button>
            )}
          </div>
        )}

        {startError && <p className="text-xs text-red-400">{startError}</p>}
      </div>
    </Modal>
  )
}
