import { fetchChargeTags } from "./charging"

/** Distinct drive tag names. */
export async function fetchDriveTags(): Promise<string[]> {
  const res = await fetch("/api/drives/tags")
  if (!res.ok) throw new Error(`drive tags: ${res.status}`)
  const data = await res.json()
  return Array.isArray(data) ? data : []
}

/** Distinct tags currently used by drives or charge sessions. */
export async function fetchAllTagNames(): Promise<string[]> {
  const [drive, charge] = await Promise.all([
    fetchDriveTags().catch(() => [] as string[]),
    fetchChargeTags().catch(() => [] as string[]),
  ])
  return Array.from(new Set([...drive, ...charge])).sort((a, b) =>
    a.localeCompare(b),
  )
}
