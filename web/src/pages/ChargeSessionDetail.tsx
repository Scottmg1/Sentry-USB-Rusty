import { useCallback, useEffect, useMemo, useState } from "react"
import { Link, useParams } from "react-router-dom"
import {
  ArrowLeft,
  BatteryCharging,
  Check,
  DollarSign,
  Gauge,
  Home,
  Leaf,
  Loader2,
  MapPin,
  Pencil,
  Plug,
  X,
  Zap,
} from "lucide-react"
import {
  fetchChargeSession,
  fetchCurrentCharge,
  setChargeCost,
  setChargeTags,
} from "@/api/charging"
import type { ChargeSessionDetail, CurrentCharge } from "@/types/charging"
import { SectionHeading, StatTile } from "@/components/drives/StatTile"
import { TagPopover } from "@/components/drives/TagPopover"
import ChargePowerChart from "@/components/charging/ChargePowerChart"
import ChargingLineChart, { type ChargeSeries } from "@/components/charging/ChargingLineChart"
import { MiniPinMap } from "@/components/charging/MiniPinMap"
import {
  fmtDuration,
  fmtEnergy,
  fmtMoney,
  fmtPercent,
  fmtPower,
  fmtRangeUnit,
  fmtSoc,
} from "@/lib/charge-format"
import { useDistanceUnit } from "@/hooks/useDistanceUnit"

// Static chart series — hoisted to module scope so their references stay
// stable across the page's live-poll re-renders. With the charts wrapped
// in React.memo, a stable series array lets recharts skip its (expensive)
// re-layout when only `nowMs`/`current` changed.
const RANGE_SERIES: ChargeSeries[] = [{ key: "rangeMi", name: "Range", color: "#a78bfa" }]
const CURRENT_SERIES: ChargeSeries[] = [{ key: "currentA", name: "Current", color: "#fbbf24" }]
const VOLTAGE_SERIES: ChargeSeries[] = [{ key: "voltageV", name: "Voltage", color: "#22d3ee" }]
const VOLTAGE_Y_DOMAIN: [number | string, number | string] = [0, "dataMax + 10"]

