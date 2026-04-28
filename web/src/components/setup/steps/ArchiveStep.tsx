import { useEffect, useState } from "react"
import { Archive, Loader2, CheckCircle, XCircle, HardDrive, Save } from "lucide-react"
import type { StepProps } from "../SetupWizard"
import { SecretInput } from "../SecretInput"
import { NasSSHKey } from "../NasSSHKey"
import { cn } from "@/lib/utils"

const archiveSystems = [
  { id: "cifs", label: "CIFS / SMB", desc: "Windows/Mac file sharing" },
  { id: "rsync", label: "rsync", desc: "SSH-based file sync" },
  { id: "rclone", label: "rclone", desc: "Cloud storage (Google Drive, S3, etc.)" },
  { id: "nfs", label: "NFS", desc: "Network File System" },
  { id: "none", label: "None", desc: "No archiving, local storage only" },
]

function Field({
  label,
  field,
  type = "text",
  placeholder,
  data,
  onChange,
  hint,
  error,
}: {
  label: string
  field: string
  type?: string
  placeholder?: string
  data: StepProps["data"]
  onChange: StepProps["onChange"]
  hint?: string
  error?: boolean
}) {
  const inputCls = cn(
    "w-full rounded-lg border bg-white/5 px-3 py-2 text-sm text-slate-100 placeholder-slate-600 outline-none transition focus:ring-1",
    error
      ? "border-red-500/50 focus:border-red-500/50 focus:ring-red-500/25"
      : "border-white/10 focus:border-blue-500/50 focus:ring-blue-500/25"
  )
  return (
    <div>
      <label className="mb-1 block text-sm font-medium text-slate-300">
        {label}
      </label>
      {type === "password" ? (
        <SecretInput
          value={data[field] ?? ""}
          onChange={(v) => onChange(field, v)}
          placeholder={placeholder}
          className={cn(inputCls, "pr-8")}
        />
      ) : (
        <input
          type={type}
          value={data[field] ?? ""}
          onChange={(e) => onChange(field, e.target.value)}
          placeholder={placeholder}
          className={inputCls}
        />
      )}
      {hint && <p className="mt-1 text-xs text-slate-600">{hint}</p>}
    </div>
  )
}

