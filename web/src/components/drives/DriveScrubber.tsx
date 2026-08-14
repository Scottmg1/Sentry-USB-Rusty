import { useMemo, useRef } from "react"
import { PauseIcon, PlayArrowIcon } from "@/components/icons"
import { cn } from "@/lib/utils"
import { useScrubberActions, useScrubberState } from "@/hooks/useScrubberSync"

interface DriveScrubberProps {
  points: [number, number, number, number][]
  startTime: string
  // FSD states must be index-aligned with points.
  fsdStates?: number[]
}

const SPEEDS = [0.5, 1, 2, 5] as const

// Match DriveMap's palette; literal blue avoids the theme-remapped Tailwind token.
const COLOR_MANUAL = "#3b82f6"
const COLOR_FSD = "#34d399"

export function DriveScrubber({ points, startTime, fsdStates }: DriveScrubberProps) {
  const { currentIndex, playing, playbackSpeed } = useScrubberState()
  const { setIndex, setPlaying, setPlaybackSpeed } = useScrubberActions()
  const max = Math.max(0, points.length - 1)
  const n = points.length

  // Limit drag updates to one per animation frame.
  const rafRef = useRef<number | null>(null)
  const pendingRef = useRef<number | null>(null)
  const onSliderInput = (val: number) => {
    pendingRef.current = val
    if (rafRef.current === null) {
      rafRef.current = requestAnimationFrame(() => {
        const v = pendingRef.current
        if (v !== null) setIndex(v)
        rafRef.current = null
        pendingRef.current = null
      })
    }
  }

  // Render only contiguous engaged runs; mismatched data falls back to one track.
  const fsdSegments = useMemo(() => {
    if (!fsdStates || fsdStates.length !== n || n === 0) return null
    const out: { start: number; end: number }[] = []
    let curStart = 0
    let curOn = fsdStates[0] > 0
    for (let i = 1; i < n; i++) {
      const on = fsdStates[i] > 0
      if (on !== curOn) {
        if (curOn) out.push({ start: curStart, end: i })
        curStart = i
        curOn = on
      }
    }
    if (curOn) out.push({ start: curStart, end: n })
    return out
  }, [fsdStates, n])

  const baseMs = new Date(startTime).getTime()
  const driveStartLabel =
    points.length > 0 ? formatPointTime(points[0][2], baseMs) : "—"
  const driveEndLabel =
    points.length > 0 ? formatPointTime(points[max][2], baseMs) : "—"
  const currentLabel =
    points.length > 0
      ? formatPointTime(points[Math.min(currentIndex, max)][2], baseMs)
      : "—"

  const cursorPct = max > 0 ? (currentIndex / max) * 100 : 0

  const togglePlay = () => {
    if (!playing && currentIndex >= max) {
      setIndex(0)
    }
    setPlaying(!playing)
  }

  return (
    <div className="mt-3 pb-5">
      <div className="flex items-center gap-3">
        <button
          type="button"
          onClick={togglePlay}
          className="inline-flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-emerald-500/95 text-slate-950 transition-colors hover:bg-emerald-400"
          aria-label={playing ? "Pause" : "Play"}
        >
          {playing ? <PauseIcon className="h-4 w-4" /> : <PlayArrowIcon className="h-4 w-4 translate-x-px" />}
        </button>

        <span className="w-16 shrink-0 text-right text-xs tabular-nums text-slate-400">
          {driveStartLabel}
        </span>

        <div className="relative h-4 flex-1">
          {/* The track leaves vertical room for the thumb. */}
          <div
            className="pointer-events-none absolute left-0 right-0 top-1/2 h-1.5 -translate-y-1/2 overflow-hidden rounded-full"
            style={{ background: COLOR_MANUAL }}
            aria-hidden
          >
            {fsdSegments?.map((seg, i) => {
              const left = (seg.start / n) * 100
              const width = ((seg.end - seg.start) / n) * 100
              return (
                <span
                  key={i}
                  className="absolute top-0 h-full"
                  style={{
                    left: `${left}%`,
                    width: `${width}%`,
                    background: COLOR_FSD,
                  }}
                />
              )
            })}
          </div>

          {/* The transparent range input retains pointer and keyboard behavior. */}
          <input
            type="range"
            min={0}
            max={max}
            value={currentIndex}
            onChange={(e) => onSliderInput(Number(e.target.value))}
            className="peer absolute inset-0 h-full w-full cursor-pointer appearance-none bg-transparent opacity-0 focus:outline-none"
            aria-label="Drive scrubber"
          />

          {/* Pointer events pass through the custom thumb to the range input. */}
          <div
            className="pointer-events-none absolute top-1/2 h-3.5 w-3.5 -translate-x-1/2 -translate-y-1/2 rounded-full bg-white shadow ring-2 ring-emerald-500/80 transition-shadow peer-focus-visible:ring-emerald-300 peer-focus-visible:ring-offset-2 peer-focus-visible:ring-offset-slate-900"
            style={{ left: `${cursorPct}%` }}
            aria-hidden
          />

          {/* Prevent the moving time label from wrapping near either edge. */}
          <div
            className="pointer-events-none absolute -bottom-5 whitespace-nowrap text-[10px] font-semibold tabular-nums text-emerald-300"
            style={{ left: `${cursorPct}%`, transform: "translateX(-50%)" }}
            aria-hidden
          >
            {currentLabel}
          </div>
        </div>

        <span className="w-16 shrink-0 text-left text-xs tabular-nums text-slate-400">
          {driveEndLabel}
        </span>

        <div className="hidden items-center gap-1 sm:flex">
          {SPEEDS.map((s) => (
            <button
              key={s}
              type="button"
              onClick={() => setPlaybackSpeed(s)}
              className={cn(
                "rounded px-1.5 py-0.5 text-[10px] font-semibold tabular-nums transition-colors",
                playbackSpeed === s
                  ? "bg-white/10 text-emerald-300"
                  : "text-slate-500 hover:text-slate-300",
              )}
            >
              {s}x
            </button>
          ))}
        </div>
      </div>
    </div>
  )
}

function formatPointTime(relMs: number, baseMs: number): string {
  if (!Number.isFinite(baseMs)) return "—"
  const t = new Date(baseMs + relMs)
  if (Number.isNaN(t.getTime())) return "—"
  return t.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" })
}
