import type { DriveSummary } from "@/types/drives"

// Must match the distance-based, SEI-only FSD formulas in drives/db.rs and
// drives/grouper.rs. Top-line totals include imported and Summon drives.

export interface DrivesFilteredStats {
  count: number
  totalDistanceMi: number
  totalDistanceKm: number
  totalDurationMs: number
  fsdEngagedMs: number
  fsdDistanceMi: number
  fsdDistanceKm: number
  fsdPercent: number
  fsdDisengagements: number
  autopilotEngagedMs: number
  autopilotPercent: number
  tessieCount: number
}

export function computeFilteredStats(
  drives: DriveSummary[],
): DrivesFilteredStats {
  let totalDistanceMi = 0
  let totalDistanceKm = 0
  let totalDurationMs = 0
  let fsdEngagedMs = 0
  let fsdDistanceMi = 0
  let fsdDistanceKm = 0
  let fsdDisengagements = 0
  let autopilotEngagedMs = 0
  let tessieCount = 0
  // FSD and Autopilot percentages use only measured SEI telemetry.
  let seiTotalDistanceKm = 0
  let seiAutopilotDistanceKm = 0

  for (const d of drives) {
    totalDistanceMi += d.distanceMi
    totalDistanceKm += d.distanceKm
    totalDurationMs += d.durationMs
    // Imported assist data is inferred rather than measured SEI telemetry.
    if (d.source && d.source !== "sei") {
      tessieCount += 1
      continue
    }
    // Summon lacks autopilot_state and would dilute the assist percentages.
    if (d.summon) {
      continue
    }
    seiTotalDistanceKm += d.distanceKm
    fsdEngagedMs += d.fsdEngagedMs
    fsdDistanceMi += d.fsdDistanceMi
    fsdDistanceKm += d.fsdDistanceKm
    fsdDisengagements += d.fsdDisengagements
    autopilotEngagedMs += d.autosteerEngagedMs + d.taccEngagedMs
    seiAutopilotDistanceKm += d.autosteerDistanceKm + d.taccDistanceKm
  }

  const fsdPercent =
    seiTotalDistanceKm > 0 ? (fsdDistanceKm / seiTotalDistanceKm) * 100 : 0
  // Autopilot here excludes FSD so both percentages remain distinct.
  const autopilotPercent =
    seiTotalDistanceKm > 0
      ? (seiAutopilotDistanceKm / seiTotalDistanceKm) * 100
      : 0

  return {
    count: drives.length,
    totalDistanceMi,
    totalDistanceKm,
    totalDurationMs,
    fsdEngagedMs,
    fsdDistanceMi,
    fsdDistanceKm,
    fsdPercent,
    fsdDisengagements,
    autopilotEngagedMs,
    autopilotPercent,
    tessieCount,
  }
}
