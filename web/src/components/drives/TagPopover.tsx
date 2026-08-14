import { useEffect, useRef, useState } from "react"
import { AddIcon, CloseIcon, SellIcon } from "@/components/icons"
import { cn } from "@/lib/utils"
import { fetchAllTagNames } from "@/api/tags"

interface TagPopoverProps {
  tags: string[]
  onChange: (tags: string[]) => Promise<void> | void
}

export function TagPopover({ tags, onChange }: TagPopoverProps) {
  const [open, setOpen] = useState(false)
  const [draft, setDraft] = useState("")
  const [busy, setBusy] = useState(false)
  const [suggestions, setSuggestions] = useState<string[]>([])
  const wrapRef = useRef<HTMLDivElement>(null)

  useEffect(() => {
    if (!open) return
    const onDoc = (e: MouseEvent) => {
      if (!wrapRef.current?.contains(e.target as Node)) setOpen(false)
    }
    document.addEventListener("mousedown", onDoc)
    return () => document.removeEventListener("mousedown", onDoc)
  }, [open])

  // Load tags currently used by another drive or charge.
  useEffect(() => {
    if (!open) return
    let cancelled = false
    fetchAllTagNames()
      .then((all) => {
        if (!cancelled) setSuggestions(all)
      })
      .catch(() => {})
    return () => {
      cancelled = true
    }
  }, [open])

  const addTag = async () => {
    const t = draft.trim()
    if (!t) return
    if (tags.includes(t)) {
      setDraft("")
      return
    }
    setBusy(true)
    try {
      await onChange([...tags, t])
      setDraft("")
    } finally {
      setBusy(false)
    }
  }

  const selectSuggestion = async (tag: string) => {
    if (tags.includes(tag)) return
    setBusy(true)
    try {
      await onChange([...tags, tag])
      setDraft("")
    } finally {
      setBusy(false)
    }
  }

  const removeTag = async (tag: string) => {
    setBusy(true)
    try {
      await onChange(tags.filter((t) => t !== tag))
    } finally {
      setBusy(false)
    }
  }

  const clearAll = async () => {
    if (tags.length === 0) return
    setBusy(true)
    try {
      await onChange([])
    } finally {
      setBusy(false)
    }
  }

  const hasTags = tags.length > 0
  const displayTag = hasTags ? tags[0] : null
  const extraCount = hasTags ? tags.length - 1 : 0

  const query = draft.trim().toLowerCase()
  const available = suggestions.filter(
    (s) => !tags.includes(s) && (!query || s.toLowerCase().includes(query)),
  )

  return (
    <div ref={wrapRef} className="relative">
      <button
        type="button"
        aria-label={hasTags ? `Tags: ${tags.join(", ")}` : "Add tag"}
        onClick={(e) => {
          e.stopPropagation()
          e.preventDefault()
          setOpen((o) => !o)
        }}
        className={cn(
          "inline-flex items-center gap-1 rounded-full transition-colors",
          hasTags
            ? "bg-emerald-400/15 px-2.5 py-0.5 text-xs font-medium text-emerald-200 ring-1 ring-inset ring-emerald-400/20 hover:bg-emerald-400/20"
            : "h-7 w-7 justify-center text-slate-500 hover:bg-white/5 hover:text-slate-300",
        )}
      >
        <SellIcon className={hasTags ? "h-3 w-3" : "h-4 w-4"} />
        {displayTag && <span>{displayTag}</span>}
        {extraCount > 0 && (
          <span className="text-emerald-300/80">+{extraCount}</span>
        )}
      </button>
      {open && (
        <div
          onClick={(e) => e.stopPropagation()}
          className="absolute right-0 top-full z-50 mt-2 w-64 rounded-xl border border-white/10 bg-slate-900/95 p-3 shadow-2xl backdrop-blur"
        >
          {hasTags && (
            <div className="mb-2 flex flex-wrap gap-1.5">
              {tags.map((t) => (
                <span
                  key={t}
                  className="inline-flex items-center gap-1 rounded-full bg-emerald-400/10 px-2 py-0.5 text-xs text-emerald-200"
                >
                  {t}
                  <button
                    type="button"
                    aria-label={`Remove tag ${t}`}
                    disabled={busy}
                    onClick={() => removeTag(t)}
                    className="text-emerald-300/70 hover:text-emerald-100 disabled:opacity-50"
                  >
                    <CloseIcon className="h-3 w-3" />
                  </button>
                </span>
              ))}
            </div>
          )}
          <div className="flex gap-1.5">
            <input
              type="text"
              autoFocus
              value={draft}
              onChange={(e) => setDraft(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") {
                  e.preventDefault()
                  addTag()
                }
              }}
              placeholder="New tag"
              className="min-w-0 flex-1 rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 placeholder:text-slate-600 focus:border-emerald-400/40 focus:outline-none"
            />
            <button
              type="button"
              disabled={busy || !draft.trim()}
              onClick={addTag}
              className="shrink-0 whitespace-nowrap rounded-md bg-emerald-500/90 px-2.5 py-1 text-xs font-medium text-slate-950 transition-colors hover:bg-emerald-400 disabled:opacity-50"
            >
              Add
            </button>
          </div>
          {available.length > 0 && (
            <div className="mt-2">
              <div className="mb-1 text-[10px] font-medium uppercase tracking-wide text-slate-500">
                {query ? "Matches" : "Existing tags"}
              </div>
              <div className="flex max-h-28 flex-wrap gap-1.5 overflow-y-auto">
                {available.map((s) => (
                  <button
                    key={s}
                    type="button"
                    disabled={busy}
                    onClick={() => selectSuggestion(s)}
                    className="inline-flex items-center gap-1 rounded-full border border-white/10 bg-white/5 px-2 py-0.5 text-xs text-slate-300 transition-colors hover:border-emerald-400/40 hover:bg-emerald-400/10 hover:text-emerald-200 disabled:opacity-50"
                  >
                    <AddIcon className="h-3 w-3" />
                    {s}
                  </button>
                ))}
              </div>
            </div>
          )}
          {hasTags && (
            <button
              type="button"
              disabled={busy}
              onClick={clearAll}
              className="mt-2 w-full rounded-md bg-rose-500/90 px-2.5 py-1 text-xs font-medium text-white transition-colors hover:bg-rose-400 disabled:opacity-50"
            >
              Clear all
            </button>
          )}
        </div>
      )}
    </div>
  )
}
