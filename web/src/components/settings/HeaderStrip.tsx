import { SettingsIcon } from "@/components/icons"
import { formatUptime } from "@/lib/utils"
import { useVersion } from "@/hooks/useVersion"

interface HeaderStripProps {
  hostname?: string | null
  sbc?: string | null
  uptimeSec?: number | null
}

export function HeaderStrip({
  hostname,
  sbc,
  uptimeSec,
}: HeaderStripProps) {
  const version = useVersion()

  return (
    <div className="glass-card settings-strip">
      <span className="halo-accent inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-xl">
        <SettingsIcon className="h-4.5 w-4.5" />
      </span>
      <div className="min-w-0 flex-1">
        <div className="t-md font-semibold">Settings</div>
        <div className="mt-0.5 flex flex-wrap gap-x-3.5 gap-y-1">
          {sbc && (
            <span className="t-xs">
              <span className="text-slate-500">SBC</span>{" "}
              <span className="text-slate-300">{sbc}</span>
            </span>
          )}
          {hostname && (
            <span className="t-xs">
              <span className="text-slate-500">Host</span>{" "}
              <span className="t-mono text-slate-300">{hostname}</span>
            </span>
          )}
          <span className="t-xs">
            <span className="text-slate-500">Version</span>{" "}
            <span className="t-mono text-slate-300">{version ?? "…"}</span>
          </span>
          {uptimeSec != null && uptimeSec > 0 && (
            <span className="t-xs">
              <span className="text-slate-500">Uptime</span>{" "}
              <span className="text-slate-300">{formatUptime(uptimeSec)}</span>
            </span>
          )}
        </div>
      </div>
    </div>
  )
}
