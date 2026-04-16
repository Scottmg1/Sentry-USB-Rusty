import { useEffect, useRef, useState, useMemo } from "react"
import { AlertCircle, CheckCircle, Loader2, Terminal } from "lucide-react"
import { cn } from "@/lib/utils"

// ── Stage markers for progress estimation ──────────────────────────────────

const STAGE_MARKERS: [RegExp, number][] = [
  [/SENTRYUSB_SETUP_STARTED|rc\.local/, 5],
  [/Downloading common runtime/, 10],
  [/Updating package index/, 20],
  [/Upgrading installed packages/, 30],
  [/cmdline\.txt/, 40],
  [/Configuring the hostname/, 45],
  [/Mounting.*backing/, 50],
  [/Creating backing disk/, 60],
  [/create-backingfiles/, 65],
  [/archiveloop|archive/, 75],
  [/calling configure\.sh/, 80],
  [/configure-web\.sh|configure-ssh\.sh/, 85],
  [/make-root-fs-readonly:/, 90],
  [/Running post-setup tasks/, 95],
  [/All done\./, 98],
  [/SETUP_FINISHED|setup completed/i, 100],
]

function estimateProgress(logText: string): number {
  let highest = 0
  for (const [pattern, pct] of STAGE_MARKERS) {
    if (pattern.test(logText)) {
      highest = Math.max(highest, pct)
    }
  }
  return highest
}

// ── Log line parser (lightweight version of the Logs page parser) ──────────

type LogLevel = "error" | "warning" | "success" | "info" | "default"

interface ParsedLine {
  time: string
  tag: string
  message: string
  level: LogLevel
}

const TIMESTAMP_RE =
  /^[A-Z][a-z]{2}\s+\d{1,2}\s+[A-Z][a-z]{2}\s+(\d{2}:\d{2}:\d{2})\s+\w+\s+\d{4}:\s*/
const TAG_RE = /^\[([^\]]+)\]\s*/

function classifyLevel(message: string): LogLevel {
  const lower = message.toLowerCase()
  if (lower.includes("error") || lower.includes("failed") || lower.includes("fatal"))
    return "error"
  if (lower.includes("warning") || lower.includes("retrying") || lower.includes("timeout"))
    return "warning"
  if (
    lower.includes("complete") || lower.includes("success") || lower.includes("done") ||
    lower.includes("mounted") || lower.includes("connected") || lower.includes("ready") ||
    lower.includes("finished")
  )
    return "success"
  if (
    lower.includes("starting") || lower.includes("downloading") || lower.includes("configuring") ||
    lower.includes("creating") || lower.includes("running") || lower.includes("installing")
  )
    return "info"
  return "default"
}

function parseLine(raw: string): ParsedLine {
  let rest = raw
  let time = ""
  let tag = ""

  const tsMatch = rest.match(TIMESTAMP_RE)
  if (tsMatch) {
    time = tsMatch[1]
    rest = rest.slice(tsMatch[0].length)
  }

  const tagMatch = rest.match(TAG_RE)
  if (tagMatch) {
    tag = tagMatch[1]
    rest = rest.slice(tagMatch[0].length)
  }

  return { time, tag, message: rest, level: classifyLevel(rest) }
}

const levelColors: Record<LogLevel, { text: string; tag: string }> = {
  error:   { text: "text-red-400",     tag: "text-red-500"     },
  warning: { text: "text-amber-400",   tag: "text-amber-500"   },
  success: { text: "text-emerald-400", tag: "text-emerald-500" },
  info:    { text: "text-blue-400",    tag: "text-blue-500"    },
  default: { text: "text-slate-400",   tag: "text-slate-500"   },
}

// ── Phase step definitions ─────────────────────────────────────────────────

type SetupPhase = "applying" | "running" | "rebooting" | "finalizing" | "complete" | "error"

const PHASE_STEPS = [
  { id: "save",      label: "Save Config" },
  { id: "download",  label: "Download" },
  { id: "setup",     label: "Setup" },
  { id: "reboot",    label: "Reboot" },
  { id: "done",      label: "Done" },
] as const

function getActiveStepIndex(phase: SetupPhase, progress: number): number {
  if (phase === "complete") return 4
  if (phase === "finalizing") return 3
  if (phase === "rebooting") return 3
  if (phase === "running") {
    if (progress < 15) return 1 // downloading
    return 2 // setup
  }
  return 0 // applying / save
}

// ── Component ──────────────────────────────────────────────────────────────

const STALE_THRESHOLD_MS = 5 * 60 * 1000

interface SetupProgressProps {
  complete?: boolean
  phase?: SetupPhase
}

