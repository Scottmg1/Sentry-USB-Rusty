import { useCallback, useEffect, useState } from "react"
import type { WifiFirmwareStatus } from "@/components/dashboard/WifiFirmwareModal"

const DISMISS_KEY = "sentryusb.wifiFirmwareDismissed"

interface WifiFirmware {
  status: WifiFirmwareStatus | null
  /** True when the banner should be offered: eligible, and not dismissed. */
  show: boolean
  dismiss: () => void
  refresh: () => void
}

/**
 * Backs the Pi 5 Wi-Fi firmware banner. The endpoint answers `eligible: false`
 * on every other board and once the newer firmware is installed, so the banner
 * disappears on its own after a successful update.
 *
 * Dismissal is remembered per target version, so a future firmware revision
 * can still surface a banner to someone who waved this one away.
 */
export function useWifiFirmware(): WifiFirmware {
  const [status, setStatus] = useState<WifiFirmwareStatus | null>(null)
  const [dismissed, setDismissed] = useState<string>(
    () => localStorage.getItem(DISMISS_KEY) ?? ""
  )

  const refresh = useCallback(() => {
    fetch("/api/system/wifi-firmware")
      .then((r) => r.json())
      .then((d: WifiFirmwareStatus) => setStatus(d))
      .catch(() => {})
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  const dismiss = useCallback(() => {
    const v = status?.target_version ?? "1"
    localStorage.setItem(DISMISS_KEY, v)
    setDismissed(v)
  }, [status])

  const show =
    !!status && status.eligible && dismissed !== status.target_version

  return { status, show, dismiss, refresh }
}
