import { useState, useEffect, useRef, useCallback, useMemo } from "react"
import {
  ArrowBackIcon,
  BrushIcon,
  CheckCircleIcon,
  CheckIcon,
  CloseIcon,
  CreateNewFolderIcon,
  DeleteIcon,
  DownloadIcon,
  DraftIcon,
  FolderIcon,
  FolderOpenIcon,
  HardDriveIcon,
  MusicNoteIcon,
  ProgressActivityIcon,
  RectangleIcon,
  SearchIcon,
  SwapVertIcon,
  UploadIcon,
  VideocamIcon,
  VolumeUpIcon,
} from "@/components/icons"
import { cn } from "@/lib/utils"

type SortOption = "name-asc" | "name-desc" | "date-newest" | "date-oldest" | "size-largest" | "size-smallest" | "type"

const SORT_LABELS: Record<SortOption, string> = {
  "name-asc": "Name (A-Z)",
  "name-desc": "Name (Z-A)",
  "date-newest": "Date (Newest)",
  "date-oldest": "Date (Oldest)",
  "size-largest": "Size (Largest)",
  "size-smallest": "Size (Smallest)",
  "type": "Type",
}

interface FileEntry {
  name: string
  path: string
  is_dir: boolean
  size: number
  // Matches the backend's serialized field name (files.rs FileEntry).
  // This was previously declared as `modified`, which doesn't exist in
  // the response — the Date column rendered blank for every file and
  // the two date sorts quietly degraded to name order (NaN timestamps
  // made the name tiebreaker always win).
  mod_time: string
}

interface DriveTab {
  id: string
  base: string
  icon: "cam" | "media" | "wrap" | "plate" | "lock" | "drive"
}

const ALL_DRIVES: DriveTab[] = [
  { id: "USB Drive", base: "/mutable", icon: "drive" },
  { id: "TeslaCam", base: "/mutable/TeslaCam", icon: "cam" },
  { id: "Lock Sounds", base: "/mutable/LockChime", icon: "lock" },
  { id: "Wraps", base: "/mutable/Wraps", icon: "wrap" },
  { id: "License Plates", base: "/mutable/LicensePlate", icon: "plate" },
  { id: "Music", base: "/var/www/html/fs/Music", icon: "media" },
  { id: "LightShow", base: "/var/www/html/fs/LightShow", icon: "media" },
  { id: "Boombox", base: "/var/www/html/fs/Boombox", icon: "media" },
]

const TAB_ICONS: Record<DriveTab["icon"], React.ComponentType<{ className?: string }>> = {
  cam: VideocamIcon,
  media: MusicNoteIcon,
  wrap: BrushIcon,
  plate: RectangleIcon,
  lock: VolumeUpIcon,
  drive: HardDriveIcon,
}

function formatSize(bytes: number): string {
  if (bytes === 0) return "—"
  const units = ["B", "KB", "MB", "GB"]
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  return `${(bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0)} ${units[i]}`
}

interface UploadProgress {
  fileName: string
  loaded: number
  total: number
  done: boolean
  error: boolean
}

