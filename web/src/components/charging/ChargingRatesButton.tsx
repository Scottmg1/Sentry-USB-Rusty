import { useEffect, useState, type ReactNode } from "react"
import { AddIcon, CloseIcon, DeleteIcon, ExpandMoreIcon, TuneIcon } from "@/components/icons"
import {
  useChargingRates,
  type RateSchedule,
  type TagRate,
} from "@/hooks/useChargingRates"
import { cn } from "@/lib/utils"

const TIME_RE = /^\d{1,2}:\d{2}$/

// Sunday-first to match the Tessie day picker (S M T W T F S).
const DAY_LABELS = ["S", "M", "T", "W", "T", "F", "S"]
const DAY_NAMES = [
  "Sunday",
  "Monday",
  "Tuesday",
  "Wednesday",
  "Thursday",
  "Friday",
  "Saturday",
]
const MONTHS = [
  "January",
  "February",
  "March",
  "April",
  "May",
  "June",
  "July",
  "August",
  "September",
  "October",
  "November",
  "December",
]

// Draft (string-input) mirrors of the persisted types, so numeric fields
// can be empty mid-edit.
interface ScheduleDraft {
  label: string
  start: string
  end: string
  days: number[]
  startMonth: number
  endMonth: number
  rate: string
}

interface PlanDraft {
  flat: string
  schedules: ScheduleDraft[]
}

const newSchedule = (): ScheduleDraft => ({
  label: "",
  start: "22:00",
  end: "06:00",
  days: [0, 1, 2, 3, 4, 5, 6],
  startMonth: 1,
  endMonth: 12,
  rate: "",
})

