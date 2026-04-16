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

/**
 * Hidden component that manages a Godot WASM instance in an offscreen iframe.
 * Used to render 3D preview images of Tesla wraps during the upload flow.
 *
 * Communication: React <-> iframe via postMessage <-> Godot bridge <-> Godot WASM
 */
const GodotRenderer = forwardRef<GodotRendererHandle, GodotRendererProps>(
  ({ onReady, onCapture, onError, onCarLoaded }, ref) => {
    const iframeRef = useRef<HTMLIFrameElement>(null)

    const sendToGodot = useCallback((message: Record<string, unknown>) => {
      iframeRef.current?.contentWindow?.postMessage(message, "*")
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
        // Apply camera orientation twice to ensure it settles in the correct position
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
        src="https://api.sentry-six.com/wraps/godot/index.html"
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