export default function Files() {
  const [drives, setDrives] = useState<DriveTab[]>([])
  const [activeDrive, setActiveDrive] = useState<DriveTab | null>(null)
  const [currentPath, setCurrentPath] = useState("")
  const [files, setFiles] = useState<FileEntry[]>([])
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [selected, setSelected] = useState<Set<string>>(new Set())
  const uploadRef = useRef<HTMLInputElement>(null)
  const folderUploadRef = useRef<HTMLInputElement>(null)
  const [uploads, setUploads] = useState<UploadProgress[]>([])
  const [uploading, setUploading] = useState(false)
  const [dragging, setDragging] = useState(false)
  const dragCounter = useRef(0)
  const [effectiveBase, setEffectiveBase] = useState("")
  const [search, setSearch] = useState("")
  const [sortOption, setSortOption] = useState<SortOption>("name-asc")
  const [showSortMenu, setShowSortMenu] = useState(false)
  const searchTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)
  const sortMenuRef = useRef<HTMLDivElement>(null)

  // Fetch config to determine which tabs to show
  useEffect(() => {
    async function loadConfig() {
      try {
        const res = await fetch("/api/config")
        const cfg = await res.json()
        const visible: DriveTab[] = []
        // Always show USB Drive root (shows LockChime.wav, TeslaCam, etc.)
        visible.push(ALL_DRIVES.find(d => d.id === "USB Drive")!)
        // Show TeslaCam tab if cam is configured
        if (cfg.has_cam === "yes") {
          visible.push(ALL_DRIVES.find(d => d.id === "TeslaCam")!)
        }
        // Always show Lock Sounds, Wraps and License Plates (they're user-uploadable)
        visible.push(ALL_DRIVES.find(d => d.id === "Lock Sounds")!)
        visible.push(ALL_DRIVES.find(d => d.id === "Wraps")!)
        visible.push(ALL_DRIVES.find(d => d.id === "License Plates")!)
        if (cfg.has_music === "yes") visible.push(ALL_DRIVES.find(d => d.id === "Music")!)
        if (cfg.has_lightshow === "yes") visible.push(ALL_DRIVES.find(d => d.id === "LightShow")!)
        if (cfg.has_boombox === "yes") visible.push(ALL_DRIVES.find(d => d.id === "Boombox")!)
        // If nothing is configured (e.g. dev mode), show all
        const result = visible.length > 0 ? visible : ALL_DRIVES
        setDrives(result)
        setActiveDrive(result[0])
        setCurrentPath(result[0].base)
      } catch {
        // Fallback: show all
        setDrives(ALL_DRIVES)
        setActiveDrive(ALL_DRIVES[0])
        setCurrentPath(ALL_DRIVES[0].base)
      }
    }
    loadConfig()
  }, [])

  async function fetchFiles(path: string, searchQuery?: string) {
    setLoading(true)
    setError(null)
    setSelected(new Set())
    try {
      let url = `/api/files/ls?path=${encodeURIComponent(path)}`
      if (searchQuery) url += `&search=${encodeURIComponent(searchQuery)}`
      const res = await fetch(url)
      if (!res.ok) {
        const data = await res.json().catch(() => ({ error: "Failed to load" }))
        setError(data.error || "Failed to load directory")
        setFiles([])
      } else {
        const raw = await res.json()
        // Server returns { path, entries: [...] } wrapper
        const data: FileEntry[] = Array.isArray(raw) ? raw : (raw.entries ?? [])
        // Auto-navigate into the matching subfolder when at a drive's base path
        // (Music/LightShow/Boombox disk images have a root folder matching the
        // drive name, possibly alongside hidden macOS/Tesla metadata folders)
        if (activeDrive && path === activeDrive.base && !searchQuery) {
          const match = data.find(
            (e) => e.is_dir && e.name === activeDrive.id
          )
          if (match) {
            setEffectiveBase(match.path)
            setCurrentPath(match.path)
            return
          }
        }
        setFiles(data)
      }
    } catch {
      setError("Unable to connect")
      setFiles([])
    }
    setLoading(false)
  }

  useEffect(() => {
    if (currentPath) fetchFiles(currentPath, search || undefined)
    // search intentionally omitted: search changes fetch via the debounced
    // handler below; including it here would double-fetch on every keystroke.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [currentPath])

  // Debounced search
  function handleSearchChange(value: string) {
    setSearch(value)
    if (searchTimerRef.current) clearTimeout(searchTimerRef.current)
    searchTimerRef.current = setTimeout(() => {
      if (currentPath) fetchFiles(currentPath, value || undefined)
    }, 300)
  }

  // Close sort menu on outside click
  useEffect(() => {
    function handleClick(e: MouseEvent) {
      if (sortMenuRef.current && !sortMenuRef.current.contains(e.target as Node)) {
        setShowSortMenu(false)
      }
    }
    if (showSortMenu) document.addEventListener("mousedown", handleClick)
    return () => document.removeEventListener("mousedown", handleClick)
  }, [showSortMenu])

  // Client-side sorting (directories always first, name tiebreaker for stability)
  const sortedFiles = useMemo(() => {
    const sorted = [...files]
    sorted.sort((a, b) => {
      // Directories always come first
      if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1
      const nameCmp = a.name.localeCompare(b.name, undefined, { sensitivity: "base" })
      switch (sortOption) {
        case "name-asc":
          return nameCmp
        case "name-desc":
          return -nameCmp
        case "date-newest":
          return (new Date(b.mod_time).getTime() - new Date(a.mod_time).getTime()) || -nameCmp
        case "date-oldest":
          return (new Date(a.mod_time).getTime() - new Date(b.mod_time).getTime()) || nameCmp
        case "size-largest":
          return b.size - a.size
        case "size-smallest":
          return a.size - b.size
        case "type": {
          const extA = a.name.includes(".") ? a.name.split(".").pop()!.toLowerCase() : ""
          const extB = b.name.includes(".") ? b.name.split(".").pop()!.toLowerCase() : ""
          return extA.localeCompare(extB) || a.name.localeCompare(b.name, undefined, { sensitivity: "base" })
        }
        default:
          return 0
      }
    })
    return sorted
  }, [files, sortOption])

  function navigate(entry: FileEntry) {
    if (entry.is_dir) {
      setCurrentPath(entry.path)
    }
  }

  function goUp() {
    const base = effectiveBase || activeDrive?.base
    if (!activeDrive || !base || currentPath === base) return
    const parent = currentPath.split("/").slice(0, -1).join("/")
    if (parent.length < base.length) return
    setCurrentPath(parent || base)
  }

  function switchDrive(drive: DriveTab) {
    setActiveDrive(drive)
    setEffectiveBase("")
    setSearch("")
    setCurrentPath(drive.base)
  }

  async function handleDelete() {
    if (selected.size === 0) return
    if (!confirm(`Delete ${selected.size} item(s)?`)) return
    for (const path of selected) {
      await fetch(`/api/files?path=${encodeURIComponent(path)}`, { method: "DELETE" })
    }
    fetchFiles(currentPath)
  }

  function uploadFileWithProgress(file: globalThis.File, destPath: string, index: number): Promise<void> {
    return new Promise((resolve) => {
      const form = new FormData()
      form.append("file", file)
      form.append("path", destPath)

      const xhr = new XMLHttpRequest()
      xhr.open("POST", "/api/files/upload")

      xhr.upload.onprogress = (e) => {
        if (e.lengthComputable) {
          setUploads((prev) => prev.map((u, i) => i === index ? { ...u, loaded: e.loaded, total: e.total } : u))
        }
      }

      xhr.onload = () => {
        setUploads((prev) => prev.map((u, i) => i === index ? { ...u, done: true, loaded: u.total, error: xhr.status >= 400 } : u))
        resolve()
      }

      xhr.onerror = () => {
        setUploads((prev) => prev.map((u, i) => i === index ? { ...u, done: true, error: true } : u))
        resolve()
      }

      xhr.send(form)
    })
  }

  const processFiles = useCallback(async (fileArr: globalThis.File[]) => {
    if (fileArr.length === 0) return
    const initial: UploadProgress[] = fileArr.map((f) => ({
      fileName: f.name,
      loaded: 0,
      total: f.size,
      done: false,
      error: false,
    }))

    setUploads(initial)
    setUploading(true)

    // Upload files sequentially to avoid overwhelming low-RAM devices
    for (let i = 0; i < fileArr.length; i++) {
      await uploadFileWithProgress(fileArr[i], currentPath, i)
    }

    fetchFiles(currentPath)
    if (uploadRef.current) uploadRef.current.value = ""
    if (folderUploadRef.current) folderUploadRef.current.value = ""

    setTimeout(() => {
      setUploads([])
      setUploading(false)
    }, 2000)
    // fetchFiles is a plain function (new identity each render); listing it
    // would defeat the memoization. It only closes over currentPath, which
    // is a dep.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [currentPath])

  const handleUpload = useCallback(async (e: React.ChangeEvent<HTMLInputElement>) => {
    const fileList = e.target.files
    if (!fileList || fileList.length === 0) return
    await processFiles(Array.from(fileList))
  }, [processFiles])

  const handleDragEnter = useCallback((e: React.DragEvent) => {
    e.preventDefault()
    e.stopPropagation()
    dragCounter.current++
    if (e.dataTransfer.items?.length) setDragging(true)
  }, [])

  const handleDragLeave = useCallback((e: React.DragEvent) => {
    e.preventDefault()
    e.stopPropagation()
    dragCounter.current--
    if (dragCounter.current === 0) setDragging(false)
  }, [])

  const handleDragOver = useCallback((e: React.DragEvent) => {
    e.preventDefault()
    e.stopPropagation()
  }, [])

  const handleDrop = useCallback(async (e: React.DragEvent) => {
    e.preventDefault()
    e.stopPropagation()
    setDragging(false)
    dragCounter.current = 0
    if (uploading) return
    const files = Array.from(e.dataTransfer.files)
    if (files.length > 0) await processFiles(files)
  }, [uploading, processFiles])

  function handleDownloadSelected() {
    if (selected.size === 0) return
    const form = document.createElement("form")
    form.method = "POST"
    form.action = "/api/files/download-zip-multi"
    form.style.display = "none"
    const input = document.createElement("input")
    input.type = "hidden"
    input.name = "paths"
    input.value = JSON.stringify(Array.from(selected))
    form.appendChild(input)
    document.body.appendChild(form)
    form.submit()
    form.remove()
  }

  async function handleNewFolder() {
    const name = prompt("Folder name:")
    if (!name) return
    await fetch("/api/files/mkdir", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ path: `${currentPath}/${name}` }),
    })
    fetchFiles(currentPath)
  }

  if (!activeDrive) {
    return (
      <div className="flex items-center justify-center p-8">
        <ProgressActivityIcon className="h-5 w-5 animate-spin text-slate-500" />
      </div>
    )
  }

  const base = effectiveBase || activeDrive.base
  const relativePath = currentPath.replace(base, "") || "/"

  return (
    <div className="flex h-[calc(100vh-120px)] flex-col space-y-4 md:h-[calc(100vh-96px)]">
      <div className="flex items-center justify-between">
        <div>
          <h1 className="text-2xl font-bold text-slate-100">Files</h1>
          <p className="mt-1 text-sm text-slate-500">
            Manage dashcam clips and media files
          </p>
        </div>
        <div className="flex flex-wrap gap-2">
          <button
            onClick={handleNewFolder}
            className="glass-card glass-card-hover flex items-center gap-1.5 px-3 py-1.5 text-sm text-slate-400 transition-colors hover:text-slate-200"
          >
            <CreateNewFolderIcon className="h-4 w-4" />
            New Folder
          </button>
          <button
            onClick={() => uploadRef.current?.click()}
            disabled={uploading}
            className={cn(
              "glass-card glass-card-hover flex items-center gap-1.5 px-3 py-1.5 text-sm transition-colors",
              uploading ? "text-slate-600 cursor-not-allowed" : "text-slate-400 hover:text-slate-200"
            )}
          >
            {uploading ? <ProgressActivityIcon className="h-4 w-4 animate-spin" /> : <UploadIcon className="h-4 w-4" />}
            {uploading ? "Uploading..." : "Upload"}
          </button>
          <input ref={uploadRef} type="file" multiple className="hidden" onChange={handleUpload} />
          <button
            onClick={() => folderUploadRef.current?.click()}
            disabled={uploading}
            className={cn(
              "glass-card glass-card-hover flex items-center gap-1.5 px-3 py-1.5 text-sm transition-colors",
              uploading ? "text-slate-600 cursor-not-allowed" : "text-slate-400 hover:text-slate-200"
            )}
          >
            <FolderOpenIcon className="h-4 w-4" />
            Upload Folder
          </button>
          {/* @ts-expect-error webkitdirectory is non-standard but supported in all major browsers */}
          <input ref={folderUploadRef} type="file" multiple webkitdirectory="" className="hidden" onChange={handleUpload} />
        </div>
      </div>

      {/* Drive selector */}
      <div className="flex flex-wrap gap-1">
        {drives.map((drive) => (
          <button
            key={drive.id}
            onClick={() => switchDrive(drive)}
            className={cn(
              "flex items-center gap-1.5 rounded-lg px-3 py-1.5 text-sm font-medium transition-colors",
              activeDrive.id === drive.id
                ? "bg-blue-500/15 text-blue-400"
                : "text-slate-500 hover:bg-white/5 hover:text-slate-300"
            )}
          >
            {(() => { const Icon = TAB_ICONS[drive.icon]; return <Icon className="h-3.5 w-3.5" /> })()}
            {drive.id}
          </button>
        ))}
      </div>

      {/* Search and Sort */}
      <div className="flex items-center gap-2">
        <div className="relative flex-1">
          <SearchIcon className="pointer-events-none absolute left-2.5 top-1/2 h-3.5 w-3.5 -translate-y-1/2 text-slate-600" />
          <input
            type="text"
            value={search}
            onChange={(e) => handleSearchChange(e.target.value)}
            placeholder="Search files..."
            className="w-full rounded-lg border border-white/10 bg-white/5 py-1.5 pl-8 pr-8 text-sm text-slate-300 placeholder-slate-600 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"
          />
          {search && (
            <button
              onClick={() => handleSearchChange("")}
              className="absolute right-2 top-1/2 -translate-y-1/2 rounded p-0.5 text-slate-600 hover:text-slate-400"
            >
              <CloseIcon className="h-3.5 w-3.5" />
            </button>
          )}
        </div>
        <div className="relative" ref={sortMenuRef}>
          <button
            onClick={() => setShowSortMenu(!showSortMenu)}
            className={cn(
              "glass-card glass-card-hover flex items-center gap-1.5 whitespace-nowrap px-3 py-1.5 text-sm transition-colors",
              showSortMenu ? "text-blue-400" : "text-slate-400 hover:text-slate-200"
            )}
          >
            <SwapVertIcon className="h-3.5 w-3.5" />
            <span className="hidden sm:inline">{SORT_LABELS[sortOption]}</span>
          </button>
          {showSortMenu && (
            <div className="absolute right-0 top-full z-20 mt-1 w-44 overflow-hidden rounded-lg border border-white/10 bg-slate-900 shadow-xl">
              {(Object.keys(SORT_LABELS) as SortOption[]).map((opt) => (
                <button
                  key={opt}
                  onClick={() => { setSortOption(opt); setShowSortMenu(false) }}
                  className={cn(
                    "flex w-full items-center justify-between px-3 py-2 text-left text-sm transition-colors hover:bg-white/5",
                    sortOption === opt ? "text-blue-400" : "text-slate-400"
                  )}
                >
                  {SORT_LABELS[opt]}
                  {sortOption === opt && <CheckIcon className="h-3.5 w-3.5" />}
                </button>
              ))}
            </div>
          )}
        </div>
      </div>

      {/* Upload progress */}
      {uploads.length > 0 && (
        <div className="glass-card space-y-2 p-3">
          <div className="flex items-center justify-between">
            <p className="text-xs font-medium text-slate-300">
              {uploading ? "Uploading files..." : (
                <span className="flex items-center gap-1.5">
                  <CheckCircleIcon className="h-3.5 w-3.5 text-emerald-400" />
                  Upload complete
                </span>
              )}
            </p>
            {!uploading && (
              <button onClick={() => setUploads([])} className="rounded p-0.5 text-slate-600 hover:text-slate-400">
                <CloseIcon className="h-3.5 w-3.5" />
              </button>
            )}
          </div>
          {uploads.map((u, i) => {
            const pct = u.total > 0 ? Math.round((u.loaded / u.total) * 100) : 0
            return (
              <div key={i} className="space-y-1">
                <div className="flex items-center justify-between text-[11px]">
                  <span className="truncate text-slate-400">{u.fileName}</span>
                  <span className={cn("tabular-nums", u.error ? "text-red-400" : u.done ? "text-emerald-400" : "text-slate-500")}>
                    {u.error ? "Error" : u.done ? "Done" : `${pct}%`}
                  </span>
                </div>
                <div className="h-1 overflow-hidden rounded-full bg-slate-800">
                  <div
                    className={cn(
                      "h-full rounded-full transition-all duration-300",
                      u.error ? "bg-red-500" : u.done ? "bg-emerald-500" : "bg-blue-500"
                    )}
                    style={{ width: `${pct}%` }}
                  />
                </div>
              </div>
            )
          })}
          {uploading && uploads.length > 1 && (() => {
            const totalLoaded = uploads.reduce((s, u) => s + u.loaded, 0)
            const totalSize = uploads.reduce((s, u) => s + u.total, 0)
            const totalPct = totalSize > 0 ? Math.round((totalLoaded / totalSize) * 100) : 0
            const doneCount = uploads.filter((u) => u.done).length
            return (
              <div className="border-t border-white/5 pt-2">
                <div className="flex items-center justify-between text-[11px]">
                  <span className="text-slate-500">{doneCount}/{uploads.length} files</span>
                  <span className="tabular-nums text-slate-400">{formatSize(totalLoaded)} / {formatSize(totalSize)} ({totalPct}%)</span>
                </div>
                <div className="mt-1 h-1.5 overflow-hidden rounded-full bg-slate-800">
                  <div className="h-full rounded-full bg-blue-500 transition-all duration-300" style={{ width: `${totalPct}%` }} />
                </div>
              </div>
            )
          })()}
        </div>
      )}

      {/* File list */}
      <div
        className={cn("glass-card flex min-h-0 flex-1 flex-col overflow-hidden relative", dragging && "ring-2 ring-blue-500/50")}
        onDragEnter={handleDragEnter}
        onDragLeave={handleDragLeave}
        onDragOver={handleDragOver}
        onDrop={handleDrop}
      >
        {dragging && (
          <div className="absolute inset-0 z-30 flex items-center justify-center bg-slate-900/80 backdrop-blur-sm">
            <div className="flex flex-col items-center gap-2 text-blue-400">
              <UploadIcon className="h-10 w-10" />
              <p className="text-sm font-medium">Drop files here to upload</p>
            </div>
          </div>
        )}
        <div className="flex items-center justify-between border-b border-white/5 px-3 py-2">
          <div className="flex items-center gap-2">
            {currentPath !== base && (
              <button
                onClick={goUp}
                className="rounded p-1 text-slate-500 hover:bg-white/5 hover:text-slate-300"
              >
                <ArrowBackIcon className="h-4 w-4" />
              </button>
            )}
            <p className="font-mono text-sm text-slate-400">{relativePath}</p>
          </div>
          {selected.size > 0 && (
            <span className="rounded-full bg-blue-500/20 px-2 py-0.5 text-[10px] font-semibold text-blue-400">
              {selected.size} selected
            </span>
          )}
        </div>

        <div className="flex-1 overflow-y-auto">
          {loading ? (
            <div className="flex items-center justify-center p-8">
              <ProgressActivityIcon className="h-5 w-5 animate-spin text-slate-500" />
            </div>
          ) : error ? (
            <div className="flex flex-col items-center justify-center p-8">
              <FolderOpenIcon className="mb-2 h-10 w-10 text-slate-500" />
              <p className="text-sm text-slate-500">{error}</p>
            </div>
          ) : sortedFiles.length === 0 ? (
            <div className="flex flex-col items-center justify-center p-8">
              {(() => { const Icon = TAB_ICONS[activeDrive.icon]; return <Icon className="mb-2 h-10 w-10 text-slate-500" /> })()}
              <p className="text-sm text-slate-500">{search ? "No matching files" : "Empty folder"}</p>
              <p className="mt-1 text-xs text-slate-600">
                {search ? "Try a different search term" : activeDrive.icon === "cam" ? "No clips in this folder" : "Upload files to get started"}
              </p>
            </div>
          ) : (
            <table className="w-full text-sm">
              <tbody>
                {sortedFiles.map((f) => (
                  <tr
                    key={f.path}
                    className={cn(
                      "cursor-pointer border-b border-white/5 transition-colors hover:bg-white/5",
                      selected.has(f.path) && "bg-blue-500/10"
                    )}
                    onClick={() => {
                      if (f.is_dir) {
                        navigate(f)
                      } else {
                        setSelected((prev) => {
                          const next = new Set(prev)
                          if (next.has(f.path)) next.delete(f.path)
                          else next.add(f.path)
                          return next
                        })
                      }
                    }}
                  >
                    <td className="w-8 px-2 py-3">
                      <input
                        type="checkbox"
                        checked={selected.has(f.path)}
                        onChange={() => {
                          setSelected((prev) => {
                            const next = new Set(prev)
                            if (next.has(f.path)) next.delete(f.path)
                            else next.add(f.path)
                            return next
                          })
                        }}
                        onClick={(e) => e.stopPropagation()}
                        className="h-3.5 w-3.5 cursor-pointer rounded border-slate-600 accent-blue-500"
                      />
                    </td>
                    <td className="px-1 py-3">
                      {f.is_dir ? (
                        <FolderIcon className="h-4 w-4 text-blue-400" />
                      ) : (
                        <DraftIcon className="h-4 w-4 text-slate-500" />
                      )}
                    </td>
                    <td className="min-w-0 truncate py-3 text-slate-300">{f.name}</td>
                    <td className="hidden px-3 py-3 text-right text-xs text-slate-600 sm:table-cell">
                      {f.mod_time ? new Date(f.mod_time).toLocaleDateString() : ""}
                    </td>
                    <td className="px-3 py-3 text-right text-xs text-slate-600">
                      {f.is_dir ? "" : formatSize(f.size)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </div>

      {/* Floating selection action bar */}
      {selected.size > 0 && (
        <div className="fixed bottom-6 left-1/2 z-50 -translate-x-1/2 md:left-[calc(50%+7rem)]">
          <div className="glass-card flex items-center gap-3 border border-blue-500/20 bg-slate-900/95 px-4 py-3 shadow-2xl backdrop-blur-xl animate-in slide-in-from-bottom-2 fade-in duration-200">
            <span className="rounded-full bg-blue-500/20 px-2.5 py-1 text-xs font-semibold text-blue-400">
              {selected.size} selected
            </span>
            <div className="h-4 w-px bg-white/10" />
            <button
              onClick={handleDownloadSelected}
              className="flex items-center gap-2 rounded-lg bg-blue-500/15 px-3 py-2 text-sm font-medium text-blue-400 transition-colors hover:bg-blue-500/25"
            >
              <DownloadIcon className="h-4 w-4" />
              Download
            </button>
            <button
              onClick={handleDelete}
              className="flex items-center gap-2 rounded-lg bg-red-500/10 px-3 py-2 text-sm font-medium text-red-400 transition-colors hover:bg-red-500/20"
            >
              <DeleteIcon className="h-4 w-4" />
              Delete
            </button>
            <button
              onClick={() => setSelected(new Set())}
              className="rounded-lg p-2 text-slate-500 transition-colors hover:bg-white/5 hover:text-slate-300"
              title="Clear selection"
            >
              <CloseIcon className="h-4 w-4" />
            </button>
          </div>
        </div>
      )}
    </div>
  )
}
