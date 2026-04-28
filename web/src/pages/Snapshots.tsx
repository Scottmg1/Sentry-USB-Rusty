import { useCallback, useEffect, useMemo, useState } from "react"
import { Trash2, Loader2, AlertTriangle, Clock, HardDrive, ArrowUpDown } from "lucide-react"

interface SnapshotEntry {
  id: string
  size_bytes: number
  created_unix: number
}

interface FreeSpace {
  total_bytes: number
  used_bytes: number
  available_bytes: number
  mounted: boolean
}

type SortMode = "oldest" | "newest" | "largest"

function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B"
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`
  return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`
}

function formatDate(unix: number): string {
  if (!unix) return "—"
  const d = new Date(unix * 1000)
  return d.toLocaleString()
}

function relativeTime(unix: number): string {
  if (!unix) return ""
  const diff = Date.now() / 1000 - unix
  if (diff < 60) return "just now"
  if (diff < 3600) return `${Math.floor(diff / 60)}m ago`
  if (diff < 86400) return `${Math.floor(diff / 3600)}h ago`
  if (diff < 86400 * 30) return `${Math.floor(diff / 86400)}d ago`
  return `${Math.floor(diff / 86400 / 30)}mo ago`
}

export default function Snapshots() {
  const [snapshots, setSnapshots] = useState<SnapshotEntry[]>([])
  const [free, setFree] = useState<FreeSpace | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<Set<string>>(new Set())
  const [confirmId, setConfirmId] = useState<string | null>(null)
  // Default oldest-first per the user's explicit request: that's what
  // they need to delete to free space, so the first row is the most
  // useful action by default. Allow re-sorting for browsing.
  const [sortMode, setSortMode] = useState<SortMode>("oldest")

  const refresh = useCallback(async () => {
    setError(null)
    try {
      const [listRes, spaceRes] = await Promise.all([
        fetch("/api/snapshots"),
        fetch("/api/backingfiles/free-space"),
      ])
      if (!listRes.ok) throw new Error("Failed to load snapshots")
      const listData = await listRes.json()
      setSnapshots(Array.isArray(listData?.snapshots) ? listData.snapshots : [])
      if (spaceRes.ok) {
        const spaceData = await spaceRes.json()
        setFree(spaceData)
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : "Unknown error")
    } finally {
      setLoading(false)
    }
  }, [])

  useEffect(() => {
    refresh()
  }, [refresh])

  const sortedSnapshots = useMemo(() => {
    const arr = [...snapshots]
    if (sortMode === "oldest") arr.sort((a, b) => a.created_unix - b.created_unix)
    else if (sortMode === "newest") arr.sort((a, b) => b.created_unix - a.created_unix)
    else if (sortMode === "largest") arr.sort((a, b) => b.size_bytes - a.size_bytes)
    return arr
  }, [snapshots, sortMode])

  const totalSnapshotBytes = useMemo(
    () => snapshots.reduce((sum, s) => sum + (s.size_bytes || 0), 0),
    [snapshots],
  )

  async function handleDelete(id: string) {
    setDeleting((prev) => new Set(prev).add(id))
    setError(null)
    try {
      const res = await fetch(`/api/snapshots/${encodeURIComponent(id)}`, {
        method: "DELETE",
      })
      if (!res.ok) {
        const body = await res.json().catch(() => ({}))
        throw new Error(body?.error || "Delete failed")
      }
      // Optimistic UI update + background refresh of the free-space
      // gauge so the user sees the GB freed land immediately.
      setSnapshots((prev) => prev.filter((s) => s.id !== id))
      setConfirmId(null)
      void refresh()
    } catch (err) {
      setError(err instanceof Error ? err.message : "Delete failed")
    } finally {
      setDeleting((prev) => {
        const next = new Set(prev)
        next.delete(id)
        return next
      })
    }
  }

  const usedPct =
    free && free.total_bytes > 0
      ? Math.min(100, Math.round((free.used_bytes / free.total_bytes) * 100))
      : 0

  return (
    <div className="mx-auto max-w-4xl px-4 py-6">
      <div className="mb-6">
        <h1 className="text-2xl font-semibold text-slate-100">Snapshots</h1>
        <p className="mt-1 text-sm text-slate-400">
          Snapshots are point-in-time copies of your dashcam footage stored on
          the backingfiles partition. Delete oldest snapshots here to free
          space — for example, before growing the dashcam drive size.
        </p>
      </div>

      {/* Free-space gauge */}
      {free && free.mounted && (
        <div className="glass-card mb-6 p-4">
          <div className="mb-2 flex items-center justify-between text-sm">
            <span className="flex items-center gap-2 text-slate-300">
              <HardDrive className="h-4 w-4" />
              Backingfiles partition
            </span>
            <span className="text-slate-400">
              {formatBytes(free.used_bytes)} used of {formatBytes(free.total_bytes)}
              {" · "}
              {formatBytes(free.available_bytes)} free
            </span>
          </div>
          <div className="h-2 overflow-hidden rounded-full bg-white/5">
            <div
              className={`h-full transition-all ${
                usedPct > 90
                  ? "bg-red-500"
                  : usedPct > 75
                    ? "bg-amber-500"
                    : "bg-emerald-500"
              }`}
              style={{ width: `${usedPct}%` }}
            />
          </div>
          {snapshots.length > 0 && (
            <p className="mt-2 text-xs text-slate-500">
              Snapshots account for {formatBytes(totalSnapshotBytes)} of the used
              space ({snapshots.length} {snapshots.length === 1 ? "snapshot" : "snapshots"}).
            </p>
          )}
        </div>
      )}

      {error && (
        <div className="mb-4 flex items-start gap-2 rounded-lg border border-red-500/30 bg-red-500/10 p-3 text-sm">
          <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-red-400" />
          <p className="text-red-300">{error}</p>
        </div>
      )}

      {/* Sort controls */}
      {snapshots.length > 0 && (
        <div className="mb-3 flex items-center justify-between">
          <p className="text-xs text-slate-500">
            {snapshots.length} {snapshots.length === 1 ? "snapshot" : "snapshots"}
          </p>
          <label className="flex items-center gap-2 text-xs text-slate-400">
            <ArrowUpDown className="h-3 w-3" />
            <select
              value={sortMode}
              onChange={(e) => setSortMode(e.target.value as SortMode)}
              className="rounded border border-white/10 bg-white/5 px-2 py-1 text-xs text-slate-200 focus:border-blue-400 focus:outline-none"
            >
              <option value="oldest">Oldest first</option>
              <option value="newest">Newest first</option>
              <option value="largest">Largest first</option>
            </select>
          </label>
        </div>
      )}

      {/* Snapshot list */}
      {loading ? (
        <div className="flex justify-center py-12">
          <Loader2 className="h-6 w-6 animate-spin text-slate-500" />
        </div>
      ) : snapshots.length === 0 ? (
        <div className="glass-card flex flex-col items-center gap-2 px-4 py-12 text-center">
          <Clock className="h-8 w-8 text-slate-600" />
          <p className="text-sm text-slate-400">No snapshots on this device.</p>
          <p className="text-xs text-slate-500">
            Snapshots are created automatically by the archive loop while you drive.
          </p>
        </div>
      ) : (
        <ul className="flex flex-col gap-2">
          {sortedSnapshots.map((s) => {
            const isDeleting = deleting.has(s.id)
            const isConfirming = confirmId === s.id
            return (
              <li
                key={s.id}
                className="glass-card flex items-center justify-between gap-3 px-4 py-3"
              >
                <div className="min-w-0 flex-1">
                  <p className="truncate text-sm font-medium text-slate-200">{s.id}</p>
                  <p className="text-xs text-slate-500">
                    {formatDate(s.created_unix)}
                    <span className="mx-1.5 text-slate-700">•</span>
                    {relativeTime(s.created_unix)}
                    <span className="mx-1.5 text-slate-700">•</span>
                    {formatBytes(s.size_bytes)}
                  </p>
                </div>
                {isConfirming ? (
                  <div className="flex shrink-0 items-center gap-2">
                    <button
                      onClick={() => setConfirmId(null)}
                      disabled={isDeleting}
                      className="rounded border border-white/10 bg-white/5 px-3 py-1 text-xs text-slate-300 transition-colors hover:bg-white/10 disabled:opacity-50"
                    >
                      Cancel
                    </button>
                    <button
                      onClick={() => handleDelete(s.id)}
                      disabled={isDeleting}
                      className="flex items-center gap-1 rounded bg-red-500/80 px-3 py-1 text-xs font-medium text-white transition-colors hover:bg-red-500 disabled:opacity-50"
                    >
                      {isDeleting && <Loader2 className="h-3 w-3 animate-spin" />}
                      Delete forever
                    </button>
                  </div>
                ) : (
                  <button
                    onClick={() => setConfirmId(s.id)}
                    disabled={isDeleting}
                    className="flex shrink-0 items-center gap-1 rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-xs text-slate-400 transition-colors hover:border-red-500/30 hover:bg-red-500/10 hover:text-red-300 disabled:opacity-50"
                    aria-label={`Delete snapshot ${s.id}`}
                  >
                    <Trash2 className="h-3.5 w-3.5" />
                    Delete
                  </button>
                )}
              </li>
            )
          })}
        </ul>
      )}

      {/* Footnote */}
      {snapshots.length > 0 && (
        <p className="mt-6 text-xs text-slate-500">
          Snapshots are archived footage. Deletion is permanent — clips covered
          by a deleted snapshot cannot be recovered. Live clips on the dashcam
          drive (/mnt/cam) are unaffected.
        </p>
      )}
    </div>
  )
}
