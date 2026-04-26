import { useState, useRef, useCallback } from "react"
import { Users, Paintbrush, Volume2, Shield, X, Loader2 } from "lucide-react"
import { Link } from "react-router-dom"
import CommunityWraps from "./CommunityWraps"
import LockChime from "./LockChime"
import { useCommunityPrefs } from "@/hooks/useCommunityPrefs"

const API_BASE = "/api"

type CommunityView = "wraps" | "chimes"

export default function Community() {
  const { mode, loading } = useCommunityPrefs()
  const [view, setView] = useState<CommunityView>("wraps")
  const [adminPasscode, setAdminPasscode] = useState<string | null>(null)
  const [showPasscodePrompt, setShowPasscodePrompt] = useState(false)
  const clickCountRef = useRef(0)
  const clickTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  const handleHeadingClick = useCallback(() => {
    if (adminPasscode) {
      clickCountRef.current++
      if (clickTimerRef.current) clearTimeout(clickTimerRef.current)
      clickTimerRef.current = setTimeout(() => { clickCountRef.current = 0 }, 2000)
      if (clickCountRef.current >= 5) {
        clickCountRef.current = 0
        setAdminPasscode(null)
      }
      return
    }

    clickCountRef.current++
    if (clickTimerRef.current) clearTimeout(clickTimerRef.current)
    clickTimerRef.current = setTimeout(() => { clickCountRef.current = 0 }, 2000)
    if (clickCountRef.current >= 5) {
      clickCountRef.current = 0
      setShowPasscodePrompt(true)
    }
  }, [adminPasscode])

  if (loading) {
    return (
      <div className="flex items-center justify-center py-16">
        <Loader2 className="h-5 w-5 animate-spin text-slate-500" />
      </div>
    )
  }

  if (mode === "none") {
    return (
      <div className="space-y-6">
        <div className="flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-slate-500/10">
            <Users className="h-5 w-5 text-slate-500" />
          </div>
          <div>
            <h1 className="text-xl font-semibold text-slate-100">Community</h1>
            <p className="text-xs text-slate-500">Disabled</p>
          </div>
        </div>

        <div className="glass-card flex flex-col items-start gap-3 p-6">
          <p className="text-sm text-slate-300">
            Community features are disabled. Enable Wraps or Lock Chimes from Settings to use this section.
          </p>
          <Link
            to="/settings"
            className="rounded-lg bg-blue-500 px-4 py-2 text-sm font-medium text-white transition-colors hover:bg-blue-600"
          >
            Open Settings
          </Link>
        </div>
      </div>
    )
  }

  // Single-feature modes lock the view to the enabled feature.
  const effectiveView: CommunityView = mode === "wraps-only"
    ? "wraps"
    : mode === "chimes-only"
      ? "chimes"
      : view

  const headingTitle = mode === "wraps-only"
    ? "Wraps"
    : mode === "chimes-only"
      ? "Lock Chimes"
      : "Community"

  const headingSubtitle = mode === "wraps-only"
    ? "Community wraps"
    : mode === "chimes-only"
      ? "Lock chime sounds"
      : "Wraps & Chimes"

  const HeadingIcon = mode === "wraps-only"
    ? Paintbrush
    : mode === "chimes-only"
      ? Volume2
      : Users

  return (
    <div className="space-y-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-blue-500/20">
            <HeadingIcon className="h-5 w-5 text-blue-400" />
          </div>
          <div>
            <h1
              className="cursor-default select-none text-xl font-semibold text-slate-100"
              onClick={handleHeadingClick}
            >{headingTitle}</h1>
            <p className="text-xs text-slate-500">{headingSubtitle}</p>
          </div>
        </div>
        {adminPasscode && (
          <div className="flex items-center gap-1.5 rounded bg-red-500/10 border border-red-500/20 px-2.5 py-1 text-xs text-red-400">
            <Shield className="h-3 w-3" />
            Admin Mode
            <button onClick={() => setAdminPasscode(null)} className="ml-1 hover:text-red-300">
              <X className="h-3 w-3" />
            </button>
          </div>
        )}
      </div>

      {/* Toggle Switch — only visible when both features are enabled */}
      {mode === "both" && (
        <div className="flex items-center gap-1 rounded-lg bg-white/[0.03] border border-white/10 p-1 w-fit">
          <button
            onClick={() => setView("wraps")}
            className={`flex items-center gap-2 rounded-md px-4 py-2 text-sm font-medium transition-colors ${
              view === "wraps"
                ? "bg-blue-500/15 text-blue-400"
                : "text-slate-400 hover:text-slate-200"
            }`}
          >
            <Paintbrush className="h-4 w-4" />
            Wraps
          </button>
          <button
            onClick={() => setView("chimes")}
            className={`flex items-center gap-2 rounded-md px-4 py-2 text-sm font-medium transition-colors ${
              view === "chimes"
                ? "bg-blue-500/15 text-blue-400"
                : "text-slate-400 hover:text-slate-200"
            }`}
          >
            <Volume2 className="h-4 w-4" />
            Chimes
          </button>
        </div>
      )}

      {/* Content */}
      {effectiveView === "wraps"
        ? <CommunityWraps adminPasscode={adminPasscode} onAdminPasscodeChange={setAdminPasscode} />
        : <LockChime adminPasscode={adminPasscode} onAdminPasscodeChange={setAdminPasscode} />
      }

      {/* Shared passcode modal */}
      {showPasscodePrompt && (
        <PasscodeModal
          view={effectiveView}
          onSuccess={(passcode) => {
            setAdminPasscode(passcode)
            setShowPasscodePrompt(false)
          }}
          onClose={() => setShowPasscodePrompt(false)}
        />
      )}
    </div>
  )
}

