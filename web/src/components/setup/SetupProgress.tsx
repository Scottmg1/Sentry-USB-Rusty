import { useEffect, useMemo, useRef, useState } from "react"
import { AlertCircle, Check, Loader2, Terminal } from "lucide-react"
import { cn } from "@/lib/utils"

// ── Log line parser ────────────────────────────────────────────────────────

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

// ── Component ──────────────────────────────────────────────────────────────

const STALE_THRESHOLD_MS = 5 * 60 * 1000

type SetupPhaseStatus = "applying" | "running" | "rebooting" | "finalizing" | "complete" | "error"

interface PhaseEntry {
  id: string
  label: string
}

interface SetupProgressProps {
  complete?: boolean
  phase?: SetupPhaseStatus
}

export function SetupProgress({ complete, phase = "running" }: SetupProgressProps) {
  const [logLines, setLogLines] = useState<string[]>([])
  const [phases, setPhases] = useState<PhaseEntry[]>([])
  const [stale, setStale] = useState(false)
  const scrollRef = useRef<HTMLDivElement>(null)
  const prevLenRef = useRef(0)
  const lastChangeRef = useRef(Date.now())

  // Poll setup log as a fallback / catch-up mechanism only. Real-time log
  // lines arrive via the `setup_progress` WebSocket event in the next
  // effect — polling alone used to be the only source, which meant the
  // UI could be up to 3 seconds behind reality and the Pi would reboot
  // before the "Rebooting..." line ever showed up. The poll still
  // matters for (a) seeding on mount, (b) catching up after the server
  // disappears during a reboot.
  useEffect(() => {
    if (complete) return
    let cancelled = false
    async function poll() {
      try {
        const res = await fetch("/api/logs/setup")
        if (!res.ok) return
        const text = await res.text()
        if (cancelled) return
        setLogLines(text.split("\n").filter(Boolean))
      } catch {
        // server unreachable during reboot — expected
      }
    }
    poll()
    const id = setInterval(poll, 2000)
    return () => { cancelled = true; clearInterval(id) }
  }, [complete])

  // Seed the phase list from the server's persisted ledger, then subscribe
  // to `setup_phase` WebSocket events for live updates. Polling the ledger
  // periodically covers the gap when the server is down during a reboot.
  useEffect(() => {
    if (complete) return

    let cancelled = false

    async function fetchPhases() {
      try {
        const res = await fetch("/api/setup/phases")
        if (!res.ok) return
        const data = await res.json()
        if (cancelled) return
        const fetched: PhaseEntry[] = data.phases ?? []
        setPhases((prev) => {
          // Merge: fetched entries (authoritative order) + any WS-added
          // entries not yet in the fetched list.
          const seen = new Set(fetched.map((p) => p.id))
          const extras = prev.filter((p) => !seen.has(p.id))
          return [...fetched, ...extras]
        })
      } catch {
        // server unreachable — expected during reboot
      }
    }

    fetchPhases()
    const pollId = setInterval(fetchPhases, 4000)

    let ws: WebSocket | null = null
    let reconnectTimer: ReturnType<typeof setTimeout> | null = null
    let backoff = 2000

    function connect() {
      if (cancelled) return
      try {
        const protocol = window.location.protocol === "https:" ? "wss:" : "ws:"
        ws = new WebSocket(`${protocol}//${window.location.host}/api/ws`)
        ws.onopen = () => { backoff = 2000 }
        ws.onmessage = (event) => {
          try {
            const msg = JSON.parse(event.data)
            if (msg.type === "setup_phase") {
              const { id, label } = msg.data
              if (!id || !label) return
              setPhases((prev) => {
                if (prev.some((p) => p.id === id)) return prev
                return [...prev, { id, label }]
              })
            } else if (msg.type === "setup_progress") {
              // Live log append — the backend broadcasts this for every
              // `emitter.progress()` call, so lines land here well before
              // the 2s HTTP poll would pick them up. The next poll will
              // authoritative-rewrite the whole list, which papers over
              // any transient duplicate if the line is already in our
              // array from a previous poll.
              const text: string = msg.data?.message ?? ""
              if (!text) return
              setLogLines((prev) => {
                if (prev.length > 0 && prev[prev.length - 1] === text) return prev
                return [...prev, text]
              })
            }
          } catch { /* ignore */ }
        }
        ws.onclose = () => {
          if (cancelled) return
          reconnectTimer = setTimeout(() => {
            backoff = Math.min(backoff * 1.5, 15000)
            connect()
          }, backoff)
        }
      } catch { /* ws unavailable */ }
    }
    connect()

    return () => {
      cancelled = true
      clearInterval(pollId)
      if (reconnectTimer) clearTimeout(reconnectTimer)
      ws?.close()
    }
  }, [complete])

  // Auto-scroll log + stale detection
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

  const parsedLines = useMemo(() => logLines.map(parseLine), [logLines])
  const visibleLines = parsedLines.slice(-200)

  // Phase visualisation: done = all but the last; the last is in-progress
  // while setup is running, and marked done when complete/finalizing.
  const isDone = complete || phase === "complete" || phase === "finalizing"
  const activeIdx = isDone ? phases.length : Math.max(0, phases.length - 1)
  const headerLabel = isDone
    ? "Setup complete"
    : phase === "rebooting"
      ? "Rebooting to continue setup..."
      : phases.length === 0
        ? "Preparing..."
        : phases[phases.length - 1].label

  return (
    <div className="w-full space-y-5">
      {/* Current activity heading — centered above the two columns */}
      <div className="flex items-center justify-center gap-2.5 text-center">
        {isDone ? (
          <div className="flex h-7 w-7 shrink-0 items-center justify-center rounded-full bg-emerald-500/20">
            <Check className="h-3.5 w-3.5 text-emerald-400" />
          </div>
        ) : (
          <Loader2 className="h-5 w-5 shrink-0 animate-spin text-blue-400" />
        )}
        <div className="min-w-0">
          <div className="text-sm font-medium text-slate-200 truncate">
            {headerLabel}
          </div>
          {!isDone && phases.length > 0 && (
            <div className="text-[11px] text-slate-500 tabular-nums">
              Step {phases.length}
            </div>
          )}
        </div>
      </div>

      {/* Stale warning — full width above both columns */}
      {stale && !isDone && (
        <div className="flex items-start gap-2 rounded-xl border border-yellow-500/20 bg-yellow-500/5 px-3 py-2.5">
          <AlertCircle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-yellow-400" />
          <p className="text-xs text-yellow-300/80">
            No new progress in the last 5 minutes. Setup may be waiting on a slow
            operation (package install, large partition format), or it may be stuck.
            If this persists, check the system logs or power-cycle the device.
          </p>
        </div>
      )}

      {/* Two-column layout on lg+, stacked on mobile. The phase list and the
          setup log scroll independently so neither overflows the viewport
          when there are 20+ phases / hundreds of log lines. */}
      <div className="grid grid-cols-1 gap-5 lg:grid-cols-2 lg:items-start">
        {/* Phase list — left column */}
        {phases.length > 0 && (
          <div className="overflow-hidden rounded-xl border border-white/8 bg-white/[0.02]">
            <div className="border-b border-white/5 px-3.5 py-2 text-xs font-medium text-slate-500">
              Phases
              <span className="ml-2 text-[10px] tabular-nums text-slate-600">
                {phases.length}
              </span>
            </div>
            <ul className="max-h-[28rem] divide-y divide-white/5 overflow-y-auto lg:max-h-[34rem]">
              {phases.map((p, i) => {
                const done = i < activeIdx
                const active = !isDone && i === activeIdx
                return (
                  <li
                    key={p.id}
                    className="flex items-center gap-3 px-3.5 py-2"
                  >
                    <span className={cn(
                      "flex h-5 w-5 shrink-0 items-center justify-center rounded-full",
                      done
                        ? "bg-emerald-500/20"
                        : active
                          ? "bg-blue-500/20"
                          : "bg-white/5"
                    )}>
                      {done ? (
                        <Check className="h-3 w-3 text-emerald-400" />
                      ) : active ? (
                        <Loader2 className="h-3 w-3 animate-spin text-blue-400" />
                      ) : (
                        <span className="h-1 w-1 rounded-full bg-white/20" />
                      )}
                    </span>
                    <span className={cn(
                      "text-sm",
                      done ? "text-slate-500" : active ? "text-slate-200" : "text-slate-600"
                    )}>
                      {p.label}
                    </span>
                  </li>
                )
              })}
            </ul>
          </div>
        )}

        {/* Setup log — right column */}
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
            className="max-h-[28rem] overflow-y-auto p-3 font-mono text-[11px] leading-relaxed lg:max-h-[34rem]"
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
    </div>
  )
}
