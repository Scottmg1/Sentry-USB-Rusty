import type {
  ChargeSessionDetail,
  ChargeSessionSummary,
  CurrentCharge,
} from "@/types/charging"

export async function fetchChargeSessions(): Promise<ChargeSessionSummary[]> {
  const res = await fetch("/api/charging")
  if (!res.ok) throw new Error(`charging: ${res.status}`)
  const data = await res.json()
  return Array.isArray(data.sessions) ? data.sessions : []
}

export async function fetchChargeSession(
  id: string | number,
): Promise<ChargeSessionDetail> {
  const res = await fetch(`/api/charging/${id}`)
  if (!res.ok) throw new Error(`charge session ${id}: ${res.status}`)
  return res.json()
}

/// Live "is the car charging right now" for the dashboard banner.
export async function fetchCurrentCharge(): Promise<CurrentCharge> {
  const res = await fetch("/api/charging/current")
  if (!res.ok) throw new Error(`charging/current: ${res.status}`)
  return res.json()
}

export type ChargingActionRequest =
  | { action: "start" | "stop" }
  | { action: "setAmps" | "setLimit"; value: number }

export async function sendChargingAction(
  request: ChargingActionRequest,
): Promise<void> {
  const res = await fetch("/api/charging/action", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(request),
  })
  if (!res.ok) {
    const body = await res.json().catch(() => ({}))
    throw new Error(body.error || `charging action: ${res.status}`)
  }
}

/// Every charge tag in use (for the filter + per-tag rate editor).
export async function fetchChargeTags(): Promise<string[]> {
  const res = await fetch("/api/charging/tags")
  if (!res.ok) throw new Error(`charging tags: ${res.status}`)
  const data = await res.json()
  return Array.isArray(data) ? data : []
}

/// Replace the tags for a charge session (id == its start timestamp).
export async function setChargeTags(
  id: string | number,
  tags: string[],
): Promise<void> {
  const res = await fetch(`/api/charging/${id}/tags`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ tags }),
  })
  if (!res.ok) throw new Error(`set charge tags ${id}: ${res.status}`)
}

/// Set or clear the manual per-charge cost override (id == its start
/// timestamp). Pass `null` to clear and revert to the rate-derived cost.
/// The amount is stored in the user's configured currency server-side.
export async function setChargeCost(
  id: string | number,
  amount: number | null,
): Promise<void> {
  const res = await fetch(`/api/charging/${id}/cost`, {
    method: "PUT",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ amount }),
  })
  if (!res.ok) throw new Error(`set charge cost ${id}: ${res.status}`)
}

export interface BulkDeleteChargesResult {
  deleted: number
  sessions: number
}

/// Delete charge sessions by id (their start timestamps). The backend
/// removes each session's charge-bearing telemetry samples + tags.
export async function bulkDeleteCharges(
  ids: Array<string | number>,
): Promise<BulkDeleteChargesResult> {
  const res = await fetch("/api/charging/bulk-delete", {
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
