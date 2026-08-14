import { useState, useEffect } from "react"
import {
  CancelIcon,
  CheckCircleIcon,
  ChevronRightIcon,
  ErrorIcon,
  ExpandMoreIcon,
  ProgressActivityIcon,
  StethoscopeIcon,
  WarningIcon,
} from "@/components/icons"
import { cn } from "@/lib/utils"
import { Modal } from "@/components/ui/Modal"

type HealthItem = { name: string; status: "pass" | "warn" | "fail"; detail?: string }
type HealthCategory = { name: string; items: HealthItem[] }
type HealthReport = { summary: string; categories: HealthCategory[] }

export function HealthCheckModal({ onClose }: { onClose: () => void }) {
  const [loading, setLoading] = useState(true)
  const [report, setReport] = useState<HealthReport | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [expanded, setExpanded] = useState<Record<string, boolean>>({})

  async function runCheck() {
    setLoading(true)
    setError(null)
    setReport(null)
    try {
      const res = await fetch("/api/system/health-check")
      if (!res.ok) throw new Error(`Server responded with ${res.status}`)
      const data: HealthReport = await res.json()
      setReport(data)
      const exp: Record<string, boolean> = {}
      for (const cat of data.categories) {
        if (cat.items.some((i) => i.status !== "pass")) exp[cat.name] = true
      }
      setExpanded(exp)
    } catch (err) {
      setError(err instanceof Error ? err.message : "Health check failed")
    } finally {
      setLoading(false)
    }
  }

  // Run the initial check after mount.
  useEffect(() => {
    void runCheck()
  }, [])

  const statusIcon = (s: string) => {
    if (s === "pass") return <CheckCircleIcon className="h-3.5 w-3.5 text-emerald-400" />
    if (s === "warn") return <WarningIcon className="h-3.5 w-3.5 text-amber-400" />
    return <CancelIcon className="h-3.5 w-3.5 text-red-400" />
  }

  const failCount = report
    ? report.categories.reduce(
        (n, c) => n + c.items.filter((i) => i.status === "fail").length,
        0
      )
    : 0
  const warnCount = report
    ? report.categories.reduce(
        (n, c) => n + c.items.filter((i) => i.status === "warn").length,
        0
      )
    : 0

  const headerIconClass = error
    ? "text-red-400"
    : failCount > 0
    ? "text-red-400"
    : warnCount > 0
    ? "text-amber-400"
    : "text-emerald-400"

  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <StethoscopeIcon className={cn("h-4 w-4", headerIconClass)} />
          <span>Health Check</span>
          {report && !loading && (
            <span
              className={cn(
                "rounded-full px-2 py-0.5 text-xs font-medium",
                failCount > 0
                  ? "bg-red-500/15 text-red-400"
                  : warnCount > 0
                  ? "bg-amber-500/15 text-amber-400"
                  : "bg-emerald-500/15 text-emerald-400"
              )}
            >
              {report.summary}
            </span>
          )}
        </span>
      }
      onClose={onClose}
      size="md"
      footer={
        <div className="flex justify-end">
          <button
            onClick={runCheck}
            disabled={loading}
            className="rounded-lg px-3 py-1.5 text-xs text-slate-400 hover:bg-white/5 hover:text-slate-200 disabled:opacity-50"
          >
            {loading ? "Running..." : "Re-run"}
          </button>
        </div>
      }
    >
      {loading && (
        <div className="flex items-center justify-center py-8 text-slate-500">
          <ProgressActivityIcon className="mr-2 h-5 w-5 animate-spin" />
          Running health check...
        </div>
      )}

      {error && !loading && (
        <div className="flex items-start gap-3 rounded-xl border border-red-500/20 bg-red-500/5 p-4 text-sm">
          <ErrorIcon className="mt-0.5 h-5 w-5 shrink-0 text-red-400" />
          <div>
            <p className="font-medium text-red-300">Health check failed</p>
            <p className="mt-1 text-xs text-slate-400">{error}</p>
          </div>
        </div>
      )}

      {report &&
        !loading &&
        !error &&
        report.categories.map((cat) => {
          const isOpen = expanded[cat.name] ?? false
          const catFails = cat.items.filter((i) => i.status === "fail").length
          const catWarns = cat.items.filter((i) => i.status === "warn").length
          return (
            <div key={cat.name} className="border-b border-white/5 last:border-0">
              <button
                onClick={() => setExpanded((p) => ({ ...p, [cat.name]: !isOpen }))}
                className="flex w-full items-center gap-2 py-2 text-left"
              >
                {isOpen ? (
                  <ExpandMoreIcon className="h-3.5 w-3.5 text-slate-500" />
                ) : (
                  <ChevronRightIcon className="h-3.5 w-3.5 text-slate-500" />
                )}
                <span className="flex-1 text-xs font-medium text-slate-300">{cat.name}</span>
                {catFails > 0 && (
                  <span className="rounded-md bg-red-500/15 px-1.5 py-0.5 text-[10px] text-red-400">
                    {catFails} fail
                  </span>
                )}
                {catWarns > 0 && (
                  <span className="rounded-md bg-amber-500/15 px-1.5 py-0.5 text-[10px] text-amber-400">
                    {catWarns} warn
                  </span>
                )}
                {catFails === 0 && catWarns === 0 && (
                  <span className="rounded-md bg-emerald-500/15 px-1.5 py-0.5 text-[10px] text-emerald-400">
                    all pass
                  </span>
                )}
              </button>
              {isOpen && (
                <div className="mb-2 space-y-0.5 pl-5">
                  {cat.items.map((item, i) => (
                    <div key={i} className="flex items-start gap-2 py-0.5">
                      {statusIcon(item.status)}
                      <span className="text-xs text-slate-300">{item.name}</span>
                      {item.detail && (
                        <span className="text-xs text-slate-600">— {item.detail}</span>
                      )}
                    </div>
                  ))}
                </div>
              )}
            </div>
          )
        })}
    </Modal>
  )
}
