import { useEffect, useRef, useState } from "react"
import { ProgressActivityIcon, SpeedIcon } from "@/components/icons"
import { Modal } from "@/components/ui/Modal"

export function SpeedTestModal({ onClose }: { onClose: () => void }) {
  const [running, setRunning] = useState(false)
  const [mbps, setMbps] = useState<string | null>(null)
  const [error, setError] = useState(false)
  const cancelRef = useRef(false)
  const readerRef = useRef<ReadableStreamDefaultReader<Uint8Array> | null>(null)

  async function runOnce() {
    const res = await fetch("/api/system/speedtest")
    if (!res.ok || !res.body) throw new Error("Speed test failed")

    const reader = res.body.getReader()
    readerRef.current = reader
    const start = Date.now()
    let totalBytes = 0
    let lastUpdate = start

    try {
      while (true) {
        const { done, value } = await reader.read()
        if (done) break
        totalBytes += value.length
        const now = Date.now()
        if (now - lastUpdate >= 250) {
          const elapsedSec = (now - start) / 1000
          if (elapsedSec > 0)
            setMbps(((totalBytes * 8) / elapsedSec / 1_000_000).toFixed(1))
          lastUpdate = now
        }
      }
    } finally {
      readerRef.current = null
    }

    const elapsed = (Date.now() - start) / 1000
    if (elapsed > 0 && totalBytes > 0) {
      setMbps(((totalBytes * 8) / elapsed / 1_000_000).toFixed(1))
    }
  }

  async function startTest() {
    setRunning(true)
    cancelRef.current = false
    setMbps(null)
    setError(false)
    while (!cancelRef.current) {
      try {
        await runOnce()
        if (cancelRef.current) break
      } catch {
        if (cancelRef.current) break
        setError(true)
        break
      }
    }
    setRunning(false)
  }

  function stopTest() {
    cancelRef.current = true
    if (readerRef.current) {
      readerRef.current.cancel().catch(() => {})
      readerRef.current = null
    }
  }

  useEffect(() => {
    void startTest()
    return () => stopTest()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  return (
    <Modal
      title={
        <span className="flex items-center gap-2">
          <SpeedIcon className="h-4 w-4 text-blue-400" />
          <span>Speed Test</span>
        </span>
      }
      onClose={() => {
        stopTest()
        onClose()
      }}
      size="sm"
      footer={
        <div className="flex justify-end">
          <button
            onClick={running ? stopTest : startTest}
            className="rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 hover:bg-blue-500/25"
          >
            {running ? "Stop" : "Run again"}
          </button>
        </div>
      }
    >
      <div className="flex flex-col items-center justify-center gap-3 py-6">
        {running && !mbps ? (
          <>
            <ProgressActivityIcon className="h-8 w-8 animate-spin text-blue-400" />
            <p className="text-xs text-slate-500">Measuring throughput…</p>
          </>
        ) : mbps ? (
          <>
            <p className="text-4xl font-bold text-blue-400">
              {mbps} <span className="text-base font-normal text-slate-500">Mbps</span>
            </p>
            <p className="text-xs text-slate-500">
              {running ? "Continuously measuring…" : "Test complete"}
            </p>
          </>
        ) : error ? (
          <p className="text-sm text-red-400">Speed test failed. Try again?</p>
        ) : (
          <p className="text-xs text-slate-500">Starting…</p>
        )}
      </div>
    </Modal>
  )
}
