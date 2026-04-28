import { useEffect, useState } from "react"
import { AlertTriangle } from "lucide-react"
import type { StepProps } from "./SetupWizard"

type Unit = "G" | "M"

export function SizeInput({
  label,
  field,
  data,
  onChange,
  hint,
  defaultVal,
  warning,
  disabled,
}: {
  label: string
  field: string
  data: StepProps["data"]
  onChange: StepProps["onChange"]
  hint: string
  defaultVal: string
  warning?: string
  disabled?: boolean
}) {
  const raw = data[field] ?? ""

  const numericVal = raw.replace(/[gGmM]/g, "")

  const [unit, setUnit] = useState<Unit>(() => (/[mM]$/.test(raw) ? "M" : "G"))

  const [localVal, setLocalVal] = useState(numericVal)
  const [focused, setFocused] = useState(false)

  useEffect(() => {
    if (/[mM]$/.test(raw)) setUnit("M")
    else if (/[gG]$/.test(raw)) setUnit("G")
  }, [raw])

  useEffect(() => {
    if (!focused) setLocalVal(numericVal)
  }, [numericVal, focused])

  const handleFocus = () => {
    if (disabled) return
    setFocused(true)
    setLocalVal(numericVal)
  }

  const handleBlur = () => {
    if (disabled) return
    setFocused(false)
    const cleaned = localVal.replace(/[^0-9]/g, "")
    onChange(field, cleaned ? cleaned + unit : (defaultVal ? defaultVal : ""))
  }

  const handleChange = (v: string) => {
    if (disabled) return
    setLocalVal(v.replace(/[^0-9]/g, ""))
  }

  const handleUnitChange = (newUnit: Unit) => {
    if (disabled) return
    setUnit(newUnit)
    const currentNum = focused ? localVal : numericVal
    if (currentNum) onChange(field, currentNum + newUnit)
  }

  const unitLabel = unit === "M" ? "MB" : "GB"
  const displayNum = focused ? localVal : numericVal
  const displayText = displayNum
    ? `${displayNum} ${unitLabel}`
    : defaultVal
      ? `${defaultVal} GB`
      : "—"

  const inputCls = disabled
    ? "flex-1 cursor-not-allowed rounded-lg border border-white/5 bg-white/[0.02] px-3 py-2 text-sm text-slate-500 outline-none"
    : "flex-1 rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-sm text-slate-100 placeholder-slate-600 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"

  const selectCls = disabled
    ? "cursor-not-allowed rounded-lg border border-white/5 bg-white/[0.02] px-2 py-2 text-sm text-slate-500 outline-none"
    : "rounded-lg border border-white/10 bg-slate-900 px-2 py-2 text-sm text-slate-100 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25 [&>option]:bg-slate-900 [&>option]:text-slate-100"

  return (
    <div className={`rounded-lg border border-white/5 bg-white/[0.02] p-4 ${disabled ? "opacity-70" : ""}`}>
      <div className="mb-2 flex items-center justify-between">
        <label className={`text-sm font-medium ${disabled ? "text-slate-500" : "text-slate-300"}`}>{label}</label>
        <span className={`text-sm font-mono ${disabled ? "text-slate-600" : "text-blue-400"}`}>{displayText}</span>
      </div>
      <div className="flex gap-2">
        <input
          type="text"
          inputMode="numeric"
          value={focused ? localVal : numericVal}
          onChange={(e) => handleChange(e.target.value)}
          onFocus={handleFocus}
          onBlur={handleBlur}
          placeholder={defaultVal}
          disabled={disabled}
          title={disabled ? "Locked after first setup" : undefined}
          className={inputCls}
        />
        <select
          value={unit}
          onChange={(e) => handleUnitChange(e.target.value as Unit)}
          disabled={disabled}
          title={disabled ? "Locked after first setup" : undefined}
          className={selectCls}
        >
          <option value="G">GB</option>
          <option value="M">MB</option>
        </select>
      </div>
      <p className="mt-1 text-xs text-slate-600">{hint}</p>
      {!disabled && warning && numericVal && unit === "G" && (
        <div className="mt-2 flex items-start gap-2 rounded-lg border border-amber-500/20 bg-amber-500/5 px-3 py-2">
          <AlertTriangle className="mt-0.5 h-3.5 w-3.5 shrink-0 text-amber-400" />
          <p className="text-xs text-amber-300">{warning}</p>
        </div>
      )}
    </div>
  )
}
