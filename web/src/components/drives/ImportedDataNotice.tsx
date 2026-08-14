import { useState } from "react"
import { CloseIcon, InfoIcon } from "@/components/icons"
import { BannerStack } from "@/components/ui/Banner"

const DISMISS_KEY = "drives.importedDiscrepancyDismissed"

/**
 * Dismissible warning that imported providers may group drives differently.
 * Counts reflect the current filter window and the choice persists locally.
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
  // Preserve one decimal below 10 percent.
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