export default function ChargeSessionDetailPage() {
  const { id } = useParams<{ id: string }>()
  const [session, setSession] = useState<ChargeSessionDetail | null>(null)
  const [loading, setLoading] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const metric = useDistanceUnit()
  const [current, setCurrent] = useState<CurrentCharge | null>(null)
  // `nowMs` ticks from the poll below so the in-progress check doesn't
  // read Date.now() during render (React 19 rule).
  const [nowMs, setNowMs] = useState(() => Date.now())

  useEffect(() => {
    if (!id) return
    let cancelled = false
    setLoading(true)
    setError(null)
    fetchChargeSession(id)
      .then((s) => {
        if (!cancelled) setSession(s)
      })
      .catch((e) => {
        if (!cancelled) setError(e instanceof Error ? e.message : String(e))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [id])

  const onTagsChange = useCallback(
    async (next: string[]) => {
      if (!id) return
      // Optimistic, then refetch so cost reflects the new tags' rate.
      setSession((prev) => (prev ? { ...prev, tags: next } : prev))
      try {
        await setChargeTags(id, next)
      } finally {
        try {
          setSession(await fetchChargeSession(id))
        } catch {
          /* keep optimistic tags if the refetch fails */
        }
      }
    },
    [id],
  )

  // Save (or clear, when `amount` is null) the manual per-charge cost, then
  // refetch so the override/rate state comes straight from the backend.
  const onCostSave = useCallback(
    async (amount: number | null) => {
      if (!id) return
      await setChargeCost(id, amount)
      setSession(await fetchChargeSession(id))
    },
    [id],
  )

  // Live status: refresh the in-progress charge + drive its projection.
  useEffect(() => {
    if (!id) return
    let cancelled = false
    const tick = async () => {
      const c = await fetchCurrentCharge().catch(() => null)
      if (cancelled) return
      setNowMs(Date.now())
      if (c) setCurrent(c)
      if (c?.charging) {
        try {
          const s = await fetchChargeSession(id)
          if (!cancelled) setSession(s)
        } catch {
          /* keep the current view on a transient error */
        }
      }
    }
    tick()
    const interval = setInterval(() => {
      if (!document.hidden) tick()
    }, 30_000)
    return () => {
      cancelled = true
      clearInterval(interval)
    }
  }, [id])

  const rangeAdded =
    session?.startRangeMi != null && session?.endRangeMi != null
      ? session.endRangeMi - session.startRangeMi
      : null

  const hasCurrent = (session?.points ?? []).some((p) => p.currentA != null)
  const hasVoltage = (session?.points ?? []).some((p) => p.voltageV != null)
  const hasRange = (session?.points ?? []).some((p) => p.rangeMi != null)

  // The Range chart plots rangeMi directly; convert the series to km for
  // metric so the axis and tooltip match the unit.
  const rangePoints = useMemo(
    () =>
      metric
        ? (session?.points ?? []).map((p) => ({
            ...p,
            rangeMi: p.rangeMi == null ? null : p.rangeMi * 1.609344,
          }))
        : (session?.points ?? []),
    [session?.points, metric],
  )

  const lastPoint = session?.points[session.points.length - 1]
  // In progress when the live status says charging and this session's
  // last sample is recent — so viewing an old session during a different
  // ongoing charge doesn't wrongly show a projection.
  const inProgress =
    !!current?.charging &&
    session != null &&
    nowMs - session.endMs < 15 * 60 * 1000
  const projMins = inProgress ? (current?.minutesToFull ?? null) : null
  // Dashed projection from the last sample to the charge limit over the
  // reported time-to-full. We emit a point roughly once a minute (capped
  // at 60 so a long, slow charge doesn't generate hundreds) rather than
  // only the endpoint, so the dashed line can be hovered anywhere to read
  // the projected battery % at that time — not just "now" and "full by".
  // SoC is interpolated linearly: the car reports only total time-to-full,
  // so a straight ramp up to the limit is the honest model.
  const projection = useMemo(() => {
    if (!inProgress || projMins == null || projMins <= 0 || !lastPoint) {
      return undefined
    }
    const limit = current?.limitSoc ?? 100
    const lastSoc = lastPoint.soc ?? current?.soc ?? null
    if (lastSoc == null || limit <= lastSoc) return undefined
    const startTs = lastPoint.ts
    const endTs = startTs + projMins * 60_000
    const segments = Math.min(60, Math.max(1, Math.round(projMins)))
    const pts: { ts: number; soc: number }[] = []
    for (let i = 1; i <= segments; i++) {
      const frac = i / segments
      pts.push({
        ts: Math.round(startTs + (endTs - startTs) * frac),
        soc: lastSoc + (limit - lastSoc) * frac,
      })
    }
    return pts
  }, [inProgress, projMins, lastPoint, current])
  const remaining =
    projMins != null && projMins > 0
      ? projMins >= 60
        ? `${Math.floor(projMins / 60)}h ${projMins % 60}m`
        : `${projMins}m`
      : null
  const etaLabel =
    projMins != null && projMins > 0
      ? formatEndLabel(nowMs, nowMs + projMins * 60_000)
      : null

  return (
    <div className="mx-auto w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
      <Link
        to="/charging"
        className="mb-4 inline-flex items-center gap-1.5 text-sm text-slate-400 hover:text-slate-200"
      >
        <ArrowLeft className="h-4 w-4" />
        Charging
      </Link>

      {loading && (
        <div className="flex items-center justify-center gap-2 rounded-2xl border border-white/[0.06] bg-white/[0.025] p-10 text-sm text-slate-400">
          <Loader2 className="h-4 w-4 animate-spin" />
          Loading session…
        </div>
      )}
      {error && !loading && (
        <div className="rounded-2xl border border-rose-400/30 bg-rose-500/5 p-6 text-sm text-rose-200">
          Failed to load session: {error}
        </div>
      )}

      {!loading && !error && session && (
        <div className="space-y-6">
          <div className="flex items-start justify-between gap-3">
            <div>
              <h1 className="text-2xl font-semibold text-slate-100">
                {formatDateTime(session.startMs)}
              </h1>
              <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-sm text-slate-500">
                {session.fastCharging && (
                  <span
                    title="DC fast charging (Supercharger / CCS) — peak power over 22 kW"
                    className="inline-flex items-center gap-1 rounded-full bg-amber-500/15 px-2 py-0.5 text-xs font-medium text-amber-300 ring-1 ring-inset ring-amber-400/20"
                  >
                    <Zap className="h-3 w-3 fill-amber-300" />
                    Fast charging
                  </span>
                )}
                <span>&rarr; {formatEndLabel(session.startMs, session.endMs)}</span>
                <span>{fmtDuration(session.durationSecs)}</span>
                {session.location && (
                  <span className="inline-flex items-center gap-1">
                    <MapPin className="h-3.5 w-3.5" />
                    {session.location}
                  </span>
                )}
              </div>
            </div>
            <div className="flex items-center gap-1.5">
              {session.atHome && (
                <span
                  title="Charged at home (automatic)"
                  className="inline-flex items-center gap-1 rounded-md border border-emerald-400/30 bg-emerald-400/10 px-2 py-1 text-xs font-medium text-emerald-200"
                >
                  <Home className="h-3 w-3" />
                  Home
                </span>
              )}
              <TagPopover tags={session.tags} onChange={onTagsChange} />
            </div>
          </div>

          {inProgress && (
            <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 rounded-xl border border-emerald-500/20 bg-emerald-500/10 px-4 py-2.5 text-sm text-emerald-200 tabular-nums">
              <BatteryCharging className="h-4 w-4 shrink-0 animate-pulse" />
              <span className="font-medium text-emerald-100">Charging</span>
              {remaining && (
                <>
                  <span className="text-emerald-400/50">·</span>
                  <span>{remaining} left</span>
                </>
              )}
              {etaLabel && (
                <>
                  <span className="text-emerald-400/50">·</span>
                  <span>full by {etaLabel}</span>
                </>
              )}
              {current?.powerKw != null && (
                <>
                  <span className="text-emerald-400/50">·</span>
                  <span>{current.powerKw} kW</span>
                </>
              )}
            </div>
          )}

          {session.locationLat != null && session.locationLon != null && (
            <MiniPinMap
              lat={session.locationLat}
              lon={session.locationLon}
              zoom={16}
              className="h-44 w-full"
            />
          )}

          <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
            <SectionHeading>Session</SectionHeading>
            <div className="space-y-5">
              <StatGroup label="Energy">
                <StatTile
                  label="Energy added"
                  value={fmtEnergy(session.energyAddedKwh)}
                  icon={<BatteryCharging className="h-4 w-4" />}
                  info="Energy added to the battery this session (reported by the car)."
                />
                <StatTile
                  label="Energy used"
                  value={fmtEnergy(session.energyUsedKwh)}
                  icon={<Zap className="h-4 w-4" />}
                  info="Energy drawn from the charger (wall-side), estimated by integrating charging power. Higher than energy added — the difference is charging loss."
                />
                <StatTile
                  label="Efficiency"
                  value={fmtPercent(session.efficiencyPct)}
                  icon={<Leaf className="h-4 w-4" />}
                  info="Energy added to the battery divided by energy drawn from the charger."
                />
                <StatTile
                  label="Cost"
                  value={
                    session.fastCharging ? (
                      <EditableCost
                        cost={session.cost}
                        currency={session.currency}
                        overridden={session.costOverridden}
                        onSave={onCostSave}
                      />
                    ) : session.cost != null ? (
                      fmtMoney(session.cost, session.currency)
                    ) : (
                      "—"
                    )
                  }
                  icon={<DollarSign className="h-4 w-4" />}
                  info={
                    session.fastCharging
                      ? session.costOverridden
                        ? "Manually set for this charge. Click to edit or clear."
                        : "Click to enter what this fast charge cost (e.g. from the Supercharger receipt). Overrides any rate."
                      : session.rate != null
                        ? `Charged on energy used at ${fmtMoney(session.rate, session.currency)}/kWh. Set rates from the Charging page.`
                        : "Set an electricity rate from the Charging page to see cost."
                  }
                />
              </StatGroup>

              <StatGroup label="Battery & range">
                <StatTile
                  label="Battery"
                  value={
                    session.startSoc != null && session.endSoc != null
                      ? `${fmtSoc(session.startSoc)} → ${fmtSoc(session.endSoc)}`
                      : fmtSoc(session.endSoc)
                  }
                  icon={<BatteryCharging className="h-4 w-4" />}
                  info="State of charge at start and end."
                />
                <StatTile
                  label="Range added"
                  value={rangeAdded != null ? fmtRangeUnit(rangeAdded, metric) : "—"}
                  icon={<Gauge className="h-4 w-4" />}
                  info="Rated range gained this session."
                />
                <StatTile
                  label="Charge limit"
                  value={fmtSoc(session.chargeLimitSoc)}
                  icon={<BatteryCharging className="h-4 w-4" />}
                  info="Target state of charge for this session."
                />
              </StatGroup>

              <StatGroup label="Power">
                <StatTile
                  label="Peak power"
                  value={fmtPower(session.peakPowerKw)}
                  icon={<Zap className="h-4 w-4" />}
                  info={
                    session.avgPowerKw != null
                      ? `Highest charging power seen this session. Average ${Math.round(session.avgPowerKw)} kW.`
                      : "Highest charging power seen this session."
                  }
                />
                <StatTile
                  label="Peak current"
                  value={session.peakCurrentA != null ? `${session.peakCurrentA} A` : "—"}
                  icon={<Plug className="h-4 w-4" />}
                />
                <StatTile
                  label="Voltage"
                  value={session.peakVoltageV != null ? `${session.peakVoltageV} V` : "—"}
                  icon={<Zap className="h-4 w-4" />}
                />
              </StatGroup>
            </div>
          </div>

          {session.points.length > 1 && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Power &amp; battery</SectionHeading>
              <ChargePowerChart points={session.points} projection={projection} />
            </div>
          )}

          {session.points.length > 1 && hasRange && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Range</SectionHeading>
              <ChargingLineChart
                points={rangePoints}
                series={RANGE_SERIES}
                unit={metric ? " km" : " mi"}
              />
            </div>
          )}

          {session.points.length > 1 && hasCurrent && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Amperage</SectionHeading>
              {session.fastCharging && (
                <p className="-mt-2 mb-3 text-xs text-slate-500">
                  Estimated from power ÷ voltage — Tesla reports 0 A on DC
                  fast charging (the onboard AC charger is bypassed).
                </p>
              )}
              <ChargingLineChart
                points={session.points}
                series={CURRENT_SERIES}
                unit=" A"
              />
            </div>
          )}

          {session.points.length > 1 && hasVoltage && (
            <div className="rounded-2xl border border-white/10 bg-slate-900/40 p-5">
              <SectionHeading>Voltage</SectionHeading>
              <ChargingLineChart
                points={session.points}
                series={VOLTAGE_SERIES}
                unit=" V"
                yDomain={VOLTAGE_Y_DOMAIN}
              />
            </div>
          )}
        </div>
      )}
    </div>
  )
}

