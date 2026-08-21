import { useCallback, useEffect, useState } from "react"
import type { WifiFirmwareStatus } from "@/components/dashboard/WifiFirmwareModal"

interface WifiFirmware {
  status: WifiFirmwareStatus | null
  /** True while the banner should be shown. */
  show: boolean
  refresh: () => void
}

/**
 * Backs the Wi-Fi firmware banner. The endpoint answers `eligible: false` on
 * unsupported boards and once the newer firmware is installed, so the banner
 * clears itself after a successful update and not before.
 *
 * There is deliberately no dismiss: this is a correctness fix for a fault that
 * silently halves archive speed and stops the car being kept awake, so the
 * banner stays until the firmware is actually updated.
 */
export function useWifiFirmware(): WifiFirmware {
  const [status, setStatus] = useState<WifiFirmwareStatus | null>(null)

  const refresh = useCallback(() => {
    fetch("/api/system/wifi-firmware")
      .then((r) => r.json())
      .then((d: WifiFirmwareStatus) => setStatus(d))
      .catch(() => {})
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  return { status, show: !!status && status.eligible, refresh }
}
