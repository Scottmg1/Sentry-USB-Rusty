import { createContext, useCallback, useContext, useEffect, useMemo, useState } from "react"

interface ScrubberStateValue {
  currentIndex: number
  totalPoints: number
  playing: boolean
  playbackSpeed: number
}

interface ScrubberActionsValue {
  setIndex: (n: number) => void
  setPlaying: (b: boolean) => void
  setPlaybackSpeed: (s: number) => void
  setTotal: (n: number) => void
}

// Separate state from actions so dispatch-only consumers do not render on
// every scrubber tick.
const ScrubberStateContext = createContext<ScrubberStateValue | null>(null)
const ScrubberActionsContext = createContext<ScrubberActionsValue | null>(null)

interface ScrubberProviderProps {
  children: React.ReactNode
}

export function ScrubberProvider({ children }: ScrubberProviderProps) {
  const [currentIndex, setCurrentIndex] = useState(0)
  const [totalPoints, setTotalPoints] = useState(0)
  const [playing, setPlaying] = useState(false)
  const [playbackSpeed, setPlaybackSpeed] = useState(1)

  // Stable action references preserve the dispatch-only render boundary.

  useEffect(() => {
    if (!playing || totalPoints === 0) return
    const tickMs = Math.max(20, Math.floor(100 / playbackSpeed))
    const id = window.setInterval(() => {
      setCurrentIndex((prev) => {
        const next = prev + 1
        if (next >= totalPoints) {
          setPlaying(false)
          return totalPoints - 1
        }
        return next
      })
    }, tickMs)
    return () => window.clearInterval(id)
  }, [playing, playbackSpeed, totalPoints])

  const setIndex = useCallback((n: number) => {
    setCurrentIndex(() => {
      if (n < 0) return 0
      return n
    })
  }, [])

  const setTotal = useCallback((n: number) => {
    setTotalPoints(n)
    setCurrentIndex((prev) => (prev >= n ? Math.max(0, n - 1) : prev))
  }, [])

  const stateValue = useMemo<ScrubberStateValue>(
    () => ({ currentIndex, totalPoints, playing, playbackSpeed }),
    [currentIndex, totalPoints, playing, playbackSpeed],
  )

  const actionsValue = useMemo<ScrubberActionsValue>(
    () => ({ setIndex, setPlaying, setPlaybackSpeed, setTotal }),
    [setIndex, setTotal],
  )

  return (
    <ScrubberActionsContext.Provider value={actionsValue}>
      <ScrubberStateContext.Provider value={stateValue}>
        {children}
      </ScrubberStateContext.Provider>
    </ScrubberActionsContext.Provider>
  )
}

/** Subscribe to scrubber state; consumers render on every tick. */
export function useScrubberState(): ScrubberStateValue {
  const ctx = useContext(ScrubberStateContext)
  if (!ctx) throw new Error("useScrubberState must be used inside <ScrubberProvider>")
  return ctx
}

/** Stable action setters that do not subscribe to scrubber state. */
export function useScrubberActions(): ScrubberActionsValue {
  const ctx = useContext(ScrubberActionsContext)
  if (!ctx) throw new Error("useScrubberActions must be used inside <ScrubberProvider>")
  return ctx
}