export function ArchiveStep({ data, onChange }: StepProps) {
  const system = data.ARCHIVE_SYSTEM ?? "cifs"
  const [testing, setTesting] = useState(false)
  const [testStage, setTestStage] = useState<string | null>(null)
  const [testResult, setTestResult] = useState<{ success: boolean; error?: string } | null>(null)
  // Lifted to ArchiveStep so the displayed key survives switching between
  // archive systems (CIFS ↔ rsync) — the file on disk persists either way,
  // this just keeps the visible state sticky across the conditional render.
  const [pubKey, setPubKey] = useState<string | null>(null)

  // Backend broadcasts `archive_test_status` for the long-running stages of
  // Test Connection — specifically the on-demand `apt-get install` of
  // nfs-common / cifs-utils when the userspace mount helper is missing.
  // Without this, the button just sits on "Testing..." for up to 4 minutes
  // (apt can wait on the dpkg frontend lock if setup is concurrently
  // running) with no indication of what's actually happening.
  useEffect(() => {
    if (!testing) return
    let ws: WebSocket | null = null
    let cancelled = false
    try {
      const protocol = window.location.protocol === "https:" ? "wss:" : "ws:"
      ws = new WebSocket(`${protocol}//${window.location.host}/api/ws`)
      ws.onmessage = (event) => {
        if (cancelled) return
        try {
          const msg = JSON.parse(event.data)
          if (msg.type !== "archive_test_status") return
          const d = msg.data ?? {}
          if (d.stage === "installing") {
            setTestStage(`Installing ${d.package ?? "package"}...`)
          } else if (d.stage === "testing") {
            setTestStage("Probing mount...")
          }
        } catch { /* ignore */ }
      }
    } catch { /* ws unavailable — button stays on "Testing..." */ }
    return () => { cancelled = true; ws?.close() }
  }, [testing])

  function req(field: string, systems: string[]): boolean {
    return systems.includes(system) && !data[field]?.trim()
  }

  function canTest(): boolean {
    if (system === "none") return false
    if (system === "cifs") return !!(data.ARCHIVE_SERVER?.trim() && data.SHARE_NAME?.trim() && data.SHARE_USER?.trim() && data.SHARE_PASSWORD?.trim())
    if (system === "rsync") return !!(data.RSYNC_SERVER?.trim() && data.RSYNC_USER?.trim() && data.RSYNC_PATH?.trim())
    if (system === "rclone") return !!(data.RCLONE_DRIVE?.trim() && data.RCLONE_PATH?.trim())
    if (system === "nfs") return !!(data.ARCHIVE_SERVER?.trim() && data.SHARE_NAME?.trim())
    return false
  }

  async function handleTest() {
    setTesting(true)
    setTestStage(null)
    setTestResult(null)
    try {
      const res = await fetch("/api/setup/test-archive", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(data),
      })
      const result = await res.json()
      setTestResult(result)
    } catch {
      setTestResult({ success: false, error: "Unable to connect to device" })
    }
    setTesting(false)
    setTestStage(null)
  }

  return (
    <div className="space-y-6">
      <div className="flex items-center gap-2">
        <Archive className="h-4 w-4 text-blue-400" />
        <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
          Archive System
        </h3>
      </div>

      <p className="text-xs text-slate-500">
        Choose how recorded clips are automatically backed up when you connect
        to WiFi.
      </p>

      {/* System selector */}
      <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
        {archiveSystems.map((s) => (
          <button
            key={s.id}
            onClick={() => { onChange("ARCHIVE_SYSTEM", s.id); setTestResult(null) }}
            className={cn(
              "rounded-lg border p-3 text-left transition-colors",
              system === s.id
                ? "border-blue-500/40 bg-blue-500/10"
                : "border-white/5 bg-white/[0.02] hover:border-white/10 hover:bg-white/[0.04]"
            )}
          >
            <p
              className={cn(
                "text-sm font-medium",
                system === s.id ? "text-blue-400" : "text-slate-300"
              )}
            >
              {s.label}
            </p>
            <p className="mt-0.5 text-xs text-slate-600">{s.desc}</p>
          </button>
        ))}
      </div>

      {/* Dynamic fields per archive system */}
      {system === "cifs" && (
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Archive Server" field="ARCHIVE_SERVER" placeholder="hostname or IP" data={data} onChange={onChange} error={req("ARCHIVE_SERVER", ["cifs"])} />
          <Field label="Share Name" field="SHARE_NAME" placeholder="share/path" data={data} onChange={onChange} error={req("SHARE_NAME", ["cifs"])} />
          <Field label="Username" field="SHARE_USER" placeholder="username" data={data} onChange={onChange} error={req("SHARE_USER", ["cifs"])} />
          <Field label="Password" field="SHARE_PASSWORD" type="password" placeholder="password" data={data} onChange={onChange} error={req("SHARE_PASSWORD", ["cifs"])} />
          <Field label="Domain" field="SHARE_DOMAIN" placeholder="optional" data={data} onChange={onChange} hint="Usually not needed" />
          <Field label="CIFS Version" field="CIFS_VERSION" placeholder="3.0" data={data} onChange={onChange} hint="Usually not needed" />
        </div>
      )}

      {system === "rsync" && (
        <>
          <div className="grid gap-3 sm:grid-cols-2">
            <Field label="Server" field="RSYNC_SERVER" placeholder="hostname or IP" data={data} onChange={onChange} error={req("RSYNC_SERVER", ["rsync"])} />
            <Field label="Username" field="RSYNC_USER" placeholder="username" data={data} onChange={onChange} error={req("RSYNC_USER", ["rsync"])} />
            <Field label="Remote Path" field="RSYNC_PATH" placeholder="/path/on/server" data={data} onChange={onChange} error={req("RSYNC_PATH", ["rsync"])} />
          </div>
          <NasSSHKey pubKey={pubKey} setPubKey={setPubKey} />
        </>
      )}

      {system === "rclone" && (
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Remote Name" field="RCLONE_DRIVE" placeholder="remotename" data={data} onChange={onChange} error={req("RCLONE_DRIVE", ["rclone"])} />
          <Field label="Remote Path" field="RCLONE_PATH" placeholder="remotepath" data={data} onChange={onChange} error={req("RCLONE_PATH", ["rclone"])} />
          <Field label="Archive Server" field="ARCHIVE_SERVER" placeholder="8.8.8.8" data={data} onChange={onChange} hint="For connectivity checks" />
        </div>
      )}

      {system === "nfs" && (
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="NFS Server" field="ARCHIVE_SERVER" placeholder="hostname or IP" data={data} onChange={onChange} error={req("ARCHIVE_SERVER", ["nfs"])} />
          <Field label="Export Path" field="SHARE_NAME" placeholder="/volume1/TeslaCam" data={data} onChange={onChange} hint="Exact export path on the NAS" error={req("SHARE_NAME", ["nfs"])} />
        </div>
      )}

      {/* Test Connection */}
      {system !== "none" && (
        <div className="flex items-center gap-3">
          <button
            onClick={handleTest}
            disabled={testing || !canTest()}
            className={cn(
              "flex items-center gap-1.5 rounded-lg border px-4 py-2 text-sm font-medium transition-colors",
              testing || !canTest()
                ? "cursor-not-allowed border-white/5 text-slate-600"
                : "border-white/10 text-slate-300 hover:bg-white/5 hover:text-slate-100"
            )}
          >
            {testing ? (
              <>
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                {testStage ?? "Testing..."}
              </>
            ) : (
              "Test Connection"
            )}
          </button>
          {testResult && (
            <div className={cn("flex items-center gap-1.5 text-sm", testResult.success ? "text-emerald-400" : "text-red-400")}>
              {testResult.success ? (
                <>
                  <CheckCircle className="h-4 w-4" />
                  Connection successful
                </>
              ) : (
                <>
                  <XCircle className="h-4 w-4 shrink-0" />
                  <span className="line-clamp-2">{testResult.error || "Connection failed"}</span>
                </>
              )}
            </div>
          )}
        </div>
      )}

      {/* Archive options */}
      {system !== "none" && (
        <div className="space-y-2">
          <p className="text-xs font-medium uppercase tracking-wider text-slate-500">
            What to Archive
          </p>
          {[
            { field: "ARCHIVE_SAVEDCLIPS", label: "Saved Clips", def: "true" },
            { field: "ARCHIVE_SENTRYCLIPS", label: "Sentry Clips", def: "true" },
            { field: "ARCHIVE_RECENTCLIPS", label: "Recent Clips", def: "true" },
            { field: "ARCHIVE_TRACKMODECLIPS", label: "Track Mode Clips", def: "true" },
          ].map(({ field, label, def }) => (
            <label key={field} className="flex items-center gap-2">
              <input
                type="checkbox"
                checked={(data[field] ?? def) === "true"}
                onChange={(e) => onChange(field, e.target.checked ? "true" : "false")}
                className="h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500"
              />
              <span className="text-sm text-slate-300">{label}</span>
            </label>
          ))}
        </div>
      )}

      {/* Config backup location */}
      <div className="space-y-2 rounded-lg border border-white/5 bg-white/[0.02] p-4">
        <div className="flex items-center gap-2">
          <Save className="h-4 w-4 text-blue-400" />
          <p className="text-xs font-medium uppercase tracking-wider text-slate-500">
            Config Backup Location
          </p>
        </div>
        <p className="text-xs text-slate-500">
          Your configuration is automatically backed up after each archive.
          Choose where to store backups for easy recovery if the SD card fails.
        </p>
        <div className="grid grid-cols-2 gap-2">
          <button
            type="button"
            onClick={() => onChange("_BACKUP_LOCATION", "archive")}
            className={cn(
              "rounded-xl border px-3 py-3 text-left text-xs transition-all",
              (data._BACKUP_LOCATION ?? "archive") === "archive"
                ? "border-blue-500/40 bg-blue-500/10 text-blue-400 shadow-[0_0_12px_rgba(59,130,246,0.1)]"
                : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05] hover:border-white/10"
            )}
          >
            <Archive className="mb-1 h-4 w-4" />
            <span className="font-semibold">Archive Server</span>
            <br />
            <span className="text-[10px] opacity-60">Same location as footage</span>
          </button>
          <button
            type="button"
            onClick={() => onChange("_BACKUP_LOCATION", "ssd")}
            className={cn(
              "rounded-xl border px-3 py-3 text-left text-xs transition-all",
              data._BACKUP_LOCATION === "ssd"
                ? "border-blue-500/40 bg-blue-500/10 text-blue-400 shadow-[0_0_12px_rgba(59,130,246,0.1)]"
                : "border-white/5 bg-white/[0.02] text-slate-400 hover:bg-white/[0.05] hover:border-white/10"
            )}
          >
            <HardDrive className="mb-1 h-4 w-4" />
            <span className="font-semibold">Local SSD</span>
            <br />
            <span className="text-[10px] opacity-60">On the data drive</span>
          </button>
        </div>
      </div>
    </div>
  )
}
