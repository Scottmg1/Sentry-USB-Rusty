import { useEffect, useRef, useState } from "react"
import {
  ChevronDown,
  Download,
  Loader2,
  Play,
  Radio,
  RefreshCw,
  Trash2,
  Upload,
} from "lucide-react"
import { cn } from "@/lib/utils"
import {
  deleteAllDrives,
  triggerProcessNew,
  triggerReprocessAll,
  triggerSummonCheck,
  uploadDriveData,
} from "@/api/drives"

interface DrivesActionsBarProps {
  onChanged: () => void
}

export function DrivesActionsBar({ onChanged }: DrivesActionsBarProps) {
  const [processMenuOpen, setProcessMenuOpen] = useState(false)
  const [processing, setProcessing] = useState(false)
  const [importing, setImporting] = useState(false)
  const [deleting, setDeleting] = useState(false)
  const [confirmingDelete, setConfirmingDelete] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const menuRef = useRef<HTMLDivElement>(null)
  const fileInputRef = useRef<HTMLInputElement>(null)

  useEffect(() => {
    if (!processMenuOpen) return
    const onDoc = (e: MouseEvent) => {
      if (!menuRef.current?.contains(e.target as Node)) setProcessMenuOpen(false)
    }
    document.addEventListener("mousedown", onDoc)
    return () => document.removeEventListener("mousedown", onDoc)
  }, [processMenuOpen])

  const runProcess = async (mode: "new" | "all" | "summon") => {
    setProcessMenuOpen(false)
    setProcessing(true)
    setError(null)
    try {
      if (mode === "new") await triggerProcessNew()
      else if (mode === "summon") await triggerSummonCheck()
      else await triggerReprocessAll()
      // Backend runs the job async; surface a soft hint, then refresh
      // the list so newly extracted drives appear when the user comes back.
      window.setTimeout(onChanged, 2000)
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setProcessing(false)
    }
  }

  const runImport = async (file: File) => {
    setImporting(true)
    setError(null)
    try {
      await uploadDriveData(file)
      onChanged()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setImporting(false)
      if (fileInputRef.current) fileInputRef.current.value = ""
    }
  }

  const runDelete = async () => {
    setDeleting(true)
    setError(null)
    try {
      await deleteAllDrives()
      setConfirmingDelete(false)
      onChanged()
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setDeleting(false)
    }
  }

  return (
    <>
      <div className="flex flex-wrap items-center gap-2">
        <div ref={menuRef} className="relative">
          <button
            type="button"
            disabled={processing}
            onClick={() => setProcessMenuOpen((o) => !o)}
            className="inline-flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/[0.03] px-3 py-1.5 text-xs font-medium text-slate-200 transition-colors hover:bg-white/[0.06] disabled:opacity-50"
          >
            {processing ? (
              <Loader2 className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <RefreshCw className="h-3.5 w-3.5" />
            )}
            Process
            <ChevronDown className="h-3 w-3" />
          </button>
          {processMenuOpen && !processing && (
            <div className="absolute right-0 z-50 mt-1 w-60 rounded-lg border border-white/10 bg-slate-950/95 py-1 shadow-2xl backdrop-blur">
              <MenuItem
                icon={<Play className="h-3.5 w-3.5 text-emerald-400" />}
                title="Process new drives"
                hint="Extract GPS from unprocessed clips"
                onClick={() => runProcess("new")}
              />
              <MenuItem
                icon={<RefreshCw className="h-3.5 w-3.5 text-amber-400" />}
                title="Reprocess all drives"
                hint="Re-extract every existing clip on disk"
                onClick={() => runProcess("all")}
              />
              <MenuItem
                icon={<Radio className="h-3.5 w-3.5 text-violet-400" />}
                title="Scan for Summon drives"
                hint="Re-read slow clips for summon evidence"
                onClick={() => runProcess("summon")}
              />
            </div>
          )}
        </div>

        <a
          href="/api/drives/data/download"
          className="inline-flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/[0.03] px-3 py-1.5 text-xs font-medium text-slate-200 transition-colors hover:bg-white/[0.06]"
        >
          <Download className="h-3.5 w-3.5" /> Export
        </a>

        <button
          type="button"
          disabled={importing}
          onClick={() => fileInputRef.current?.click()}
          className="inline-flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/[0.03] px-3 py-1.5 text-xs font-medium text-slate-200 transition-colors hover:bg-white/[0.06] disabled:opacity-50"
        >
          {importing ? (
            <Loader2 className="h-3.5 w-3.5 animate-spin" />
          ) : (
            <Upload className="h-3.5 w-3.5" />
          )}
          {importing ? "Importing…" : "Import"}
        </button>
        <input
          ref={fileInputRef}
          type="file"
          accept=".json"
          className="hidden"
          onChange={(e) => {
            const f = e.target.files?.[0]
            if (f) runImport(f)
          }}
        />

        <button
          type="button"
          disabled={processing || importing || deleting}
          onClick={() => setConfirmingDelete(true)}
          className="inline-flex items-center gap-1.5 rounded-lg border border-rose-400/30 bg-rose-500/10 px-3 py-1.5 text-xs font-medium text-rose-200 transition-colors hover:bg-rose-500/20 disabled:opacity-50"
        >
          <Trash2 className="h-3.5 w-3.5" /> Delete all
        </button>
      </div>

      {error && (
        <p className="mt-2 text-xs text-rose-300">
          {error}
        </p>
      )}

      {confirmingDelete && (
        <div className="fixed inset-0 z-[2000] flex items-center justify-center bg-black/60 backdrop-blur-sm">
          <div className="w-full max-w-sm rounded-2xl border border-white/10 bg-slate-950 p-6 shadow-2xl">
            <h3 className="text-base font-semibold text-slate-100">Delete all drives?</h3>
            <p className="mt-2 text-xs leading-relaxed text-slate-400">
              This permanently removes every route, processed file record, and drive tag from the database. The action cannot be undone.
            </p>
            <p className="mt-2 text-[11px] text-slate-500">
              Tip: export your data first if you want a backup.
            </p>
            <div className="mt-5 flex items-center justify-end gap-2">
              <button
                type="button"
                disabled={deleting}
                onClick={() => setConfirmingDelete(false)}
                className="rounded-lg border border-white/10 bg-white/[0.03] px-4 py-1.5 text-xs font-medium text-slate-300 hover:bg-white/[0.06] disabled:opacity-50"
              >
                Cancel
              </button>
              <button
                type="button"
                disabled={deleting}
                onClick={runDelete}
                className={cn(
                  "inline-flex items-center gap-1.5 rounded-lg px-4 py-1.5 text-xs font-medium text-white transition-colors disabled:opacity-50",
                  "bg-rose-600 hover:bg-rose-500",
                )}
              >
                {deleting ? (
                  <Loader2 className="h-3.5 w-3.5 animate-spin" />
                ) : (
                  <Trash2 className="h-3.5 w-3.5" />
                )}
                {deleting ? "Deleting…" : "Delete everything"}
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  )
}

interface MenuItemProps {
  icon: React.ReactNode
  title: string
  hint: string
  onClick: () => void
}

function MenuItem({ icon, title, hint, onClick }: MenuItemProps) {
  return (
    <button
      type="button"
      onClick={onClick}
      className="flex w-full items-start gap-2 px-3 py-2 text-left text-xs text-slate-300 transition-colors hover:bg-white/[0.04]"
    >
      <span className="mt-0.5">{icon}</span>
      <span>
        <span className="block font-medium">{title}</span>
        <span className="block text-[10px] text-slate-500">{hint}</span>
      </span>
    </button>
  )
}
