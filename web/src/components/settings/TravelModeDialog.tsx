import { useEffect, useState } from "react"
import { createPortal } from "react-dom"
import { ProgressActivityIcon, TravelIcon } from "@/components/icons"
import { Modal } from "@/components/ui/Modal"
import { Toggle } from "@/components/ui/Toggle"
import { api } from "@/lib/api"

interface Props {
  onClose: () => void
  /** Reports the latest enabled state so the parent can show its badge. */
  onChange?: (enabled: boolean) => void
}

/** "58 min" / "4 min" / "45 sec" — for showing snapshot cadences. */
function fmtInterval(sec: number): string {
  if (sec < 90) return `${Math.round(sec)} sec`
  return `${Math.round(sec / 60)} min`
}

/**
 * Hidden "secret menu" reached by tapping the Away Mode card icon 5×.
 * Toggles Travel Mode — persisted flags the archive loop reads to keep the
 * USB drive connected to the car while archiving on the road, plus an
 * optional half-interval snapshot cadence for long trips.
 */
export function TravelModeDialog({ onClose, onChange }: Props) {
  const [enabled, setEnabled] = useState(false)
  const [halfSnapshots, setHalfSnapshots] = useState(false)
  const [fastRetry, setFastRetry] = useState(false)
  // Matches archiveloop's ${SNAPSHOT_INTERVAL:-3480} until status loads.
  const [intervalSec, setIntervalSec] = useState(3480)
  const [loading, setLoading] = useState(true)
  const [saving, setSaving] = useState(false)
  const [archiving, setArchiving] = useState(false)

  useEffect(() => {
    let cancelled = false
    api
      .getTravelMode()
      .then((r) => {
        if (cancelled) return
        setEnabled(r.enabled)
        setHalfSnapshots(r.half_snapshots)
        setFastRetry(r.fast_retry)
        setIntervalSec(r.snapshot_interval_sec)
      })
      .catch(() => {})
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [])

  // Poll archive status while the dialog is open and block toggling during an
  // active archive: flipping Travel Mode mid-cycle would desync the keep-awake
  // start/stop pair for the in-flight cycle. The signal is archiveloop's
  // /tmp/archive_status.json, surfaced as DriveStatus.archiving (is_archiving()).
  useEffect(() => {
    let cancelled = false
    const check = () =>
      api
        .getDriveStatus()
        .then((s) => {
          if (!cancelled) setArchiving(Boolean(s.archiving))
        })
        .catch(() => {})
    check()
    const id = setInterval(check, 4000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [])

  async function toggle(next: boolean) {
    if (archiving) return // toggle is also disabled in the UI; belt-and-suspenders
    setSaving(true)
    setEnabled(next) // optimistic
    try {
      const r = await api.setTravelMode(next)
      setEnabled(r.enabled)
      onChange?.(r.enabled)
    } catch {
      setEnabled(!next) // revert on failure
    } finally {
      setSaving(false)
    }
  }

  async function toggleHalf(next: boolean) {
    if (archiving) return
    setSaving(true)
    setHalfSnapshots(next) // optimistic
    try {
      const r = await api.setTravelMode(enabled, next)
      setHalfSnapshots(r.half_snapshots)
      setIntervalSec(r.snapshot_interval_sec)
    } catch {
      setHalfSnapshots(!next) // revert on failure
    } finally {
      setSaving(false)
    }
  }

  async function toggleFastRetry(next: boolean) {
    if (archiving) return
    setSaving(true)
    setFastRetry(next) // optimistic
    try {
      const r = await api.setTravelMode(enabled, undefined, next)
      setFastRetry(r.fast_retry)
    } catch {
      setFastRetry(!next) // revert on failure
    } finally {
      setSaving(false)
    }
  }

  // Portal to document.body: the Away Mode card is a `glass-card` with a
  // backdrop-filter, which establishes a containing block for `position:
  // fixed` and would otherwise pin/clip this overlay inside the card.
  return createPortal(
    <Modal
      title={
        <span className="flex items-center gap-2">
          <TravelIcon className="h-4 w-4 text-sky-400" />
          Travel Mode
        </span>
      }
      onClose={onClose}
      size="sm"
    >
      <div className="space-y-4">
        <p className="t-xs">
          For road trips. Keeps archiving footage to your server in the background — over an
          always-on connection like a travel router or VPN — but{" "}
          <span className="font-medium text-slate-200">
            never disconnects the USB drive from the car
          </span>
          , so Sentry &amp; Dashcam keep recording the whole time.
        </p>

        <Toggle
          checked={enabled}
          onChange={toggle}
          disabled={loading || saving || archiving}
          label="Enable Travel Mode"
          sub={
            archiving
              ? "An archive is in progress — wait for it to finish to change Travel Mode."
              : enabled
                ? "On — the drive stays connected to the car while archiving."
                : "Off — normal behavior (the drive briefly disconnects to tidy up after archiving)."
          }
        />

        <Toggle
          checked={halfSnapshots}
          onChange={toggleHalf}
          disabled={loading || saving || archiving}
          label="How often to take snapshots"
          sub={
            halfSnapshots
              ? `Half — every ${fmtInterval(intervalSec / 2)} instead of every ${fmtInterval(intervalSec)}. Snapshots and uploads run twice as often, so less footage is at risk if the car's drive fills up on a long trip.`
              : `Default — every ${fmtInterval(intervalSec)}, same as your setup. Flip on to snapshot and upload twice as often while traveling.`
          }
        />

        <Toggle
          checked={fastRetry}
          onChange={toggleFastRetry}
          disabled={loading || saving || archiving}
          label="Retry quickly after failures"
          sub={
            fastRetry
              ? "On — if an archive attempt fails, try again about every minute until it succeeds. Great for spotty connections like Starlink, where a brief dropout under a bridge would otherwise stall archiving until the next cycle."
              : "Off — after a failed attempt, archiving waits for the next regular cycle. Flip on for spotty connections like Starlink."
          }
        />

        {archiving && (
          <div className="flex items-start gap-2 rounded-lg border border-amber-500/30 bg-amber-500/10 p-3">
            <ProgressActivityIcon className="mt-0.5 h-4 w-4 shrink-0 animate-spin text-amber-400" />
            <p className="text-xs text-amber-200/90">
              An archive cycle is running right now. To avoid interrupting it mid-cycle, Travel Mode
              can’t be changed until it finishes — this unlocks automatically in a moment.
            </p>
          </div>
        )}

        <p className="text-[10px] text-slate-500">
          While on, disk cleanup is paused (the car manages its own space) and archiving runs once
          per snapshot interval — or twice as often with the toggle above. Turn this off when
          you’re back home to resume normal cleanup.
        </p>
      </div>
    </Modal>,
    document.body,
  )
}
