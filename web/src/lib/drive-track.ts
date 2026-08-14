// Produces an index-aligned GPS track with a strictly increasing time axis.

// A single GPS sample: [lat, lng, relativeMs, speedMps].
export type TrackPoint = [number, number, number, number]

// Keep this jump threshold aligned with DriveMap's GAP_M. Persistent jumps are
// dropouts; a point that immediately snaps back is a bad fix.
const MAX_STEP_M = 300

function haversineM(
  lat1: number,
  lon1: number,
  lat2: number,
  lon2: number,
): number {
  const R = 6371000
  const r = Math.PI / 180
  const dLa = (lat2 - lat1) * r
  const dLo = (lon2 - lon1) * r
  const a =
    Math.sin(dLa / 2) ** 2 +
    Math.cos(lat1 * r) * Math.cos(lat2 * r) * Math.sin(dLo / 2) ** 2
  return 2 * R * Math.asin(Math.sqrt(a))
}

export interface NormalisedTrack {
  points: TrackPoint[]
  // Filter FSD states only when they are index-aligned with the input points.
  fsdStates: number[] | undefined
}

/**
 * Repair backward timestamps at overlapping clip seams without dropping real
 * positions. Isolated teleport-and-return fixes are removed, with FSD states
 * filtered in lockstep.
 */
export function monotonicTrack(
  points: TrackPoint[],
  fsdStates: number[] | undefined,
): NormalisedTrack {
  const hasFsd =
    Array.isArray(fsdStates) && fsdStates.length === points.length
  const outPoints: TrackPoint[] = []
  const outFsd: number[] = []

  let prevRawMs: number | null = null
  let prevMs: number | null = null
  let lastKept: TrackPoint | null = null

  for (let i = 0; i < points.length; i++) {
    const p = points[i]

    // A lone distant point that snaps back is a bad fix, not a dropout.
    if (lastKept && haversineM(lastKept[0], lastKept[1], p[0], p[1]) > MAX_STEP_M) {
      const next = points[i + 1]
      const snapsBack =
        !!next && haversineM(lastKept[0], lastKept[1], next[0], next[1]) <= MAX_STEP_M
      if (snapsBack) continue
      // Keep persistent jumps; DriveMap breaks the polyline at the threshold.
    }

    // Preserve positive cadence; nudge duplicate or backward seams by 1 ms.
    let ms: number
    if (prevMs === null || prevRawMs === null) {
      ms = p[2]
    } else {
      const rawDelta = p[2] - prevRawMs
      ms = prevMs + (rawDelta > 0 ? rawDelta : 1)
    }

    outPoints.push([p[0], p[1], ms, p[3]])
    if (hasFsd) outFsd.push(fsdStates![i])
    prevRawMs = p[2]
    prevMs = ms
    lastKept = p
  }

  return { points: outPoints, fsdStates: hasFsd ? outFsd : fsdStates }
}
