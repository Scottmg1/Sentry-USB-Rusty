import { useState, useEffect, useRef, useCallback, useMemo } from "react"
import { ScrollText, Download, RefreshCw, ArrowDown, Loader2 } from "lucide-react"
import { cn } from "@/lib/utils"

const logTabs = [
  { id: "archiveloop", label: "Archive Loop", url: "/api/logs/archiveloop" },
  { id: "setup", label: "Setup Log", url: "/api/logs/setup" },
  { id: "diagnostics", label: "Diagnostics", url: "/api/logs/diagnostics" },
]

const SCROLL_THRESHOLD = 60

// ---------------------------------------------------------------------------
// Log line parser
//
// Shell logs:  "Fri 20 Mar 21:27:22 PDT 2026: some message"
// Go logs:     "Mon 21 Mar 14:30:45 UTC 2026: [drive-map] message"
// Format:       Day DD Mon HH:MM:SS TZ YYYY:
//
// We extract the time portion (HH:MM:SS), an optional [tag], and the message,
// then classify the level by keywords so we can color-code it.
// ---------------------------------------------------------------------------

type LogLevel = "error" | "warning" | "success" | "info" | "debug" | "default"

interface ParsedLine {
  date: string // e.g. "Mar 20"
  time: string
  tag: string
  message: string
  level: LogLevel
  raw: string
  fullTs: number // full timestamp in ms, used to detect clock jumps
}

// Matches: "Day DD Mon HH:MM:SS TZ YYYY:" at the start of a line
// Captures: (DD) (Mon) (HH:MM:SS) (YYYY)
const TIMESTAMP_RE =
  /^[A-Z][a-z]{2}\s+(\d{1,2})\s+([A-Z][a-z]{2})\s+(\d{2}:\d{2}:\d{2})\s+\w+\s+(\d{4}):\s*/

const MONTH_INDEX: Record<string, number> = {
  Jan: 0, Feb: 1, Mar: 2, Apr: 3, May: 4, Jun: 5,
  Jul: 6, Aug: 7, Sep: 8, Oct: 9, Nov: 10, Dec: 11,
}

// Matches a [tag] prefix after the timestamp
const TAG_RE = /^\[([^\]]+)\]\s*/

function classifyLevel(message: string, tag: string): LogLevel {
  // Away-mode lines are always amber/orange regardless of keywords
  if (tag === "away-mode") return "warning"

  const lower = message.toLowerCase()

  // Errors — check first so "failed" always wins over softer matches
  if (
    lower.includes("error") ||
    lower.includes("failed") ||
    lower.includes("giving up") ||
    lower.includes("unable to") ||
    lower.includes("killed") ||
    lower.includes("fatal") ||
    lower.includes("connection dead")
  )
    return "error"

  // Warnings
  if (
    lower.includes("warning") ||
    lower.includes("warn") ||
    lower.includes("retrying") ||
    lower.includes("retry") ||
    lower.includes("timed out") ||
    lower.includes("timeout") ||
    lower.includes("stale lock") ||
    lower.includes("still running") ||
    lower.includes("still processing") ||
    lower.includes("unreachable") ||
    lower.includes("skipping") ||
    lower.includes("falling back") ||
    lower.includes("network lost") ||
    lower.includes("not mounted") ||
    lower.includes("not supported") ||
    lower.includes("not enabled") ||
    lower.includes("incomplete") ||
    lower.includes("discarding") ||
    lower.includes("cycling wifi")
  )
    return "warning"

  // Success
  if (
    lower.includes("nudge ok") ||
    lower.includes("success") ||
    lower.includes("enabled") ||
    lower.includes("disabled") ||
    lower.includes("restored") ||
    lower.includes("complete") ||
    lower.includes("finished") ||
    lower.includes("started") ||
    lower.includes("ready") ||
    lower.includes("acquired") ||
    lower.includes("mounted") ||
    lower.includes("unmounted") ||
    lower.includes("connected") ||
    lower.includes("disconnected") ||
    lower.includes("reachable") ||
    lower.includes("reconnected") ||
    lower.includes("took snapshot") ||
    lower.includes("synced") ||
    lower.includes("done") ||
    lower.includes("up to date") ||
    lower.includes("trim complete") ||
    lower.includes("rebuilt") ||
    lower.includes("removed") ||
    lower.includes("time adjusted")
  )
    return "success"

  // Info (tagged lines or informational keywords)
  if (
    tag ||
    lower.includes("starting") ||
    lower.includes("archiving") ||
    lower.includes("processing") ||
    lower.includes("syncing") ||
    lower.includes("copying") ||
    lower.includes("checking") ||
    lower.includes("running") ||
    lower.includes("disabling") ||
    lower.includes("restoring") ||
    lower.includes("queued") ||
    lower.includes("waiting") ||
    lower.includes("trimming") ||
    lower.includes("cleaning") ||
    lower.includes("making") ||
    lower.includes("comparing") ||
    lower.includes("detecting") ||
    lower.includes("rebuilding") ||
    lower.includes("taking snapshot") ||
    lower.includes("ensuring") ||
    lower.includes("tearing down") ||
    lower.includes("temperature monitor") ||
    lower.includes("command sent") ||
    lower.includes("update available")
  )
    return "info"

  return "default"
}

