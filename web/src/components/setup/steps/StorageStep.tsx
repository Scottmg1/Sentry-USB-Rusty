import { useState, useEffect } from "react"
import { HardDrive, RefreshCw } from "lucide-react"
import type { StepProps } from "../SetupWizard"
import { SizeInput } from "../SizeInput"

interface BlockDevice {
  path: string
  name: string
  size_gb: string
  model: string
}

export function StorageStep({ data, onChange }: StepProps) {
  const [devices, setDevices] = useState<BlockDevice[]>([])
  const [loadingDevices, setLoadingDevices] = useState(false)

  async function fetchDevices() {
    setLoadingDevices(true)
    try {
      const res = await fetch("/api/system/block-devices")
      const data = await res.json()
      setDevices(Array.isArray(data) ? data : [])
    } catch { setDevices([]) }
    setLoadingDevices(false)
  }

  useEffect(() => { fetchDevices() }, [])

  // Calculate dashcam warning — only meaningful for GB values
  const camRaw = data.CAM_SIZE ?? ""
  const camIsGB = !/[mM]$/.test(camRaw)
  const camSize = parseInt(camRaw.replace(/[^0-9]/g, "") || "0")
  const camWarning = camIsGB && camSize >= 100
    ? "Large dashcam sizes leave very little room for snapshots. The car saves ~1 hour of recent footage and rotates it. If there's not enough free space for snapshots, you won't see clips beyond the last hour. We recommend 40-60 GB for dashcam and leaving the rest for snapshots."
    : camIsGB && camSize >= 80
      ? "Consider leaving more space for snapshots. If the dashcam partition is too large, the car's recent clips may not save properly."
      : undefined

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-2">
        <HardDrive className="h-4 w-4 text-blue-400" />
        <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
          Drive Sizes
        </h3>
      </div>

      <p className="text-xs text-slate-500">
        Configure the size of each USB drive partition. Choose GB or MB per drive.
        A 128GB+ SD card is recommended. The remaining space is used for snapshots.
      </p>

      <div className="grid gap-3">
        <SizeInput
          label="Dashcam Size"
          field="CAM_SIZE"
          data={data}
          onChange={onChange}
          defaultVal="40"
          hint="Storage for TeslaCam recordings (~7-10 GB per hour). Do NOT use your entire drive — leave room for snapshots so recent clips save properly."
          warning={camWarning}
        />
        <SizeInput
          label="Music"
          field="MUSIC_SIZE"
          data={data}
          onChange={onChange}
          defaultVal=""
          hint="Optional. Leave empty for no music drive."
        />
        {(data.MUSIC_SIZE ?? "").replace(/[^0-9]/g, "") && (
          <div className="rounded-lg border border-white/5 bg-white/[0.02] p-4">
            <div className="mb-2">
              <label className="text-sm font-medium text-slate-300">Music Share Name</label>
              <span className="ml-2 text-xs text-slate-600">(optional)</span>
            </div>
            <input
              type="text"
              value={data.MUSIC_SHARE_NAME ?? ""}
              onChange={(e) => onChange("MUSIC_SHARE_NAME", e.target.value)}
              placeholder={(data.ARCHIVE_SYSTEM ?? "cifs") === "rsync" ? "/mnt/user/music" : "e.g. Music or Media/Music"}
              className="w-full rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-sm text-slate-100 placeholder-slate-600 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"
            />
            <p className="mt-1 text-xs text-slate-600">
              {(data.ARCHIVE_SYSTEM ?? "cifs") === "rsync"
                ? "The absolute path to your music folder on the rsync server (e.g. /mnt/user/music). Leave empty to skip music syncing."
                : "The share name on your Archive Server where your music is stored. Leave empty to skip music syncing — the drive will be created but not auto-synced."}
            </p>
          </div>
        )}
        <SizeInput
          label="LightShow"
          field="LIGHTSHOW_SIZE"
          data={data}
          onChange={onChange}
          defaultVal=""
          hint="Optional. Leave empty for no lightshow drive."
        />
        <SizeInput
          label="Boombox"
          field="BOOMBOX_SIZE"
          data={data}
          onChange={onChange}
          defaultVal=""
          hint="Optional. Leave empty for no boombox drive."
        />
      </div>

      {/* Data Drive */}
      <div>
        <label className="mb-1 block text-sm font-medium text-slate-300">
          External Data Drive
        </label>
        <div className="flex gap-2">
          <select
            value={data.DATA_DRIVE ?? ""}
            onChange={(e) => onChange("DATA_DRIVE", e.target.value)}
            className="flex-1 rounded-lg border border-white/10 bg-slate-900 px-3 py-2 text-sm text-slate-100 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25 [&>option]:bg-slate-900 [&>option]:text-slate-100"
          >
            <option value="">None (use SD card)</option>
            {devices.map((d) => (
              <option key={d.path} value={d.path}>{d.name}</option>
            ))}
          </select>
          <button
            type="button"
            onClick={fetchDevices}
            disabled={loadingDevices}
            className="flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10 disabled:opacity-50"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${loadingDevices ? "animate-spin" : ""}`} />
            Refresh
          </button>
        </div>
        <p className="mt-1 text-xs text-slate-600">
          Optional. Use an external USB or NVMe drive instead of the SD card.
          <span className="font-medium text-amber-400"> WARNING: The selected drive will be wiped.</span>
        </p>
      </div>

      {/* ExFAT toggle */}
      <label className="flex cursor-pointer items-center gap-2">
        <input
          type="checkbox"
          checked={(data.USE_EXFAT ?? "true") === "true"}
          onChange={(e) =>
            onChange("USE_EXFAT", e.target.checked ? "true" : "false")
          }
          className="h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500"
        />
        <span className="text-sm text-slate-300">Use ExFAT filesystem</span>
      </label>
    </div>
  )
}
