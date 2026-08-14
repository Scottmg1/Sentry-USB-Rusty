import { useEffect, useRef, useImperativeHandle, forwardRef, useCallback } from "react"

export interface GodotRendererHandle {
  loadScene(modelId: string): void
  setTexture(dataUrl: string): void
  capture(distance?: number): void
}

interface GodotRendererProps {
  onReady: () => void
  onCapture: (dataUrl: string) => void
  onError: (message: string) => void
  onCarLoaded?: () => void
}

/** Manages an offscreen Godot WASM renderer for wrap preview images. */
// Restrict both postMessage directions to the cloud renderer origin so another
// document cannot read commands or forge capture results.
const GODOT_ORIGIN = "https://api.sentry-six.com"

const GodotRenderer = forwardRef<GodotRendererHandle, GodotRendererProps>(
  ({ onReady, onCapture, onError, onCarLoaded }, ref) => {
    const iframeRef = useRef<HTMLIFrameElement>(null)

    const sendToGodot = useCallback((message: Record<string, unknown>) => {
      iframeRef.current?.contentWindow?.postMessage(message, GODOT_ORIGIN)
    }, [])

    useImperativeHandle(ref, () => ({
      loadScene(modelId: string) {
        sendToGodot({ type: "load_scene", modelId })
      },
      setTexture(dataUrl: string) {
        sendToGodot({ type: "set_texture", texture: dataUrl })
      },
      capture(distance?: number) {
        const angle = { type: "set_camera_angle", horizontal: -135, vertical: 25, distance: distance ?? 7 }
        // Godot needs a second camera update after its first render settles.
        sendToGodot(angle)
        setTimeout(() => {
          sendToGodot(angle)
          setTimeout(() => {
            sendToGodot({ type: "capture" })
          }, 1000)
        }, 500)
      },
    }), [sendToGodot])

    useEffect(() => {
      const handleMessage = (e: MessageEvent) => {
        if (e.origin !== GODOT_ORIGIN) return
        if (!e.data || !e.data.type) return

        switch (e.data.type) {
          case "godot_ready":
            onReady()
            break
          case "car_loaded":
          case "scene_loaded":
            onCarLoaded?.()
            break
          case "capture_result":
            if (e.data.dataUrl) onCapture(e.data.dataUrl)
            break
          case "capture_error":
            onError(e.data.error || "Capture failed")
            break
          case "godot_error":
            onError(e.data.message || "Godot engine error")
            break
        }
      }

      window.addEventListener("message", handleMessage)
      return () => window.removeEventListener("message", handleMessage)
    }, [onReady, onCapture, onError, onCarLoaded])

    return (
      <iframe
        ref={iframeRef}
        src={`${GODOT_ORIGIN}/wraps/godot/index.html`}
        title="Wrap 3D Preview Renderer"
        style={{
          position: "absolute",
          width: "1px",
          height: "1px",
          opacity: 0,
          pointerEvents: "none",
          border: "none",
        }}
        sandbox="allow-scripts allow-same-origin"
      />
    )
  }
)

GodotRenderer.displayName = "GodotRenderer"

export default GodotRenderer
