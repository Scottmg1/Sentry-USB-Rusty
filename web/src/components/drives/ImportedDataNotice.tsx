import { useState } from "react"
import { CloseIcon, InfoIcon } from "@/components/icons"
import { BannerStack } from "@/components/ui/Banner"

const DISMISS_KEY = "drives.importedDiscrepancyDismissed"

/**
 * Heads-up banner shown on the Drives page when the library contains
 * imported (Tessie/Teslascope) drives. Rusty, Sentry Cloud, and Sentry
 * Drive each ingest and group clips a little differently — and imported
 * drives come from an external service's API rather than the dashcam —
 * so the headline totals can differ by a small amount across the three.
 * Dismissible; the choice persists in localStorage.
 *
 * `count` / `importedCount` are the filtered-window drive counts from
 * `DrivesFilteredStats` (importedCount == tessieCount: every non-SEI
 * drive). When there are no imported drives the three apps agree to
 * floating-point precision, so the banner stays hidden.
 */
export function ImportedDataNotice({
  count,
  importedCount,
}: {
  count: number
  importedCount: number
}) {
  const [dismissed, setDismissed] = useState(() => {
    try {
      return localStorage.getItem(DISMISS_KEY) === "1"
    } catch {
      return false
    }
  })

  if (importedCount <= 0 || count <= 0 || dismissed) return null

  const sharePct = (importedCount / count) * 100
  // Show one decimal under 10% so a handful of imports doesn't read "0%".
  const shareLabel = sharePct >= 10 ? Math.round(sharePct).toString() : sharePct.toFixed(1)

  const dismiss = () => {
    try {
      localStorage.setItem(DISMISS_KEY, "1")
    } catch {
      /* private mode / storage disabled — dismiss for this session only */
    }
    setDismissed(true)
  }

  return (
    <div className="mb-4">
    <BannerStack
      banners={[
        {
          id: "imported-discrepancy",
          kind: "info",
          icon: <InfoIcon className="h-4 w-4" />,
          title: "Totals may differ slightly from Sentry Cloud & Sentry Drive",
          sub: (
            <>
              About <span className="font-semibold text-slate-200">{shareLabel}%</span> of
              these drives are imported (Tessie/Teslascope). Rusty, Cloud, and Sentry Drive
              each pull and group drive data a little differently, so distance, drive count,
              and time can vary by a small amount — usually well under 1%.
            </>
          ),
          action: (
            <button
              type="button"
              onClick={dismiss}
              aria-label="Dismiss notice"
              className="banner-icon shrink-0 rounded-md text-slate-400 transition-colors hover:bg-white/10 hover:text-slate-200"
            >
              <CloseIcon className="h-4 w-4" />
            </button>
          ),
        },
      ]}
    />
    </div>
  )
}
