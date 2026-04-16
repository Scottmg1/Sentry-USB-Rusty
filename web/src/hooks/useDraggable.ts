import { useRef, useCallback, useState, useEffect } from "react"

interface Position {
  x: number
  y: number
}

interface UseDraggableOptions {
  /** Initial position anchor — which corner/edge the element starts at */
  initialAnchor?: "top-left" | "top-right" | "bottom-center" | "bottom-left" | "bottom-right"
}

/**
 * Makes an element draggable within its offset parent.
 * Returns a ref to attach to the element and the current position style.
 */
export function useDraggable(options: UseDraggableOptions = {}) {
  const { initialAnchor = "top-left" } = options
  const elRef = useRef<HTMLDivElement>(null)
  const [pos, setPos] = useState<Position | null>(null)
  const dragging = useRef(false)
  const dragStart = useRef<Position>({ x: 0, y: 0 })
  const posStart = useRef<Position>({ x: 0, y: 0 })

  // Compute initial position on first render
  useEffect(() => {
    if (pos !== null || !elRef.current) return
    const el = elRef.current
    const parent = el.offsetParent as HTMLElement
    if (!parent) return

    const pw = parent.clientWidth
    const ph = parent.clientHeight
    const ew = el.offsetWidth
    const eh = el.offsetHeight

    let x = 0
    let y = 0

    switch (initialAnchor) {
      case "top-right":
        x = pw - ew - 8
        y = 8
        break
      case "bottom-center":
        x = (pw - ew) / 2
        y = ph - eh - 32
        break
      case "bottom-left":
        x = 8
        y = ph - eh - 8
        break
      case "bottom-right":
        x = pw - ew - 8
        y = ph - eh - 8
        break
      default: // top-left
        x = 8
        y = 8
    }

    setPos({ x, y })
  }, [initialAnchor, pos])

  const clamp = useCallback((x: number, y: number): Position => {
    const el = elRef.current
    if (!el) return { x, y }
    const parent = el.offsetParent as HTMLElement
    if (!parent) return { x, y }

    const pw = parent.clientWidth
    const ph = parent.clientHeight
    const ew = el.offsetWidth
    const eh = el.offsetHeight

    return {
      x: Math.max(0, Math.min(x, pw - ew)),
      y: Math.max(0, Math.min(y, ph - eh)),
    }
  }, [])

  const onPointerDown = useCallback((e: React.PointerEvent) => {
    // Only drag from header / the element itself, not interactive children
    const target = e.target as HTMLElement
    if (target.closest("button, a, input, .leaflet-container")) return

    e.preventDefault()
    e.stopPropagation()
    dragging.current = true
    dragStart.current = { x: e.clientX, y: e.clientY }
    posStart.current = pos ?? { x: 0, y: 0 }
    ;(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId)
  }, [pos])

  const onPointerMove = useCallback((e: React.PointerEvent) => {
    if (!dragging.current) return
    e.preventDefault()
    const dx = e.clientX - dragStart.current.x
    const dy = e.clientY - dragStart.current.y
    setPos(clamp(posStart.current.x + dx, posStart.current.y + dy))
  }, [clamp])

  const onPointerUp = useCallback((e: React.PointerEvent) => {
    if (!dragging.current) return
    dragging.current = false
    ;(e.currentTarget as HTMLElement).releasePointerCapture(e.pointerId)
  }, [])

  const style: React.CSSProperties = pos
    ? { position: "absolute", left: pos.x, top: pos.y, cursor: "grab", touchAction: "none" }
    : { position: "absolute", visibility: "hidden" as const, touchAction: "none" }

  const dragProps = {
    onPointerDown,
    onPointerMove,
    onPointerUp,
    style,
  }

  return { ref: elRef, dragProps }
}