function parseLine(raw: string): ParsedLine {
  let rest = raw
  let date = ""
  let time = ""
  let tag = ""
  let fullTs = 0

  // Extract timestamp — captures (DD) (Mon) (HH:MM:SS) (YYYY)
  const tsMatch = rest.match(TIMESTAMP_RE)
  if (tsMatch) {
    const day = parseInt(tsMatch[1], 10)
    const month = MONTH_INDEX[tsMatch[2]] ?? 0
    const year = parseInt(tsMatch[4], 10)
    date = `${tsMatch[2]} ${tsMatch[1]}` // e.g. "Mar 20"
    time = tsMatch[3]
    const [h, m, s] = time.split(":").map(Number)
    fullTs = new Date(year, month, day, h, m, s).getTime()
    rest = rest.slice(tsMatch[0].length)
  }

  // Extract [tag]
  const tagMatch = rest.match(TAG_RE)
  if (tagMatch) {
    tag = tagMatch[1]
    rest = rest.slice(tagMatch[0].length)
  }

  const message = rest
  const level = classifyLevel(message, tag)

  return { date, time, tag, message, level, raw, fullTs }
}

// Colors for each level
const levelColors: Record<LogLevel, { text: string; tag: string }> = {
  error:   { text: "text-red-400",    tag: "text-red-500"    },
  warning: { text: "text-amber-400",  tag: "text-amber-500"  },
  success: { text: "text-emerald-400", tag: "text-emerald-500" },
  info:    { text: "text-blue-400",   tag: "text-blue-500"   },
  debug:   { text: "text-slate-500",  tag: "text-slate-600"  },
  default: { text: "text-slate-300",  tag: "text-slate-500"  },
}

function LogLine({ parsed }: { parsed: ParsedLine }) {
  const colors = levelColors[parsed.level]

  return (
    <span className="block">
      {parsed.time && (
        <span className="text-slate-500 select-none">{parsed.time}  </span>
      )}
      {parsed.tag && (
        <span className={cn("font-semibold", colors.tag)}>
          [{parsed.tag}]
          {"  "}
        </span>
      )}
      <span className={colors.text}>{parsed.message}</span>
    </span>
  )
}

function FormattedLog({ content }: { content: string }) {
  const lines = useMemo(() => {
    if (!content) return []
    return content.split("\n").map((line) => parseLine(line))
  }, [content])

  // Track the last displayed date string so we only show a date header
  // when the date actually changes (or at the start of a boot cycle).
  let prevDate = ""
  let inBootCycle = false // becomes true after we see the first entry

  return (
    <>
      {lines.map((parsed, i) => {
        if (parsed.raw.trim() === "") {
          return <span key={i} className="block">{"\n"}</span>
        }

        // Boot cycle separator (====== lines from archiveloop)
        if (parsed.raw.trim().startsWith("=====")) {
          prevDate = "" // reset — new boot cycle
          inBootCycle = false
          return (
            <span key={i} className="block border-b border-slate-700/40 my-3" />
          )
        }

        // Show a date header when:
        // 1. First timestamped entry in a boot cycle
        // 2. The date string actually changes (new day or clock correction)
        let dateSeparator = null
        if (parsed.date) {
          if (!inBootCycle || parsed.date !== prevDate) {
            dateSeparator = (
              <span className="block border-b border-slate-700/50 pb-1 pt-3 mb-1 text-xs font-medium text-slate-500 select-none">
                — {parsed.date} —
              </span>
            )
          }
          prevDate = parsed.date
          inBootCycle = true
        }

        return (
          <span key={i}>
            {dateSeparator}
            <LogLine parsed={parsed} />
          </span>
        )
      })}
    </>
  )
}

