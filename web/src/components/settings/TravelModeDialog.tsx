import { useEffect, useState } from "react"
import { ProgressActivityIcon, TravelIcon } from "@/components/icons"
import { Modal } from "@/components/ui/Modal"
import { Toggle } from "@/components/ui/Toggle"
import { api } from "@/lib/api"

interface Props {
  onClose: () => void
  /** Reports the latest enabled state so the parent can show its badge. */
  onChange?: (enabled: boolean) => void
}

/** Compact snapshot cadence. */
function fmtInterval(sec: number): string {
  if (sec < 90) return `${Math.round(sec)} sec`
  return `${Math.round(sec / 60)} min`
}

/** Hidden Travel Mode controls for connected, on-road archiving. */
export function TravelModeDialog({ onClose, onChange }: Props) {
  const [enabled, setEnabled] = useState(false)
  const [halfSnapshots, setHalfSnapshots] = useState(false)
  const [fastRetry, setFastRetry] = useState(false)
  // Matches the archive-loop default until status loads.
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

  // Do not change travel flags mid-archive; that would desynchronize keep-awake.
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
    if (archiving) return
    setSaving(true)
    setEnabled(next)
    try {
      const r = await api.setTravelMode(next)
      setEnabled(r.enabled)
      onChange?.(r.enabled)
    } catch {
      setEnabled(!next)
    } finally {
      setSaving(false)
    }
  }

  async function toggleHalf(next: boolean) {
    if (archiving) return
    setSaving(true)
    setHalfSnapshots(next)
    try {
      const r = await api.setTravelMode(enabled, next)
      setHalfSnapshots(r.half_snapshots)
      setIntervalSec(r.snapshot_interval_sec)
    } catch {
      setHalfSnapshots(!next)
    } finally {
      setSaving(false)
    }
  }

  async function toggleFastRetry(next: boolean) {
    if (archiving) return
    setSaving(true)
    setFastRetry(next)
    try {
      const r = await api.setTravelMode(enabled, undefined, next)
      setFastRetry(r.fast_retry)
    } catch {
      setFastRetry(!next)
    } finally {
      setSaving(false)
    }
  }

  return (
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
    </Modal>
  )
}