function formatDateTime(ms: number): string {
  const d = new Date(ms)
  return d.toLocaleString([], {
    weekday: "short",
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
  })
}

// End of the session. Time-only when it ended the same day as it started
// (the common case); otherwise the full date too.
function formatEndLabel(startMs: number, endMs: number): string {
  const start = new Date(startMs)
  const end = new Date(endMs)
  const sameDay = start.toDateString() === end.toDateString()
  return end.toLocaleString(
    [],
    sameDay
      ? { hour: "numeric", minute: "2-digit" }
      : {
          weekday: "short",
          month: "short",
          day: "numeric",
          hour: "numeric",
          minute: "2-digit",
        },
  )
}

// Inline editor for the manual per-charge cost, rendered inside the Cost
// StatTile's value slot on fast-charging sessions. Click to type an amount
// (e.g. off the Supercharger receipt); saving overrides the rate-derived
// cost, Clear reverts to it. Empty/invalid input clears the override.
function EditableCost({
  cost,
  currency,
  overridden,
  onSave,
}: {
  cost: number | null
  currency: string
  overridden: boolean
  onSave: (amount: number | null) => Promise<void>
}) {
  const [editing, setEditing] = useState(false)
  const [draft, setDraft] = useState("")
  const [saving, setSaving] = useState(false)

  const begin = () => {
    setDraft(cost != null ? String(cost) : "")
    setEditing(true)
  }
  const commit = async (amount: number | null) => {
    setSaving(true)
    try {
      await onSave(amount)
      setEditing(false)
    } finally {
      setSaving(false)
    }
  }
  const save = () => {
    const n = parseFloat(draft)
    void commit(draft.trim() !== "" && Number.isFinite(n) && n >= 0 ? n : null)
  }

  if (!editing) {
    return (
      <button
        type="button"
        onClick={begin}
        className="group/cost inline-flex items-center gap-1.5 text-lg font-semibold text-slate-100 tabular-nums hover:text-white"
      >
        {cost != null ? (
          fmtMoney(cost, currency)
        ) : (
          <span className="text-base font-medium text-slate-500">Set cost</span>
        )}
        <Pencil className="h-3.5 w-3.5 text-slate-500 opacity-0 transition-opacity group-hover/cost:opacity-100" />
      </button>
    )
  }

  return (
    <div className="flex items-center gap-1.5">
      <div className="inline-flex items-center rounded-md border border-white/10 bg-white/[0.04] pl-2">
        <span className="text-sm text-slate-400">{currency}</span>
        <input
          autoFocus
          type="number"
          inputMode="decimal"
          step="0.01"
          min="0"
          value={draft}
          disabled={saving}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") save()
            if (e.key === "Escape") setEditing(false)
          }}
          className="w-20 bg-transparent px-1 py-1 text-lg font-semibold text-slate-100 outline-none tabular-nums [appearance:textfield] [&::-webkit-inner-spin-button]:appearance-none"
        />
      </div>
      <button
        type="button"
        aria-label="Save cost"
        disabled={saving}
        onClick={save}
        className="inline-flex h-7 w-7 items-center justify-center rounded-md bg-emerald-500/90 text-white hover:bg-emerald-400 disabled:opacity-50"
      >
        {saving ? (
          <Loader2 className="h-4 w-4 animate-spin" />
        ) : (
          <Check className="h-4 w-4" />
        )}
      </button>
      <button
        type="button"
        aria-label="Cancel"
        disabled={saving}
        onClick={() => setEditing(false)}
        className="inline-flex h-7 w-7 items-center justify-center rounded-md border border-white/10 bg-white/[0.03] text-slate-300 hover:bg-white/[0.06] disabled:opacity-50"
      >
        <X className="h-4 w-4" />
      </button>
      {overridden && (
        <button
          type="button"
          disabled={saving}
          onClick={() => void commit(null)}
          className="text-xs font-medium text-slate-500 hover:text-rose-300 disabled:opacity-50"
        >
          Clear
        </button>
      )}
    </div>
  )
}

// Labelled cluster of stat tiles, so the Session card reads as a few
// small groups instead of one dense block.
function StatGroup({
  label,
  children,
}: {
  label: string
  children: React.ReactNode
}) {
  return (
    <div>
      <div className="mb-3 text-[11px] font-semibold uppercase tracking-wider text-slate-500">
        {label}
      </div>
      <div className="grid grid-cols-2 gap-x-4 gap-y-4 sm:grid-cols-3">
        {children}
      </div>
    </div>
  )
}