function PasscodeModal({ view, onSuccess, onClose }: { view: CommunityView; onSuccess: (passcode: string) => void; onClose: () => void }) {
  const [input, setInput] = useState("")
  const [error, setError] = useState<string | null>(null)
  const [validating, setValidating] = useState(false)

  const handleValidate = async () => {
    if (!input.trim()) return
    setValidating(true)
    setError(null)
    try {
      const endpoint = view === "wraps"
        ? `${API_BASE}/wraps/admin/validate`
        : `${API_BASE}/lockchime/community/admin/validate`
      const res = await fetch(endpoint, {
        method: "POST",
        headers: { "x-passcode": input.trim() },
      })
      if (res.ok) {
        onSuccess(input.trim())
      } else {
        setError("Invalid passcode")
        setInput("")
      }
    } catch {
      setError("Connection failed")
    } finally {
      setValidating(false)
    }
  }

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center bg-black/60 p-4" onClick={onClose}>
      <div
        className="w-full max-w-sm overflow-hidden rounded-2xl border border-white/10 bg-slate-900 p-6"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="text-lg font-semibold text-slate-100">Admin Access</h3>
        <p className="mt-1 text-xs text-slate-500">Enter the admin passcode to continue</p>
        <input
          type="password"
          value={input}
          onChange={(e) => setInput(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && handleValidate()}
          placeholder="Passcode"
          autoFocus
          className="mt-4 w-full rounded-lg border border-white/10 bg-white/[0.03] px-3 py-2 text-sm text-slate-200 placeholder:text-slate-600 focus:border-blue-500/50 focus:outline-none"
        />
        {error && <p className="mt-2 text-xs text-red-400">{error}</p>}
        <div className="mt-4 flex gap-3">
          <button
            onClick={handleValidate}
            disabled={!input.trim() || validating}
            className="flex flex-1 items-center justify-center gap-2 rounded-lg bg-blue-600 px-4 py-2.5 text-sm font-medium text-white transition-colors hover:bg-blue-500 disabled:opacity-50"
          >
            {validating && <Loader2 className="h-4 w-4 animate-spin" />}
            Validate
          </button>
          <button
            onClick={onClose}
            className="rounded-lg border border-white/10 px-4 py-2.5 text-sm text-slate-400 transition-colors hover:bg-white/5"
          >
            Cancel
          </button>
        </div>
      </div>
    </div>
  )
}
