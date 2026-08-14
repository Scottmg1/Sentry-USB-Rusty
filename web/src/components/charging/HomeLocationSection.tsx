import { useEffect, useState } from "react"
import { HomeGeofencePicker } from "../settings/HomeGeofencePicker"
import { useKeepAccessory } from "../../hooks/useKeepAccessory"

/**
 * Edits the shared home geofence. Changes stay drafted because saving may
 * reclassify historical charges and invalidate their home-rate costs.
 */
export function HomeLocationSection({
  onSaved,
  onDone,
  /** Pin supplied by a charge when live BLE coordinates are unavailable. */
  seedLat,
  seedLon,
}: {
  onSaved?: () => void
  onDone?: () => void
  seedLat?: number | null
  seedLon?: number | null
}) {
  // Alias the context method so hook lint does not treat it as a hook call.
  const { values, loaded, saveError, useCurrentLocation: fetchCurrentLocation } =
    useKeepAccessory()

  // Null drafts defer to the saved geofence or charge seed.
  const [draft, setDraft] = useState<{
    lat: number | null
    lon: number | null
    radiusM: number
  } | null>(null)
  const [atRisk, setAtRisk] = useState<number | null>(null)
  const [freezeTag, setFreezeTag] = useState("Old House")
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [gpsAvailable, setGpsAvailable] = useState(false)

  // Treat unknown history conservatively so a failed probe cannot bypass confirmation.
  useEffect(() => {
    let alive = true
    fetch("/api/charging/home-sessions")
      .then((r) => (r.ok ? r.json() : null))
      .then((d) => {
        if (alive) setAtRisk(d && d.home_set ? d.count : d ? 0 : null)
      })
      .catch(() => {
        if (alive) setAtRisk(null)
      })
    return () => {
      alive = false
    }
  }, [])

  // Current location requires a Tesla BLE GPS fix.
  useEffect(() => {
    let alive = true
    fetch("/api/system/keep-accessory-gps")
      .then((r) => (r.ok ? r.json() : null))
      .then((d) => {
        if (alive) setGpsAvailable(typeof d?.lat === "number" && typeof d?.lon === "number")
      })
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [])

  const savedLat = values.homeLat
  const savedLon = values.homeLon
  // A charge seed takes precedence; otherwise use the saved geofence.
  const baseLat = seedLat ?? savedLat ?? null
  const baseLon = seedLon ?? savedLon ?? null

  const shownLat = draft ? draft.lat : baseLat
  const shownLon = draft ? draft.lon : baseLon
  const shownRadius = draft ? draft.radiusM : values.radiusM

  const moved =
    shownLat != null &&
    shownLon != null &&
    (savedLat == null ||
      savedLon == null ||
      Math.abs(shownLat - savedLat) > 1e-6 ||
      Math.abs(shownLon - savedLon) > 1e-6 ||
      shownRadius !== values.radiusM)

  const risky = moved && (atRisk === null || atRisk > 0)

  /** The server atomically preserves requested history before moving the geofence. */
  const commit = async (freezeAs: string | null) => {
    if (shownLat == null || shownLon == null) return
    setBusy(true)
    setError(null)
    try {
      const res = await fetch("/api/system/keep-accessory-config", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
          home_lat: shownLat,
          home_lon: shownLon,
          home_radius_m: shownRadius,
          ...(freezeAs ? { freeze_home_as: freezeAs } : {}),
        }),
      })
      if (!res.ok) {
        const body = await res.json().catch(() => null)
        setError(body?.error ?? "Couldn't save — the Pi rejected the change.")
        return
      }
      setDraft(null)
      setAtRisk(0)
      onSaved?.()
      onDone?.()
    } catch {
      setError("Couldn't reach the Pi.")
    } finally {
      setBusy(false)
    }
  }

  return (
    <section className="space-y-3">
      <p className="text-xs text-slate-500">
        Charges inside this area are tagged <span className="text-slate-300">Home</span> and priced
        with your Home rate. The same location is used by Keep Accessory; Away Mode shares the
        center but keeps its own radius.
      </p>

      {!loaded ? (
        <p className="text-xs text-slate-500">Loading…</p>
      ) : (
        <>
          <HomeGeofencePicker
            values={{ homeLat: shownLat, homeLon: shownLon, radiusM: shownRadius }}
            onChange={(patch) =>
              setDraft((d) => ({
                lat: patch.homeLat ?? d?.lat ?? shownLat,
                lon: patch.homeLon ?? d?.lon ?? shownLon,
                radiusM: patch.radiusM ?? d?.radiusM ?? shownRadius,
              }))
            }
            onUseCurrentLocation={
              gpsAvailable
                ? async () => {
                    const fix = await fetchCurrentLocation()
                    if (fix) {
                      setDraft((d) => ({
                        lat: fix.lat,
                        lon: fix.lon,
                        radiusM: d?.radiusM ?? shownRadius,
                      }))
                    }
                    return fix
                  }
                : undefined
            }
            saveError={saveError}
            mapHint="Charges inside this circle are tagged Home."
          />

          {moved && !risky && (
            <div className="flex flex-wrap items-center gap-2 rounded-md border border-emerald-400/20 bg-emerald-400/5 px-2.5 py-2">
              <p className="min-w-0 flex-1 text-[11px] text-emerald-200/90">
                Nothing is saved until you confirm.
              </p>
              <button
                type="button"
                disabled={busy}
                onClick={() => commit(null)}
                className="shrink-0 rounded-md bg-emerald-400/20 px-3 py-1 text-xs font-medium text-emerald-100 disabled:opacity-50"
              >
                Save home location
              </button>
            </div>
          )}

          {risky && (
            <div className="space-y-2 rounded-md border border-amber-500/25 bg-amber-500/5 p-2.5">
              <p className="text-[11px] text-amber-200">
                {atRisk === null
                  ? "Some past charges may be tagged Home at your current location."
                  : `${atRisk} past ${atRisk === 1 ? "charge is" : "charges are"} tagged Home at your current location.`}{" "}
                Any that end up outside the new area lose that tag, and with it any cost that came
                from your Home rate. Keep those under a name instead:
              </p>
              <div className="flex items-center gap-2">
                <input
                  value={freezeTag}
                  onChange={(e) => setFreezeTag(e.target.value)}
                  placeholder="Old House"
                  aria-label="Name to keep the old home charges under"
                  className="min-w-0 flex-1 rounded-md border border-white/10 bg-black/30 px-2 py-1 text-xs text-slate-200"
                />
                <button
                  type="button"
                  disabled={busy || !freezeTag.trim()}
                  onClick={() => commit(freezeTag.trim())}
                  className="shrink-0 rounded-md bg-emerald-400/20 px-3 py-1 text-xs font-medium text-emerald-100 disabled:opacity-50"
                >
                  Keep &amp; move
                </button>
              </div>
              <p className="text-[10px] text-amber-200/60">
                Tags the charges that leave the area and copies your Home rate onto that name, so
                they keep their cost and stay filterable. Charges still inside it are left alone.
              </p>
              <button
                type="button"
                disabled={busy}
                onClick={() => commit(null)}
                className="text-[11px] text-slate-400 underline disabled:opacity-50"
              >
                Move without keeping them
              </button>
            </div>
          )}

          {error && <p className="text-[11px] text-rose-300">{error}</p>}

          <p className="rounded-md border border-white/[0.06] px-2.5 py-2 text-[11px] text-slate-500">
            The Home tag is worked out from this location every time the list loads — it is not
            saved per charge. Costs you typed in yourself are always kept.
          </p>
        </>
      )}
    </section>
  )
}
