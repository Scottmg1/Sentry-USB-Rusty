import type { DriveDetail, DriveSummary, RouteOverview } from "@/types/drives"

export async function fetchDrives(): Promise<DriveSummary[]> {
  const res = await fetch("/api/drives")
  if (!res.ok) throw new Error(`drives: ${res.status}`)
  return res.json()
}

export async function fetchDriveDetail(id: string | number): Promise<DriveDetail> {
  const res = await fetch(`/api/drives/${id}`)
  if (!res.ok) throw new Error(`drive ${id}: ${res.status}`)
  return res.json()
}

export async function fetchRouteOverviews(maxPoints = 20): Promise<RouteOverview[]> {
  const res = await fetch(`/api/drives/routes?max_points=${maxPoints}`)
  if (!res.ok) throw new Error(`routes: ${res.status}`)
  return res.json()
}

export async function setDriveTags(id: string | number, tags: string[]): Promise<void> {
  const res = await fetch(`/api/drives/${id}/tags`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tags }),
  })
  if (!res.ok) throw new Error(`set tags ${id}: ${res.status}`)
}

export async function triggerProcessNew(): Promise<void> {
  const res = await fetch("/api/drives/process", { method: "POST" })
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body.error || `process: ${res.status}`)
  }
}

export async function triggerReprocessAll(): Promise<void> {
  const res = await fetch("/api/drives/reprocess", { method: "POST" })
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body.error || `reprocess: ${res.status}`)
  }
}

/** Targeted summon evidence re-read (backend runs it async — poll the
 *  drives list afterwards; summon pills appear once the scan finishes). */
export async function triggerSummonCheck(): Promise<void> {
  const res = await fetch("/api/drives/check-summon", { method: "POST" })
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body.error || `check-summon: ${res.status}`)
  }
}

export async function uploadDriveData(file: File): Promise<{ imported: number }> {
  const res = await fetch("/api/drives/data/upload", {
    method: "POST",
    body: file,
  })
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body.error || `upload: ${res.status}`)
  }
  return res.json()
}

export async function deleteAllDrives(): Promise<void> {
  const res = await fetch("/api/drives/data", { method: "DELETE" })
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body.error || `delete: ${res.status}`)
  }
}

export interface BulkDeleteResult {
  /** Number of underlying clip rows removed from the `routes` table. */
  deleted: number
  /** Number of drives that were resolved + deleted (excludes not_found). */
  drives: number
  /** Drive ids the backend could not resolve back to clip files. */
  not_found: string[]
}

export async function bulkDeleteDrives(
  ids: Array<string | number>,
): Promise<BulkDeleteResult> {
  const res = await fetch("/api/drives/bulk-delete", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ ids: ids.map(String) }),
  })
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body.error || `bulk-delete: ${res.status}`)
  }
  return res.json()
}

