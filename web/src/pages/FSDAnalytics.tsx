import { useEffect, useState } from "react"
import { useNavigate } from "react-router-dom"
import { ArrowLeft, Zap, Calendar, TrendingUp, AlertTriangle, Flame, ChevronLeft } from "lucide-react"
import { api } from "@/lib/api"
import type { FSDAnalytics as FSDAnalyticsData } from "@/lib/api"
import { cn } from "@/lib/utils"
import RadialProgress from "@/components/charts/RadialProgress"
import BarChart from "@/components/charts/BarChart"

type Period = "day" | "week" | "all"

const gradeConfig: Record<string, { color: string; bgClass: string; ringColor: string }> = {
  Great: { color: "text-emerald-400", bgClass: "border-emerald-500/20 bg-emerald-500/5", ringColor: "#34d399" },
  Good: { color: "text-blue-400", bgClass: "border-blue-500/20 bg-blue-500/5", ringColor: "#60a5fa" },
  "Needs Improvement": { color: "text-amber-400", bgClass: "border-amber-500/20 bg-amber-500/5", ringColor: "#fbbf24" },
}

export default function FSDAnalytics() {
  const navigate = useNavigate()
  const [data, setData] = useState<FSDAnalyticsData | null>(null)
  const [period, setPeriod] = useState<Period>("week")
  const [loading, setLoading] = useState(true)
  const [metric, setMetric] = useState(false)
  const [selectedMonth, setSelectedMonth] = useState<string | null>(null) // "YYYY-MM" for drill-down

  useEffect(() => {
    fetch("/api/setup/config")
      .then((r) => r.json())
      .then((cfg) => {
        const entry = cfg.DRIVE_MAP_UNIT
        if (entry) {
          const val = typeof entry === "object"
            ? (entry.active ? entry.value : null)
            : entry
          if (val !== null) setMetric(val === "km")
        }
      })
      .catch(() => {})
  }, [])

  useEffect(() => {
    setLoading(true)
    api.getFSDAnalytics(period === "all" ? "all" : period)
      .then(setData)
      .catch(() => setData(null))
      .finally(() => setLoading(false))
  }, [period])

  if (loading) {
    return (
      <div className="space-y-4 p-4 sm:p-6">
        <div className="flex items-center gap-3">
          <div className="h-8 w-8 animate-pulse rounded-lg bg-white/5" />
          <div className="h-6 w-40 animate-pulse rounded bg-white/5" />
        </div>
        <div className="h-48 animate-pulse rounded-xl bg-white/5" />
        <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
          {[...Array(4)].map((_, i) => <div key={i} className="h-24 animate-pulse rounded-xl bg-white/5" />)}
        </div>
        <div className="h-56 animate-pulse rounded-xl bg-white/5" />
      </div>
    )
  }

  if (!data) {
    return (
      <div className="flex h-full items-center justify-center p-6">
        <p className="text-slate-500">No FSD data available yet. Drive with FSD to see analytics.</p>
      </div>
    )
  }

  const grade = gradeConfig[data.fsd_grade] || gradeConfig["Needs Improvement"]

  // For "all" period: group by month, with drill-down into daily view
  const MONTH_NAMES = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]
  const daily = data.daily || []

  // Monthly buckets: { key: "YYYY-MM", days: FSDDayStats[] }
  const monthlyMap = new Map<string, typeof daily>()
  for (const day of daily) {
    if (!day.date) continue
    const key = day.date.slice(0, 7) // "YYYY-MM"
    const arr = monthlyMap.get(key)
    if (arr) arr.push(day)
    else monthlyMap.set(key, [day])
  }
  const monthKeys = Array.from(monthlyMap.keys()).sort()

  const isMonthDrillDown = period === "all" && selectedMonth !== null
  const showMonthly = period === "all" && selectedMonth === null && monthKeys.length > 1

  let barData: { label: string; value: number; color?: string; subLabel?: string }[]
  let chartTitle: string
  let onBarClick: ((i: number) => void) | undefined

  if (showMonthly) {
    // Monthly overview bars
    chartTitle = "Monthly FSD Usage"
    barData = monthKeys.map((key) => {
      const days = monthlyMap.get(key)!
      const avg = days.reduce((s, d) => s + d.fsdPercent, 0) / days.length
      const totalDisengagements = days.reduce((s, d) => s + d.disengagements, 0)
      const [y, m] = key.split("-")
      const label = `${MONTH_NAMES[parseInt(m) - 1]} '${y.slice(2)}`
      return {
        label,
        value: Math.round(avg),
        color: avg >= 90 ? "#34d399" : avg >= 60 ? "#60a5fa" : "#fbbf24",
        subLabel: totalDisengagements > 0 ? `${totalDisengagements}` : undefined,
      }
    })
    onBarClick = (i) => setSelectedMonth(monthKeys[i])
  } else if (isMonthDrillDown) {
    // Drill-down: daily bars for selected month
    const days = monthlyMap.get(selectedMonth!) || []
    const [y, m] = selectedMonth!.split("-")
    chartTitle = `${MONTH_NAMES[parseInt(m) - 1]} ${y}`
    barData = days.map((day) => {
      const [, , d] = day.date.split("-")
      return {
        label: `${day.dayName} ${parseInt(d)}`,
        value: Math.round(day.fsdPercent),
        color: day.fsdPercent >= 90 ? "#34d399" : day.fsdPercent >= 60 ? "#60a5fa" : "#fbbf24",
        subLabel: day.disengagements > 0 ? `${day.disengagements}` : undefined,
      }
    })
  } else {
    // Day / Week / All with only 1 month
    chartTitle = "Daily FSD Usage"
    barData = daily.map((day) => ({
      label: day.dayName,
      value: Math.round(day.fsdPercent),
      color: day.fsdPercent >= 90 ? "#34d399" : day.fsdPercent >= 60 ? "#60a5fa" : "#fbbf24",
      subLabel: day.disengagements > 0 ? `${day.disengagements}` : undefined,
    }))
  }

  const fsdDist = metric ? data.fsd_distance_km : data.fsd_distance_mi
  const totalDist = metric ? data.total_distance_km : data.total_distance_mi
  const distUnit = metric ? "km" : "mi"
  const distPct = totalDist > 0 ? (fsdDist / totalDist) * 100 : 0

  return (
    <div className="space-y-4 p-4 sm:p-6">
      {/* Header */}
      <div className="flex items-center justify-between">
        <div className="flex items-center gap-3">
          <button onClick={() => navigate("/drives")} className="rounded-lg border border-white/10 bg-white/5 p-2 text-slate-400 transition-colors hover:bg-white/10 hover:text-slate-200">
            <ArrowLeft className="h-4 w-4" />
          </button>
          <div>
            <h1 className="text-lg font-semibold text-slate-100">FSD Analytics</h1>
            <p className="text-xs text-slate-500">
              {period === "day" ? "Today" : period === "week" ? `${data.period_start} — Today` : "All Time"}
            </p>
          </div>
        </div>
        <div className="flex items-center gap-1 rounded-full border border-white/10 bg-white/5 p-0.5">
          {(["day", "week", "all"] as Period[]).map((p) => (
            <button
              key={p}
              onClick={() => { setPeriod(p); setSelectedMonth(null) }}
              className={cn(
                "rounded-full px-3 py-1 text-xs font-medium transition-colors",
                period === p ? "bg-white/10 text-slate-100" : "text-slate-500 hover:text-slate-300"
              )}
            >
              {p === "day" ? "Day" : p === "week" ? "Week" : "All Time"}
            </button>
          ))}
        </div>
      </div>

      {/* Grade Hero */}
      <div className={cn("rounded-xl border p-5 backdrop-blur-sm", grade.bgClass)}>
        <div className="flex flex-col items-center gap-5 sm:flex-row">
          <RadialProgress value={data.fsd_percent} size={140} strokeWidth={10} color={grade.ringColor}>
            <div className="text-center px-3">
              <p className={cn("font-bold leading-tight", grade.color, data.fsd_grade.length > 5 ? "text-sm" : "text-2xl")}>{data.fsd_grade}</p>
              <p className="text-xs text-slate-400">{Math.round(data.fsd_percent)}%</p>
            </div>
          </RadialProgress>
          <div className="flex flex-1 flex-col gap-3 text-center sm:text-left">
            <div className="grid grid-cols-3 gap-3">
              <div>
                <p className="text-xs text-slate-500">FSD Time</p>
                <p className="text-lg font-semibold text-slate-100">{data.fsd_time_formatted}</p>
              </div>
              <div>
                <p className="text-xs text-slate-500">Sessions</p>
                <p className="text-lg font-semibold text-slate-100">{data.fsd_sessions}</p>
              </div>
              <div>
                <p className="text-xs text-slate-500">Streak</p>
                <p className="text-lg font-semibold text-slate-100">
                  {data.streak_days > 0 && <Flame className="mr-1 inline h-4 w-4 text-orange-400" />}
                  {data.streak_days}d
                </p>
              </div>
            </div>
          </div>
        </div>
      </div>

      {/* Stat Cards */}
      <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
        <StatCard
          icon={Zap}
          label="Today"
          value={`${Math.round(data.today_percent)}%`}
          color={data.today_percent >= 90 ? "emerald" : data.today_percent >= 60 ? "blue" : "amber"}
        />
        <StatCard
          icon={TrendingUp}
          label={period === "day" ? "Day" : period === "week" ? "Week" : "All Time"}
          value={`${Math.round(data.fsd_percent)}%`}
          color={data.fsd_percent >= 90 ? "emerald" : data.fsd_percent >= 60 ? "blue" : "amber"}
        />
        <StatCard
          icon={Calendar}
          label="Best Day"
          value={`${Math.round(data.best_day_percent)}%`}
          sub={data.best_day ? new Date(data.best_day + "T00:00:00").toLocaleDateString([], { month: "short", day: "numeric" }) : "—"}
          color="emerald"
        />
        <StatCard
          icon={AlertTriangle}
          label="Avg. Disengagements"
          value={data.avg_disengagements_per_drive.toFixed(1)}
          sub="per drive"
          color={data.avg_disengagements_per_drive <= 1 ? "emerald" : data.avg_disengagements_per_drive <= 3 ? "amber" : "red"}
        />
      </div>

      {/* Chart */}
      {barData.length > 0 && (
        <div className="rounded-xl border border-white/10 bg-white/[0.03] p-4 backdrop-blur-sm">
          <div className="mb-3 flex items-center gap-2">
            {isMonthDrillDown && (
              <button
                onClick={() => setSelectedMonth(null)}
                className="rounded-md p-1 text-slate-400 transition-colors hover:bg-white/10 hover:text-slate-200"
              >
                <ChevronLeft className="h-4 w-4" />
              </button>
            )}
            <h2 className="text-sm font-semibold text-slate-200">{chartTitle}</h2>
            {showMonthly && (
              <span className="text-[10px] text-slate-500">tap a month to see days</span>
            )}
          </div>
          <BarChart
            data={barData}
            maxValue={100}
            height={160}
            formatValue={(v) => `${v}%`}
            onBarClick={onBarClick}
          />
          <div className="mt-2 flex items-center gap-4 text-[10px] text-slate-500">
            <span className="flex items-center gap-1"><span className="inline-block h-2 w-2 rounded-sm bg-emerald-400" /> 90%+</span>
            <span className="flex items-center gap-1"><span className="inline-block h-2 w-2 rounded-sm bg-blue-400" /> 60%+</span>
            <span className="flex items-center gap-1"><span className="inline-block h-2 w-2 rounded-sm bg-amber-400" /> &lt;60%</span>
            {barData.some((d) => d.subLabel) && (
              <span className="flex items-center gap-1"><span className="inline-block h-2 w-2 rounded-sm bg-red-400" /> disengagements</span>
            )}
          </div>
        </div>
      )}

      {/* Distance & Events */}
      <div className="grid grid-cols-1 gap-3 sm:grid-cols-2">
        {/* Distance */}
        <div className="rounded-xl border border-white/10 bg-white/[0.03] p-4 backdrop-blur-sm">
          <h2 className="mb-3 text-sm font-semibold text-slate-200">Distance</h2>
          <div className="mb-2 h-3 w-full overflow-hidden rounded-full bg-slate-800">
            <div
              className="h-full rounded-full bg-gradient-to-r from-emerald-500 to-emerald-400 transition-all duration-700"
              style={{ width: `${Math.min(distPct, 100)}%` }}
            />
          </div>
          <div className="flex justify-between text-xs">
            <span className="text-emerald-400">{fsdDist.toFixed(1)} {distUnit} FSD</span>
            <span className="text-slate-500">{totalDist.toFixed(1)} {distUnit} total</span>
          </div>
        </div>

        {/* Events */}
        <div className="rounded-xl border border-white/10 bg-white/[0.03] p-4 backdrop-blur-sm">
          <h2 className="mb-3 text-sm font-semibold text-slate-200">Events</h2>
          <div className="space-y-2">
            <div className="flex items-center justify-between">
              <span className="text-xs text-slate-400">Disengagements</span>
              <span className="text-sm font-semibold text-red-400">{data.disengagements}</span>
            </div>
            <div className="flex items-center justify-between">
              <span className="text-xs text-slate-400">Accelerator Pushes</span>
              <span className="text-sm font-semibold text-amber-400">{data.accel_pushes}</span>
            </div>
            <div className="flex items-center justify-between">
              <span className="text-xs text-slate-400">Average per Drive</span>
              <span className="text-sm font-semibold text-slate-300">{data.avg_disengagements_per_drive.toFixed(1)}</span>
            </div>
            <div className="flex items-center justify-between">
              <span className="text-xs text-slate-400">Average Accelerator Pushes per Drive</span>
              <span className="text-sm font-semibold text-slate-300">{data.avg_accel_pushes_per_drive.toFixed(1)}</span>
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}

function StatCard({
  icon: Icon,
  label,
  value,
  sub,
  color = "blue",
}: {
  icon: React.ElementType
  label: string
  value: string
  sub?: string
  color?: string
}) {
  const colorMap: Record<string, string> = {
    emerald: "text-emerald-400",
    blue: "text-blue-400",
    amber: "text-amber-400",
    red: "text-red-400",
  }
  return (
    <div className="rounded-xl border border-white/10 bg-white/[0.03] p-3 backdrop-blur-sm">
      <div className="mb-1 flex items-center gap-1.5">
        <Icon className={cn("h-3 w-3", colorMap[color] || "text-blue-400")} />
        <span className="text-xs text-slate-500">{label}</span>
      </div>
      <p className={cn("text-xl font-bold", colorMap[color] || "text-blue-400")}>{value}</p>
      {sub && <p className="text-[10px] text-slate-500">{sub}</p>}
    </div>
  )
}
