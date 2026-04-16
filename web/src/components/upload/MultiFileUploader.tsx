import { useState, useRef, useCallback, useEffect, useMemo } from "react"
import { Upload, X, CheckCircle, Loader2, AlertCircle, Plus, RotateCcw } from "lucide-react"

export interface FileEntry {
  id: string
  file: File
  name: string
  fields: Record<string, string>
  status: "pending" | "uploading" | "done" | "error"
  error?: string
}

export interface MultiFileUploaderProps {
  accept: string
  maxFiles: number
  rateLimitText: string
  accentColor: "blue" | "violet"
  validateFile: (file: File) => Promise<{ ok: boolean; error?: string }>
  renderPreview: (file: File) => React.ReactNode
  renderFields: (
    entry: FileEntry,
    onChange: (updates: Partial<Pick<FileEntry, "name" | "fields">>) => void
  ) => React.ReactNode
  isEntryReady: (entry: FileEntry) => boolean
  onUpload: (
    entry: FileEntry,
    onStep: (step: string) => void
  ) => Promise<{ success: boolean; message: string }>
}

export default function MultiFileUploader({
  accept,
  maxFiles,
  rateLimitText,
  accentColor,
  validateFile,
  renderPreview,
  renderFields,
  isEntryReady,
  onUpload,
}: MultiFileUploaderProps) {
  const [files, setFiles] = useState<FileEntry[]>([])
  const [expandedId, setExpandedId] = useState<string | null>(null)
  const [dragging, setDragging] = useState(false)
  const [uploadingAll, setUploadingAll] = useState(false)
  const [uploadProgress, setUploadProgress] = useState<{ current: number; total: number } | null>(null)
  const [currentStep, setCurrentStep] = useState<string | null>(null)
  const [errors, setErrors] = useState<string[]>([])
  const inputRef = useRef<HTMLInputElement>(null)
  const filesRef = useRef(files)
  filesRef.current = files

  const addFiles = useCallback(async (incoming: File[]) => {
    setErrors([])
    const currentCount = filesRef.current.length
    const available = maxFiles - currentCount
    if (available <= 0) {
      setErrors([`Maximum ${maxFiles} files allowed`])
      return
    }
    const toProcess = incoming.slice(0, available)
    if (incoming.length > available) {
      setErrors([`Only ${available} more file(s) can be added (limit: ${maxFiles})`])
    }

    const validated: FileEntry[] = []
    const validationErrors: string[] = []

    for (const f of toProcess) {
      const result = await validateFile(f)
      if (result.ok) {
        validated.push({
          id: Math.random().toString(36).slice(2) + Date.now().toString(36),
          file: f,
          name: f.name.replace(/\.[^/.]+$/, ""),
          fields: {},
          status: "pending",
        })
      } else {
        validationErrors.push(`${f.name}: ${result.error}`)
      }
    }

    if (validationErrors.length > 0) {
      setErrors((prev) => [...prev, ...validationErrors])
    }
    if (validated.length > 0) {
      setFiles((prev) => [...prev, ...validated])
      setExpandedId((prev) => prev ?? validated[0].id)
    }
  }, [maxFiles, validateFile])

  const handleDrop = useCallback((e: React.DragEvent) => {
    e.preventDefault()
    setDragging(false)
    const dropped = Array.from(e.dataTransfer.files)
    if (dropped.length > 0) addFiles(dropped)
  }, [addFiles])

  const handleInputChange = useCallback((e: React.ChangeEvent<HTMLInputElement>) => {
    const selected = Array.from(e.target.files || [])
    if (selected.length > 0) addFiles(selected)
    e.target.value = ""
  }, [addFiles])

  const removeFile = useCallback((id: string) => {
    setFiles((prev) => prev.filter((f) => f.id !== id))
    if (expandedId === id) {
      setExpandedId(null)
    }
  }, [expandedId])

  const updateEntry = useCallback((id: string, updates: Partial<Pick<FileEntry, "name" | "fields">>) => {
    setFiles((prev) =>
      prev.map((f) =>
        f.id === id
          ? { ...f, ...updates, fields: updates.fields ? { ...f.fields, ...updates.fields } : f.fields }
          : f
      )
    )
  }, [])

  const uploadSingle = useCallback(async (id: string) => {
    const entry = filesRef.current.find((f) => f.id === id)
    if (!entry || (entry.status !== "pending" && entry.status !== "error")) return

    setFiles((prev) => prev.map((f) => f.id === id ? { ...f, status: "uploading", error: undefined } : f))
    setExpandedId(id)
    setCurrentStep(null)

    // Re-read entry fresh in case name/fields were edited
    const freshEntry = filesRef.current.find((f) => f.id === id) ?? entry

    try {
      const result = await onUpload(freshEntry, (step) => setCurrentStep(step))
      setFiles((prev) =>
        prev.map((f) =>
          f.id === id
            ? { ...f, status: result.success ? "done" : "error", error: result.success ? undefined : result.message }
            : f
        )
      )
      if (result.success) {
        const nextPending = filesRef.current.find((f) => f.id !== id && f.status === "pending")
        setExpandedId(nextPending?.id ?? null)
      }
    } catch (err: any) {
      setFiles((prev) =>
        prev.map((f) => f.id === id ? { ...f, status: "error", error: err.message || "Upload failed" } : f)
      )
    } finally {
      setCurrentStep(null)
    }
  }, [onUpload])

  const uploadAll = useCallback(async () => {
    const ids = filesRef.current
      .filter((f) => f.status === "pending" && isEntryReady(f))
      .map((f) => f.id)
    if (ids.length === 0) return

    setUploadingAll(true)
    setUploadProgress({ current: 0, total: ids.length })

    for (let i = 0; i < ids.length; i++) {
      const entry = filesRef.current.find((f) => f.id === ids[i])
      if (!entry) continue

      setUploadProgress({ current: i + 1, total: ids.length })
      setFiles((prev) => prev.map((f) => f.id === entry.id ? { ...f, status: "uploading" } : f))
      setExpandedId(entry.id)
      setCurrentStep(null)

      try {
        const result = await onUpload(entry, (step) => setCurrentStep(step))
        setFiles((prev) =>
          prev.map((f) =>
            f.id === entry.id
              ? { ...f, status: result.success ? "done" : "error", error: result.success ? undefined : result.message }
              : f
          )
        )
      } catch (err: any) {
        setFiles((prev) =>
          prev.map((f) =>
            f.id === entry.id ? { ...f, status: "error", error: err.message || "Upload failed" } : f
          )
        )
      }

      setCurrentStep(null)
    }

    setUploadingAll(false)
    setUploadProgress(null)
    setExpandedId(null)
  }, [isEntryReady, onUpload])

  const clearAll = useCallback(() => {
    setFiles([])
    setExpandedId(null)
    setErrors([])
    setUploadProgress(null)
  }, [])

  const accent = accentColor === "blue"
    ? { border: "border-blue-500/60", borderLight: "border-blue-500/30", bg: "bg-blue-500/10", text: "text-blue-400" }
    : { border: "border-violet-500/60", borderLight: "border-violet-500/30", bg: "bg-violet-500/10", text: "text-violet-400" }

  const hasFiles = files.length > 0
  const allDone = files.length > 0 && files.every((f) => f.status === "done" || f.status === "error")
  const doneCount = files.filter((f) => f.status === "done").length
  const errorCount = files.filter((f) => f.status === "error").length
  const pendingFiles = files.filter((f) => f.status === "pending")
  const allReady = pendingFiles.length > 0 && pendingFiles.every((f) => isEntryReady(f))

  return (
    <div className="space-y-4">
      {/* Rate limit banner */}
      <div className="flex items-center gap-2 rounded-lg border border-white/10 bg-white/[0.02] px-3 py-2">
        <AlertCircle className="h-3.5 w-3.5 shrink-0 text-slate-500" />
        <p className="text-xs text-slate-500">{rateLimitText}</p>
      </div>

      {/* Validation errors */}
      {errors.length > 0 && (
        <div className="rounded-lg border border-red-500/20 bg-red-500/10 px-3 py-2 space-y-1">
          {errors.map((err, i) => (
            <p key={i} className="text-xs text-red-400">{err}</p>
          ))}
        </div>
      )}

      {/* Drop zone */}
      {!hasFiles ? (
        <div
          className={`relative rounded-xl border-2 border-dashed transition-colors cursor-pointer ${
            dragging
              ? `${accent.border} ${accent.bg}`
              : "border-white/10 hover:border-white/20 bg-white/[0.02]"
          }`}
          onDragOver={(e) => { e.preventDefault(); setDragging(true) }}
          onDragLeave={() => setDragging(false)}
          onDrop={handleDrop}
          onClick={() => inputRef.current?.click()}
        >
          <div className="flex flex-col items-center gap-3 py-10 px-4 text-center">
            <Upload className="h-8 w-8 text-slate-600" />
            <div>
              <p className="text-sm font-medium text-slate-300">Drop files or click to browse</p>
              <p className="mt-1 text-xs text-slate-500">Up to {maxFiles} files</p>
            </div>
          </div>
        </div>
      ) : !uploadingAll && !allDone ? (
        <div
          className={`flex items-center justify-center gap-2 rounded-lg border-2 border-dashed transition-colors cursor-pointer py-2.5 ${
            dragging
              ? `${accent.border} ${accent.bg}`
              : "border-white/10 hover:border-white/20 bg-white/[0.02]"
          }`}
          onDragOver={(e) => { e.preventDefault(); setDragging(true) }}
          onDragLeave={() => setDragging(false)}
          onDrop={handleDrop}
          onClick={() => files.length < maxFiles && inputRef.current?.click()}
        >
          <Plus className="h-4 w-4 text-slate-500" />
          <span className="text-xs text-slate-500">
            Add more ({files.length}/{maxFiles})
          </span>
        </div>
      ) : null}

      <input
        ref={inputRef}
        type="file"
        accept={accept}
        multiple
        className="hidden"
        onChange={handleInputChange}
      />

      {/* Thumbnail grid */}
      {hasFiles && (
        <div className="grid grid-cols-3 gap-2 sm:grid-cols-4">
          {files.map((entry) => {
            const isExpanded = expandedId === entry.id
            const isUploading = entry.status === "uploading"
            const isDone = entry.status === "done"
            const isError = entry.status === "error"

            return (
              <div
                key={entry.id}
                className={`group relative aspect-square overflow-hidden rounded-lg border-2 cursor-pointer transition-colors ${
                  isDone
                    ? "border-emerald-500/40"
                    : isError
                    ? "border-red-500/40"
                    : isExpanded
                    ? `${accent.border}`
                    : "border-white/10 hover:border-white/20"
                }`}
                onClick={() => !isUploading && setExpandedId(isExpanded ? null : entry.id)}
              >
                {/* Preview content */}
                <div className={`flex h-full w-full items-center justify-center bg-white/[0.02] ${
                  isUploading ? "opacity-50" : ""
                }`}>
                  {renderPreview(entry.file)}
                </div>

                {/* Uploading spinner overlay */}
                {isUploading && (
                  <div className="absolute inset-0 flex items-center justify-center bg-black/40">
                    <Loader2 className="h-6 w-6 animate-spin text-white" />
                  </div>
                )}

                {/* Done checkmark overlay */}
                {isDone && (
                  <div className="absolute inset-0 flex items-center justify-center bg-emerald-500/20">
                    <CheckCircle className="h-8 w-8 text-emerald-400" />
                  </div>
                )}

                {/* Error overlay */}
                {isError && (
                  <div className="absolute inset-0 flex items-center justify-center bg-red-500/10">
                    <AlertCircle className="h-6 w-6 text-red-400" />
                  </div>
                )}

                {/* Remove button (hover, hidden during upload or after done) */}
                {!isUploading && !isDone && !uploadingAll && (
                  <button
                    className="absolute top-1 right-1 flex h-5 w-5 items-center justify-center rounded-full bg-black/60 text-slate-400 opacity-0 transition-opacity group-hover:opacity-100 hover:text-white"
                    onClick={(e) => {
                      e.stopPropagation()
                      removeFile(entry.id)
                    }}
                  >
                    <X className="h-3 w-3" />
                  </button>
                )}

                {/* Filename label */}
                <div className="absolute bottom-0 left-0 right-0 truncate bg-gradient-to-t from-black/70 to-transparent px-1.5 pb-1 pt-4">
                  <p className="truncate text-[10px] text-white/80">{entry.name || entry.file.name}</p>
                </div>
              </div>
            )
          })}
        </div>
      )}

      {/* Inline editor */}
      {expandedId && (() => {
        const entry = files.find((f) => f.id === expandedId)
        if (!entry) return null
        const isUploading = entry.status === "uploading"
        const isDone = entry.status === "done"
        const isError = entry.status === "error"

        return (
          <div className={`rounded-xl border p-4 space-y-4 ${
            isError
              ? "border-red-500/30 bg-red-500/[0.04]"
              : `${accent.borderLight} bg-white/[0.02]`
          }`}>
            {/* Full preview */}
            <div className="overflow-hidden rounded-lg border border-white/10 bg-slate-800/50">
              <div className="flex h-48 items-center justify-center">
                {renderPreview(entry.file)}
              </div>
            </div>

            {/* Caller-provided form fields */}
            {!isDone && renderFields(entry, (updates) => updateEntry(entry.id, updates))}

            {/* Upload step progress */}
            {isUploading && currentStep && (
              <div className="flex items-center gap-2">
                <Loader2 className="h-4 w-4 animate-spin text-blue-400 shrink-0" />
                <span className="text-sm text-blue-300">{currentStep}</span>
              </div>
            )}

            {/* Error message */}
            {isError && entry.error && (
              <div className="flex items-center gap-2 text-sm text-red-400">
                <AlertCircle className="h-4 w-4 shrink-0" />
                {entry.error}
              </div>
            )}

            {/* Done message */}
            {isDone && (
              <div className="flex items-center gap-2 text-sm text-emerald-400">
                <CheckCircle className="h-4 w-4 shrink-0" />
                Uploaded successfully
              </div>
            )}

            {/* Individual upload / retry button */}
            {!isDone && !uploadingAll && (
              <button
                onClick={() => uploadSingle(entry.id)}
                disabled={isUploading || !isEntryReady(entry)}
                className={`flex w-full items-center justify-center gap-2 rounded-lg py-2 text-sm font-medium text-white transition-colors disabled:opacity-40 disabled:cursor-not-allowed ${
                  accentColor === "blue"
                    ? "bg-blue-600 hover:bg-blue-500"
                    : "bg-violet-600 hover:bg-violet-500"
                }`}
              >
                {isUploading ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : isError ? (
                  <RotateCcw className="h-4 w-4" />
                ) : (
                  <Upload className="h-4 w-4" />
                )}
                {isUploading ? "Uploading..." : isError ? "Retry" : "Upload"}
              </button>
            )}
          </div>
        )
      })()}

      {/* Upload All / Summary */}
      {hasFiles && !allDone && !uploadingAll && pendingFiles.length > 1 && (
        <button
          onClick={uploadAll}
          disabled={!allReady}
          className={`flex w-full items-center justify-center gap-2 rounded-lg py-2.5 text-sm font-medium text-white transition-colors disabled:opacity-40 disabled:cursor-not-allowed ${
            accentColor === "blue"
              ? "bg-blue-600 hover:bg-blue-500"
              : "bg-violet-600 hover:bg-violet-500"
          }`}
        >
          <Upload className="h-4 w-4" />
          Upload All ({pendingFiles.length} files)
        </button>
      )}

      {/* Upload All progress */}
      {uploadingAll && uploadProgress && (
        <div className="flex items-center justify-center gap-2 rounded-lg border border-white/10 bg-white/[0.02] py-2.5">
          <Loader2 className="h-4 w-4 animate-spin text-blue-400" />
          <span className="text-sm text-slate-300">
            Uploading {uploadProgress.current} of {uploadProgress.total}...
          </span>
        </div>
      )}

      {/* Completion summary */}
      {allDone && (
        <div className="space-y-3">
          <div className={`flex items-center gap-2 rounded-lg px-4 py-3 text-sm ${
            errorCount === 0
              ? "bg-emerald-500/10 text-emerald-400 border border-emerald-500/20"
              : "bg-amber-500/10 text-amber-300 border border-amber-500/20"
          }`}>
            {errorCount === 0 ? (
              <CheckCircle className="h-4 w-4 shrink-0" />
            ) : (
              <AlertCircle className="h-4 w-4 shrink-0" />
            )}
            {errorCount === 0
              ? `All ${doneCount} file${doneCount !== 1 ? "s" : ""} uploaded!`
              : `${doneCount} of ${files.length} uploaded successfully`
            }
          </div>
          <button
            onClick={clearAll}
            className="flex w-full items-center justify-center gap-2 rounded-lg border border-white/10 py-2 text-sm text-slate-400 transition-colors hover:bg-white/5"
          >
            <X className="h-4 w-4" />
            Clear All
          </button>
        </div>
      )}
    </div>
  )
}

export function useObjectUrl(file: File): string {
  const url = useMemo(() => URL.createObjectURL(file), [file])
  useEffect(() => {
    return () => URL.revokeObjectURL(url)
  }, [url])
  return url
}