// Opens a modal editor for the electricity rates used to cost charge
// sessions: a currency symbol, a flat default price-per-kWh for untagged
// charges, and a per-tag plan (a flat rate plus optional time-of-use
// schedules scoped by time, days, and months). Saving persists the prefs
// and calls `onSaved` so the page can refetch (cost is computed
// server-side from these values).
export function ChargingRatesButton({
  tags,
  onSaved,
}: {
  tags: string[]
  onSaved?: () => void
}) {
  const { rates, loading, save } = useChargingRates()
  const [open, setOpen] = useState(false)

  const [currency, setCurrency] = useState("$")
  const [defaultRate, setDefaultRate] = useState("")
  const [plans, setPlans] = useState<Record<string, PlanDraft>>({})
  const [expanded, setExpanded] = useState<Set<string>>(new Set())
  const [busy, setBusy] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)

  // Seed the draft from the loaded rates + known tags, then open. The
  // Rates button stays disabled until rates finish loading, so this
  // always runs with the saved values in hand.
  const openEditor = () => {
    setSaveError(null)
    setCurrency(rates.currency)
    setDefaultRate(rates.defaultRate != null ? String(rates.defaultRate) : "")
    const draft: Record<string, PlanDraft> = {}
    const seed = (tag: string) => {
      const plan = rates.tags[tag]
      draft[tag] = {
        flat: plan?.flat != null ? String(plan.flat) : "",
        schedules: (plan?.schedules ?? []).map((s) => ({
          label: s.label,
          start: s.start,
          end: s.end,
          // Stored [] means "every day"; show that as all-on (Tessie-style),
          // and onSave coerces all-on back to [].
          days: s.days.length === 0 ? [0, 1, 2, 3, 4, 5, 6] : [...s.days],
          startMonth: s.startMonth,
          endMonth: s.endMonth,
          rate: String(s.rate),
        })),
      }
    }
    for (const t of tags) seed(t)
    // Keep plans for tags in prefs but not in the current list (e.g. a
    // renamed tag) so saving doesn't drop them.
    for (const t of Object.keys(rates.tags)) if (!(t in draft)) seed(t)
    setPlans(draft)
    setExpanded(new Set())
    setOpen(true)
  }

  // Close on Escape while open.
  useEffect(() => {
    if (!open) return
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false)
    }
    document.addEventListener("keydown", onKey)
    return () => document.removeEventListener("keydown", onKey)
  }, [open])

  const updatePlan = (tag: string, next: PlanDraft) =>
    setPlans((prev) => ({ ...prev, [tag]: next }))

  const toggleExpanded = (tag: string) =>
    setExpanded((prev) => {
      const next = new Set(prev)
      if (next.has(tag)) next.delete(tag)
      else next.add(tag)
      return next
    })

  const onSave = async () => {
    // Reject equal start/end times before anything is persisted. The
    // half-open window math treats "12AM to 12AM" as zero-width, so a
    // schedule saved that way silently never prices a session — surface
    // the problem instead of dropping or mangling the row.
    for (const [tag, plan] of Object.entries(plans)) {
      for (const s of plan.schedules) {
        if (
          parseRate(s.rate) != null &&
          TIME_RE.test(s.start) &&
          TIME_RE.test(s.end) &&
          s.start === s.end
        ) {
          setSaveError(
            `"${tag}": schedule start and end times can't match (${s.start}). ` +
              "For an all-day rate, use the flat rate instead.",
          )
          return
        }
      }
    }
    setSaveError(null)
    setBusy(true)
    try {
      const tagsOut: Record<string, TagRate> = {}
      for (const [tag, plan] of Object.entries(plans)) {
        const flat = parseRate(plan.flat)
        const schedules: RateSchedule[] = []
        for (const s of plan.schedules) {
          const rate = parseRate(s.rate)
          if (rate == null || !TIME_RE.test(s.start) || !TIME_RE.test(s.end)) {
            continue
          }
          // All days or none selected → every day ([]).
          const days =
            s.days.length === 0 || s.days.length === 7
              ? []
              : [...s.days].sort((a, b) => a - b)
          schedules.push({
            label: s.label.trim(),
            start: s.start,
            end: s.end,
            days,
            startMonth: s.startMonth,
            endMonth: s.endMonth,
            rate,
          })
        }
        // Persist only configured plans (a flat rate or ≥1 valid schedule).
        if (flat != null || schedules.length > 0) {
          tagsOut[tag] = { flat, schedules }
        }
      }
      await save({
        currency: currency.trim() || "$",
        defaultRate: parseRate(defaultRate),
        tags: tagsOut,
      })
      setOpen(false)
      onSaved?.()
    } finally {
      setBusy(false)
    }
  }

  const tagEntries = Object.keys(plans).sort((a, b) => a.localeCompare(b))

  return (
    <>
      <button
        type="button"
        disabled={loading}
        onClick={() => (open ? setOpen(false) : openEditor())}
        className="inline-flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/[0.03] px-2.5 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/[0.06] disabled:opacity-50"
      >
        <TuneIcon className="h-3.5 w-3.5" />
        Rates
      </button>

      {open && (
        <div
          className="fixed inset-0 z-[2000] flex items-stretch justify-center bg-black/60 backdrop-blur-sm sm:items-center sm:p-4"
          onClick={() => setOpen(false)}
        >
          <div
            className="flex h-[100dvh] w-full flex-col border-white/10 bg-slate-950 shadow-2xl sm:h-auto sm:max-h-[90vh] sm:max-w-2xl sm:rounded-2xl sm:border"
            onClick={(e) => e.stopPropagation()}
          >
            {/* Header */}
            <div className="flex items-center justify-between border-b border-white/10 px-4 py-3">
              <h2 className="text-sm font-semibold uppercase tracking-wider text-slate-300">
                Electricity rates
              </h2>
              <button
                type="button"
                aria-label="Close"
                onClick={() => setOpen(false)}
                className="rounded-md p-1 text-slate-400 hover:bg-white/5 hover:text-slate-200"
              >
                <CloseIcon className="h-4 w-4" />
              </button>
            </div>

            {/* Body */}
            <div className="flex-1 space-y-4 overflow-y-auto px-4 py-4">
              <div className="flex gap-3">
                <Labeled label="Symbol" className="w-20">
                  <input
                    type="text"
                    value={currency}
                    maxLength={3}
                    onChange={(e) => setCurrency(e.target.value)}
                    className={inputClass}
                  />
                </Labeled>
                <Labeled label="Default rate / kWh" className="flex-1">
                  <input
                    type="number"
                    inputMode="decimal"
                    step="0.01"
                    min="0"
                    placeholder="e.g. 0.30"
                    value={defaultRate}
                    onChange={(e) => setDefaultRate(e.target.value)}
                    className={inputClass}
                  />
                </Labeled>
              </div>
              <p className="-mt-2 text-xs text-slate-500">
                Used for untagged charges, and whenever a tag's schedules
                don't cover the charging time.
              </p>

              <div className="border-t border-white/[0.06] pt-3">
                <div className="mb-2 text-[10px] font-semibold uppercase tracking-wide text-slate-400">
                  Per-tag rates
                </div>
                {tagEntries.length === 0 ? (
                  <p className="rounded-md bg-white/[0.02] px-3 py-2 text-xs text-slate-500">
                    Tag a charge to set a rate or time-of-use schedule for it.
                    Tagged charges use their tag's plan; the rest use the
                    default rate.
                  </p>
                ) : (
                  <div className="flex flex-col gap-2">
                    {tagEntries.map((tag) => (
                      <TagPlanEditor
                        key={tag}
                        tag={tag}
                        plan={plans[tag]}
                        expanded={expanded.has(tag)}
                        onToggle={() => toggleExpanded(tag)}
                        onChange={(next) => updatePlan(tag, next)}
                      />
                    ))}
                  </div>
                )}
              </div>
            </div>

            {/* Footer */}
            <div className="flex items-center justify-end gap-2 border-t border-white/10 px-4 py-3">
              {saveError && (
                <span className="mr-auto text-xs text-rose-300">{saveError}</span>
              )}
              <button
                type="button"
                disabled={busy}
                onClick={() => setOpen(false)}
                className="rounded-lg border border-white/10 bg-white/[0.03] px-4 py-1.5 text-xs font-medium text-slate-300 hover:bg-white/[0.06] disabled:opacity-50"
              >
                Cancel
              </button>
              <button
                type="button"
                disabled={busy}
                onClick={onSave}
                className="rounded-lg bg-emerald-500/90 px-4 py-1.5 text-xs font-medium text-slate-950 transition-colors hover:bg-emerald-400 disabled:opacity-50"
              >
                Save rates
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  )
}

function TagPlanEditor({
  tag,
  plan,
  expanded,
  onToggle,
  onChange,
}: {
  tag: string
  plan: PlanDraft
  expanded: boolean
  onToggle: () => void
  onChange: (next: PlanDraft) => void
}) {
  const flatNum = parseRate(plan.flat)
  const nSched = plan.schedules.length
  const summary =
    flatNum == null && nSched === 0
      ? "Not set"
      : [
          flatNum != null ? `${flatNum} /kWh` : null,
          nSched ? `${nSched} schedule${nSched > 1 ? "s" : ""}` : null,
        ]
          .filter(Boolean)
          .join(" · ")

  const addSchedule = () =>
    onChange({ ...plan, schedules: [...plan.schedules, newSchedule()] })
  const updateSchedule = (i: number, s: ScheduleDraft) =>
    onChange({
      ...plan,
      schedules: plan.schedules.map((x, idx) => (idx === i ? s : x)),
    })
  const removeSchedule = (i: number) =>
    onChange({ ...plan, schedules: plan.schedules.filter((_, idx) => idx !== i) })

  return (
    <div className="rounded-lg border border-white/10 bg-white/[0.02]">
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full items-center gap-2 px-3 py-2 text-left"
      >
        <ExpandMoreIcon
          className={cn(
            "h-4 w-4 shrink-0 text-slate-500 transition-transform",
            expanded && "rotate-180",
          )}
        />
        <span className="min-w-0 flex-1 truncate text-sm font-medium text-slate-200">
          {tag}
        </span>
        <span className="shrink-0 text-xs text-slate-500 tabular-nums">
          {summary}
        </span>
      </button>

      {expanded && (
        <div className="space-y-3 border-t border-white/[0.06] px-3 py-3">
          <Labeled label="Flat rate / kWh" className="max-w-[12rem]">
            <input
              type="number"
              inputMode="decimal"
              step="0.01"
              min="0"
              placeholder="default"
              value={plan.flat}
              onChange={(e) => onChange({ ...plan, flat: e.target.value })}
              className={inputClass}
            />
          </Labeled>

          <div>
            <div className="mb-1.5 text-[10px] font-semibold uppercase tracking-wide text-slate-500">
              Time-of-use schedules
            </div>
            {nSched === 0 && (
              <p className="rounded-md bg-white/[0.02] px-2.5 py-1.5 text-xs text-slate-500">
                Add a schedule like off-peak 22:00–06:00. Charging is priced
                by the first matching schedule; other times use the flat
                rate above, then the default.
              </p>
            )}
            <div className="flex flex-col gap-2">
              {plan.schedules.map((s, i) => (
                <ScheduleCard
                  key={i}
                  schedule={s}
                  onChange={(next) => updateSchedule(i, next)}
                  onRemove={() => removeSchedule(i)}
                />
              ))}
            </div>
            <button
              type="button"
              onClick={addSchedule}
              className="mt-2 inline-flex items-center justify-center gap-1 rounded-md border border-white/10 bg-white/[0.03] px-2.5 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/[0.06]"
            >
              <AddIcon className="h-3.5 w-3.5" />
              Add rate schedule
            </button>
          </div>
        </div>
      )}
    </div>
  )
}

function ScheduleCard({
  schedule,
  onChange,
  onRemove,
}: {
  schedule: ScheduleDraft
  onChange: (next: ScheduleDraft) => void
  onRemove: () => void
}) {
  return (
    <div className="space-y-3 rounded-md border border-white/10 bg-slate-950/40 p-3">
      <div className="flex items-center gap-2">
        <input
          type="text"
          value={schedule.label}
          onChange={(e) => onChange({ ...schedule, label: e.target.value })}
          placeholder="Label (e.g. Off-peak)"
          className="min-w-0 flex-1 rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 placeholder:text-slate-600 focus:border-emerald-400/40 focus:outline-none"
        />
        <button
          type="button"
          aria-label="Remove schedule"
          onClick={onRemove}
          className="shrink-0 rounded-md p-1 text-slate-500 hover:bg-white/5 hover:text-rose-300"
        >
          <DeleteIcon className="h-4 w-4" />
        </button>
      </div>

      <div className="flex flex-wrap items-end gap-3">
        <Labeled label="From">
          <input
            type="time"
            value={schedule.start}
            onChange={(e) => onChange({ ...schedule, start: e.target.value })}
            className={timeClass}
          />
        </Labeled>
        <Labeled label="To">
          <input
            type="time"
            value={schedule.end}
            onChange={(e) => onChange({ ...schedule, end: e.target.value })}
            className={timeClass}
          />
        </Labeled>
        <Labeled label="Rate / kWh" className="w-24">
          <input
            type="number"
            inputMode="decimal"
            step="0.01"
            min="0"
            placeholder="0.00"
            value={schedule.rate}
            onChange={(e) => onChange({ ...schedule, rate: e.target.value })}
            className={inputClass}
          />
        </Labeled>
      </div>

      <div>
        <div className="mb-1.5 text-[10px] uppercase tracking-wide text-slate-500">
          Days
        </div>
        <DayToggles
          days={schedule.days}
          onChange={(days) => onChange({ ...schedule, days })}
        />
      </div>

      <div className="flex flex-wrap items-end gap-3">
        <Labeled label="Starting in">
          <MonthSelect
            value={schedule.startMonth}
            onChange={(m) => onChange({ ...schedule, startMonth: m })}
          />
        </Labeled>
        <Labeled label="Through">
          <MonthSelect
            value={schedule.endMonth}
            onChange={(m) => onChange({ ...schedule, endMonth: m })}
          />
        </Labeled>
      </div>
    </div>
  )
}

function DayToggles({
  days,
  onChange,
}: {
  days: number[]
  onChange: (days: number[]) => void
}) {
  const toggle = (d: number) =>
    onChange(
      days.includes(d)
        ? days.filter((x) => x !== d)
        : [...days, d].sort((a, b) => a - b),
    )
  return (
    <div className="flex items-center gap-1.5">
      {DAY_LABELS.map((label, d) => {
        const on = days.includes(d)
        return (
          <button
            key={d}
            type="button"
            onClick={() => toggle(d)}
            aria-pressed={on}
            aria-label={DAY_NAMES[d]}
            className={cn(
              "flex h-8 w-8 items-center justify-center rounded-full text-xs font-semibold transition-colors",
              on
                ? "bg-emerald-500 text-slate-950"
                : "bg-white/[0.04] text-slate-400 hover:bg-white/[0.08]",
            )}
          >
            {label}
          </button>
        )
      })}
    </div>
  )
}

function MonthSelect({
  value,
  onChange,
}: {
  value: number
  onChange: (m: number) => void
}) {
  return (
    <select
      value={value}
      onChange={(e) => onChange(Number(e.target.value))}
      className="rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 [color-scheme:dark] focus:border-emerald-400/40 focus:outline-none"
    >
      {MONTHS.map((m, i) => (
        <option key={i} value={i + 1}>
          {m}
        </option>
      ))}
    </select>
  )
}

function Labeled({
  label,
  className,
  children,
}: {
  label: string
  className?: string
  children: ReactNode
}) {
  return (
    <label className={cn("flex flex-col gap-1", className)}>
      <span className="text-[10px] uppercase tracking-wide text-slate-500">
        {label}
      </span>
      {children}
    </label>
  )
}

const inputClass =
  "w-full rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 placeholder:text-slate-600 focus:border-emerald-400/40 focus:outline-none"

const timeClass =
  "rounded-md border border-white/10 bg-slate-950/60 px-2 py-1 text-sm text-slate-100 [color-scheme:dark] focus:border-emerald-400/40 focus:outline-none"

// Parse a rate input into a finite, non-negative number, or null for
// blank/invalid (so the field clears the stored rate).
function parseRate(s: string): number | null {
  const t = s.trim()
  if (t === "") return null
  const n = Number(t)
  return Number.isFinite(n) && n >= 0 ? n : null
}