export default function Logs() {
  const [activeTab, setActiveTab] = useState("archiveloop")
  const [content, setContent] = useState<string>("Loading...")
  const [loading, setLoading] = useState(false)
  const [showScrollBtn, setShowScrollBtn] = useState(false)
  const preRef = useRef<HTMLPreElement>(null)
  const followRef = useRef(true)

  const activeLog = logTabs.find((t) => t.id === activeTab)!

  // Format archiveloop and setup logs (same timestamp format).
  // Diagnostics is a structured system-info dump — keep it raw.
  const shouldFormat = activeTab === "archiveloop" || activeTab === "setup"

  // With flex-direction: column-reverse, scrollTop is 0 at the bottom
  // and becomes negative as you scroll up.
  const handleScroll = useCallback(() => {
    const el = preRef.current
    if (!el) return
    const atBottom = el.scrollTop >= -SCROLL_THRESHOLD
    followRef.current = atBottom
    setShowScrollBtn(!atBottom)
  }, [])

  function scrollToBottom() {
    if (preRef.current) {
      preRef.current.scrollTop = 0 // 0 = bottom in column-reverse
      followRef.current = true
      setShowScrollBtn(false)
    }
  }

  useEffect(() => {
    followRef.current = true
    setShowScrollBtn(false)
  }, [activeTab])

  useEffect(() => {
    let mounted = true
    setLoading(true)
    setContent("")

    async function fetchLog() {
      try {
        const url =
          activeTab === "diagnostics"
            ? "/api/diagnostics?" + Math.random()
            : activeLog.url + "?" + Math.random()
        const res = await fetch(url)
        const text = await res.text()
        if (mounted) {
          if (!res.ok && activeTab !== "diagnostics") {
            setContent("Log file not available. It may not exist yet.")
          } else {
            setContent(text || "(empty)")
          }
          setLoading(false)
        }
      } catch {
        if (mounted) {
          setContent("Unable to connect to Sentry USB. Is the device online?")
          setLoading(false)
        }
      }
    }

    fetchLog()

    const interval =
      activeTab !== "diagnostics" ? setInterval(fetchLog, 2000) : undefined

    return () => {
      mounted = false
      if (interval) clearInterval(interval)
    }
  }, [activeLog.url, activeTab])

  function handleDownload() {
    const blob = new Blob([content], { type: "text/plain" })
    const url = URL.createObjectURL(blob)
    const a = document.createElement("a")
    a.href = url
    a.download = `${activeTab}.log`
    a.click()
    URL.revokeObjectURL(url)
  }

  async function handleRefreshDiagnostics() {
    setLoading(true)
    setContent("Generating diagnostics...")
    try {
      await fetch("/api/diagnostics/refresh", { method: "POST" })
      await new Promise((r) => setTimeout(r, 3000))
      const res = await fetch("/api/logs/diagnostics?" + Math.random())
      const text = await res.text()
      setContent(text || "(empty)")
    } catch {
      setContent("Failed to generate diagnostics")
    }
    setLoading(false)
  }

  return (
    <div className="flex h-[calc(100vh-120px)] flex-col space-y-4 md:h-[calc(100vh-96px)]">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <h1 className="text-2xl font-bold text-slate-100">Logs</h1>
          <p className="mt-1 text-sm text-slate-500">
            System logs and diagnostics
          </p>
        </div>
        <div className="flex gap-2">
          {activeTab === "diagnostics" && (
            <button
              onClick={handleRefreshDiagnostics}
              disabled={loading}
              className="glass-card glass-card-hover flex items-center gap-1.5 px-3 py-1.5 text-sm text-slate-400 transition-colors hover:text-slate-200 disabled:opacity-50"
            >
              <RefreshCw
                className={cn("h-4 w-4", loading && "animate-spin")}
              />
              Refresh
            </button>
          )}
          <button
            onClick={handleDownload}
            className="glass-card glass-card-hover flex items-center gap-1.5 px-3 py-1.5 text-sm text-slate-400 transition-colors hover:text-slate-200"
          >
            <Download className="h-4 w-4" />
            Download
          </button>
        </div>
      </div>

      {/* Tab bar */}
      <div className="flex gap-1">
        {logTabs.map((tab) => (
          <button
            key={tab.id}
            onClick={() => setActiveTab(tab.id)}
            className={cn(
              "rounded-lg px-3 py-1.5 text-sm font-medium transition-colors",
              activeTab === tab.id
                ? "bg-blue-500/15 text-blue-400"
                : "text-slate-500 hover:bg-white/5 hover:text-slate-300"
            )}
          >
            {tab.label}
          </button>
        ))}
      </div>

      {/* Log output */}
      <div className="glass-card relative flex-1 overflow-hidden">
        <pre
          ref={preRef}
          onScroll={handleScroll}
          className="flex h-full flex-col-reverse overflow-auto p-4 font-mono text-xs leading-relaxed text-slate-300"
        >
          <div>
            {loading && !content ? (
              <span className="flex items-center gap-2 text-slate-600">
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                Loading...
              </span>
            ) : content ? (
              shouldFormat ? (
                <FormattedLog content={content} />
              ) : (
                content
              )
            ) : (
              <span className="flex items-center gap-2 text-slate-600">
                <ScrollText className="h-4 w-4" />
                No log content
              </span>
            )}
          </div>
        </pre>
        {showScrollBtn && (
          <button
            onClick={scrollToBottom}
            className="absolute bottom-4 right-6 flex items-center gap-1.5 rounded-full bg-blue-500/90 px-3 py-1.5 text-xs font-medium text-white shadow-lg backdrop-blur transition-opacity hover:bg-blue-500"
          >
            <ArrowDown className="h-3.5 w-3.5" />
            Follow
          </button>
        )}
      </div>
    </div>
  )
}
