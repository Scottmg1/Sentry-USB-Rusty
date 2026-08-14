import { useEffect, useState } from "react"
import { fetchDriveDetail, fetchDrives, setDriveTags } from "@/api/drives"
import type { DriveDetail, DriveSummary } from "@/types/drives"

export interface DriveDetailState {
  drive: DriveDetail | null
  loading: boolean
  error: string | null
  saveTags: (tags: string[]) => Promise<void>
  refresh: () => Promise<void>
}

// The detail response omits summary telemetry, so merge its matching list row.
function mergeTelemetry(detail: DriveDetail, summary: DriveSummary): DriveDetail {
  return {
    ...detail,
    batteryPctStart: summary.batteryPctStart,
    batteryPctEnd: summary.batteryPctEnd,
    batteryPctUsed: summary.batteryPctUsed,
    interiorTempMinC: summary.interiorTempMinC,
    interiorTempMaxC: summary.interiorTempMaxC,
    exteriorTempAvgC: summary.exteriorTempAvgC,
    hvacRuntimeS: summary.hvacRuntimeS,
    tireFlPsi: summary.tireFlPsi,
    tireFrPsi: summary.tireFrPsi,
    tireRlPsi: summary.tireRlPsi,
    tireRrPsi: summary.tireRrPsi,
    odometerMiStart: summary.odometerMiStart,
    odometerMiEnd: summary.odometerMiEnd,
    odometerMiDriven: summary.odometerMiDriven,
    startLocation: summary.startLocation,
    endLocation: summary.endLocation,
  }
}

export function useDriveDetail(id: string | undefined): DriveDetailState {
  const [drive, setDrive] = useState<DriveDetail | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [tick, setTick] = useState(0)

  useEffect(() => {
    if (!id) {
      /* eslint-disable-next-line react-hooks/set-state-in-effect */
      setDrive(null)
      setLoading(false)
      setError(null)
      return
    }
    let cancelled = false
    setLoading(true)
    setError(null)
    Promise.all([
      fetchDriveDetail(id),
      fetchDrives().catch(() => [] as DriveSummary[]),
    ])
      .then(([detail, summaries]) => {
        if (cancelled) return
        const numericId = Number(id)
        const summary = summaries.find((s) => s.id === numericId)
        setDrive(summary ? mergeTelemetry(detail, summary) : detail)
      })
      .catch((e) => {
        if (cancelled) return
        setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [id, tick])

  const saveTags = async (tags: string[]) => {
    if (!id) return
    await setDriveTags(id, tags)
    setDrive((d) => (d ? { ...d, tags } : d))
  }

  const refresh = async () => {
    setTick((t) => t + 1)
  }

  return { drive, loading, error, saveTags, refresh }
}