export function SetupProgress({ complete, phase = "running" }: SetupProgressProps) {
  const [logLines, setLogLines] = useState<string[]>([])
  const [progress, setProgress] = useState(0)
  const [stale, setStale] = useState(false)
  const scrollRef = useRef<HTMLDivElement>(null)
  const prevLenRef = useRef(0)
  const lastChangeRef = useRef(Date.now())

  useEffect(() => {
    if (complete) {
      setProgress(100)
      return
    }

    let cancelled = false
    async function poll() {
      try {
        const res = await fetch("/api/logs/setup")
        if (!res.ok) return
        const text = await res.text()
        if (cancelled) return
        const lines = text.split("\n").filter(Boolean)
        setLogLines(lines)
        setProgress(estimateProgress(text))
      } catch {
        // server unreachable during reboot — expected
      }
    }

    poll()
    const id = setInterval(poll, 3000)
    return () => {
      cancelled = true
      clearInterval(id)
    }
  }, [complete])

  // Auto-scroll + stale tracking
  useEffect(() => {
    if (logLines.length > prevLenRef.current) {
      if (scrollRef.current) scrollRef.current.scrollTop = scrollRef.current.scrollHeight
      lastChangeRef.current = Date.now()
      setStale(false)
    }
    prevLenRef.current = logLines.length
  }, [logLines])

  useEffect(() => {
    if (complete) return
    const id = setInterval(() => {
      if (logLines.length > 0 && Date.now() - lastChangeRef.current > STALE_THRESHOLD_MS) {
        setStale(true)
      }
    }, 15000)
    return () => clearInterval(id)
  }, [complete, logLines.length])

  const pct = complete ? 100 : progress
  const activeStep = getActiveStepIndex(complete ? "complete" : phase, progress)

  const parsedLines = useMemo(() => {
    return logLines.map(parseLine)
  }, [logLines])

  // Only show the last 200 lines to keep rendering fast
  const visibleLines = parsedLines.slice(-200)

  return (
    <div className="w-full space-y-5">
      {/* Phase steps */}
      <div className="flex items-center gap-1">
        {PHASE_STEPS.map((step, i) => {
          const isActive = i === activeStep
          const isDone = i < activeStep
          const isFuture = i > activeStep
          return (
            <div key={step.id} className="flex flex-1 flex-col items-center gap-1.5">
              <div className="flex w-full items-center">
                {i > 0 && (
                  <div className={cn(
                    "h-px flex-1 transition-colors duration-500",
                    isDone ? "bg-emerald-500/50" : isActive ? "bg-blue-500/30" : "bg-white/5"
                  )} />
                )}
                <div className={cn(
                  "flex h-7 w-7 shrink-0 items-center justify-center rounded-full transition-all duration-500",
                  isDone
                    ? "bg-emerald-500/20"
                    : isActive
                      ? "bg-blue-500/20 shadow-[0_0_12px_rgba(59,130,246,0.15)]"
                      : "bg-white/5"
                )}>
                  {isDone ? (
                    <CheckCircle className="h-3.5 w-3.5 text-emerald-400" />
                  ) : isActive ? (
                    <Loader2 className="h-3.5 w-3.5 animate-spin text-blue-400" />
                  ) : (
                    <span className={cn(
                      "h-1.5 w-1.5 rounded-full",
                      isFuture ? "bg-white/10" : "bg-white/20"
                    )} />
                  )}
                </div>
                {i < PHASE_STEPS.length - 1 && (
                  <div className={cn(
                    "h-px flex-1 transition-colors duration-500",
                    isDone ? "bg-emerald-500/50" : "bg-white/5"
                  )} />
                )}
              </div>
              <span className={cn(
                "text-[10px] font-medium transition-colors",
                isDone ? "text-emerald-400/70" : isActive ? "text-blue-400" : "text-slate-600"
              )}>
                {step.label}
              </span>
            </div>
          )
        })}
      </div>

      {/* Progress bar */}
      <div className="space-y-1.5">
        <div className="flex items-center justify-between text-xs">
          <span className="font-medium text-slate-400">
            {pct >= 100 ? "Complete" : "Setting up..."}
          </span>
          <span className="tabular-nums text-slate-500">{pct}%</span>
        </div>
        <div className="h-2 w-full overflow-hidden rounded-full bg-white/5">
          <div
            className={cn(
              "h-full rounded-full transition-all duration-700 ease-out",
              pct >= 100 ? "" : "animate-progress-stripe"
            )}
            style={{
              width: `${pct}%`,
              background: pct >= 100
                ? "rgb(52, 211, 153)"
                : "linear-gradient(90deg, rgb(59,130,246), rgb(99,102,241), rgb(59,130,246))",
              backgroundSize: pct >= 100 ? "100% 100%" : "200% 100%",
            }}
          />
        </div>
      </div>

      {/* Stale progress warning */}
      {stale && (
        <div className="flex items-start gap-2 rounded-xl border border-yellow-500/20 bg-yellow-500/5 px-3 py-2.5">
          <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-yellow-400" />
          <p className="text-xs text-yellow-300/80">
            No new progress in the last 5 minutes. Setup may be waiting on a slow
            operation (package install, large partition format), or it may be stuck.
            If this persists, check the system logs or power-cycle the device.
          </p>
        </div>
      )}

      {/* Log journal */}
      <div className="overflow-hidden rounded-xl border border-white/8 bg-black/30">
        <div className="flex items-center gap-2 border-b border-white/5 px-3 py-2">
          <Terminal className="h-3.5 w-3.5 text-slate-500" />
          <span className="text-xs font-medium text-slate-500">Setup Log</span>
          {logLines.length > 0 && (
            <span className="ml-auto text-[10px] tabular-nums text-slate-600">
              {logLines.length} lines
            </span>
          )}
        </div>
        <div
          ref={scrollRef}
          className="max-h-56 overflow-y-auto p-3 font-mono text-[11px] leading-relaxed"
        >
          {logLines.length === 0 ? (
            <div className="flex items-center gap-2 text-slate-600">
              <Loader2 className="h-3 w-3 animate-spin" />
              Waiting for setup log...
            </div>
          ) : (
            visibleLines.map((parsed, i) => {
              const colors = levelColors[parsed.level]
              return (
                <div key={i} className="whitespace-pre-wrap break-all">
                  {parsed.time && (
                    <span className="text-slate-600 select-none">{parsed.time}  </span>
                  )}
                  {parsed.tag && (
                    <span className={cn("font-semibold", colors.tag)}>
                      [{parsed.tag}]{"  "}
                    </span>
                  )}
                  <span className={colors.text}>{parsed.message}</span>
                </div>
              )
            })
          )}
        </div>
      </div>
    </div>
  )
}
