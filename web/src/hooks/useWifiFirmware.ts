import { useCallback, useEffect, useRef, useState } from "react"
import type { WifiFirmwareStatus } from "@/components/dashboard/WifiFirmwareModal"

const REVERT_DISMISS_KEY = "sentryusb.wifiFirmwareRevertDismissed"

interface WifiFirmware {
  status: WifiFirmwareStatus | null
  /** Offer the update: supported board, not yet updated. */
  show: boolean
  /** An install is in flight right now. */
  installing: boolean
  /** Updated, still revertible, and the user hasn't waved the offer away. */
  offerRevert: boolean
  dismissRevert: () => void
  refresh: () => void
}

/**
 * Backs the Wi-Fi firmware banner.
 *
 * Polls while an install is running so the banner can show live progress even
 * if the modal was closed. The endpoint answers `eligible: false` on
 * unsupported boards and once the newer firmware is in place, so the offer
 * clears itself after a successful update and not before. There is
 * deliberately no dismiss on the *offer*: it is a correctness fix for a fault
 * that silently cripples archiving, so it stays until the firmware is updated.
 */
export function useWifiFirmware(): WifiFirmware {
  const [status, setStatus] = useState<WifiFirmwareStatus | null>(null)
  const [revertDismissed, setRevertDismissed] = useState<string>(
    () => localStorage.getItem(REVERT_DISMISS_KEY) ?? ""
  )
  const pollRef = useRef<number | null>(null)

  const refresh = useCallback(() => {
    fetch("/api/system/wifi-firmware")
      .then((r) => r.json())
      .then((d: WifiFirmwareStatus) => setStatus(d))
      .catch(() => {
        /* expected while the radio reloads mid-install */
      })
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  const installing = status?.install?.state === "running"

  // Keep polling through the install, including across the Wi-Fi drop, so the
  // banner's progress stays live whether or not the modal is open.
  useEffect(() => {
    if (!installing) {
      if (pollRef.current) {
        window.clearInterval(pollRef.current)
        pollRef.current = null
      }
      return
    }
    pollRef.current = window.setInterval(refresh, 2000)
    return () => {
      if (pollRef.current) window.clearInterval(pollRef.current)
      pollRef.current = null
    }
  }, [installing, refresh])

  const dismissRevert = useCallback(() => {
    const v = status?.target_version ?? "1"
    localStorage.setItem(REVERT_DISMISS_KEY, v)
    setRevertDismissed(v)
  }, [status])

  const offerRevert =
    !!status &&
    status.up_to_date &&
    status.can_rollback &&
    !installing &&
    revertDismissed !== status.target_version

  return {
    status,
    show: !!status && status.eligible && !installing,
    installing: !!installing,
    offerRevert,
    dismissRevert,
    refresh,
  }
}
