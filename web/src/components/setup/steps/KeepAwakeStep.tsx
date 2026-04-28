import { Bluetooth, Webhook, Zap } from "lucide-react"
import type { StepProps } from "../SetupWizard"
import { SecretInput } from "../SecretInput"
import { cn } from "@/lib/utils"

const methods = [
  { id: "ble", label: "Bluetooth LE", icon: Bluetooth, desc: "Direct connection, unlimited. Requires initial pairing." },
  { id: "teslafi", label: "TeslaFi", icon: Zap, desc: "Cloud API via TeslaFi service. Requires paid subscription." },
  { id: "tessie", label: "Tessie", icon: Zap, desc: "Cloud API via Tessie service. Requires paid subscription." },
  { id: "webhook", label: "Webhook", icon: Webhook, desc: "Send webhook to external service (e.g. Home Assistant)." },
  { id: "none", label: "None", icon: Zap, desc: "No keep-awake. Use Sentry/Camp mode manually." },
]

const sentryCases = [
  { id: "1", label: "Case 1: Sentry ON everywhere except home", desc: "Sentry Mode turns OFF after archiving at home. Turns ON when you drive." },
  { id: "3", label: "Case 3: No Sentry Mode (periodic nudge)", desc: "Periodic keep-awake command without Sentry Mode. BLE/Tessie/Webhook only." },
]

function Field({ label, field, type = "text", placeholder, data, onChange, hint, error }: {
  label: string; field: string; type?: string; placeholder?: string
  data: StepProps["data"]; onChange: StepProps["onChange"]; hint?: string; error?: boolean
}) {
  const inputCls = cn(
    "w-full rounded-lg border bg-white/5 px-3 py-2 text-sm text-slate-100 placeholder-slate-600 outline-none transition focus:ring-1",
    error
      ? "border-red-500/50 focus:border-red-500/50 focus:ring-red-500/25"
      : "border-white/10 focus:border-blue-500/50 focus:ring-blue-500/25"
  )
  return (
    <div>
      <label className="mb-1 block text-sm font-medium text-slate-300">{label}</label>
      {type === "password" ? (
        <SecretInput value={data[field] ?? ""} onChange={(v) => onChange(field, v)}
          placeholder={placeholder} className={cn(inputCls, "pr-8")} />
      ) : (
        <input type={type} value={data[field] ?? ""} onChange={(e) => onChange(field, e.target.value)}
          placeholder={placeholder} className={inputCls} />
      )}
      {hint && <p className="mt-1 text-xs text-slate-600">{hint}</p>}
    </div>
  )
}

export function KeepAwakeStep({ data, onChange, onBatchChange }: StepProps) {
  // Derive initial method from existing data, then track via _KEEP_AWAKE_METHOD
  const method = data._KEEP_AWAKE_METHOD
    || (data.TESLA_BLE_VIN ? "ble"
      : data.TESLAFI_API_TOKEN ? "teslafi"
      : data.TESSIE_API_TOKEN ? "tessie"
      : data.KEEP_AWAKE_WEBHOOK_URL ? "webhook"
      : "none")

  function setMethod(m: string) {
    onBatchChange({
      _KEEP_AWAKE_METHOD: m,
      TESLA_BLE_VIN: m === "ble" ? (data.TESLA_BLE_VIN || "") : "",
      TESLAFI_API_TOKEN: m === "teslafi" ? (data.TESLAFI_API_TOKEN || "") : "",
      TESSIE_API_TOKEN: m === "tessie" ? (data.TESSIE_API_TOKEN || "") : "",
      TESSIE_VIN: m === "tessie" ? (data.TESSIE_VIN || "") : "",
      KEEP_AWAKE_WEBHOOK_URL: m === "webhook" ? (data.KEEP_AWAKE_WEBHOOK_URL || "") : "",
    })
  }

  return (
    <div className="space-y-6">
      <div>
        <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
          Keep Car Awake Method
        </h3>
        <p className="mt-1 text-xs text-slate-500">
          The car may cut USB power when sleeping. Choose a method to keep it awake during archiving.
        </p>
      </div>

      <div className="grid grid-cols-2 gap-2 sm:grid-cols-3">
        {methods.map((m) => (
          <button key={m.id} onClick={() => setMethod(m.id)}
            className={cn("rounded-lg border p-3 text-left transition-colors",
              method === m.id ? "border-blue-500/40 bg-blue-500/10" : "border-white/5 bg-white/[0.02] hover:border-white/10")}>
            <div className="flex items-center gap-2">
              <m.icon className={cn("h-4 w-4", method === m.id ? "text-blue-400" : "text-slate-600")} />
              <p className={cn("text-sm font-medium", method === m.id ? "text-blue-400" : "text-slate-300")}>{m.label}</p>
            </div>
            <p className="mt-1 text-xs text-slate-600">{m.desc}</p>
          </button>
        ))}
      </div>

      {/* Sentry Case */}
      {method !== "none" && (
        <div className="space-y-2">
          <p className="text-xs font-medium uppercase tracking-wider text-slate-500">Sentry Mode Behavior</p>
          {sentryCases.map((c) => {
            const disabled = c.id === "3" && method === "teslafi"
            return (
              <label key={c.id} className={cn("flex cursor-pointer items-start gap-3 rounded-lg border p-3 transition-colors",
                data.SENTRY_CASE === c.id ? "border-blue-500/40 bg-blue-500/10" : "border-white/5 bg-white/[0.02]",
                disabled && "cursor-not-allowed opacity-40")}>
                <input type="radio" name="sentry_case" value={c.id} checked={data.SENTRY_CASE === c.id}
                  disabled={disabled} onChange={() => onChange("SENTRY_CASE", c.id)}
                  className="mt-0.5 accent-blue-500" />
                <div>
                  <p className="text-sm font-medium text-slate-300">{c.label}</p>
                  <p className="mt-0.5 text-xs text-slate-600">{c.desc}</p>
                </div>
              </label>
            )
          })}
        </div>
      )}

      {/* Method-specific fields */}
      {method === "ble" && (
        <Field label="Vehicle VIN" field="TESLA_BLE_VIN" placeholder="5YJ3E1EA4JF000001" data={data} onChange={onChange}
          hint="After setup, use the Pair BLE button in Settings to complete pairing."
          error={!data.TESLA_BLE_VIN?.trim()} />
      )}
      {method === "teslafi" && (
        <Field label="TeslaFi API Token" field="TESLAFI_API_TOKEN" type="password" placeholder="Your TeslaFi API token" data={data} onChange={onChange}
          error={!data.TESLAFI_API_TOKEN?.trim()} />
      )}
      {method === "tessie" && (
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Tessie API Token" field="TESSIE_API_TOKEN" type="password" placeholder="Your Tessie API token" data={data} onChange={onChange}
            error={!data.TESSIE_API_TOKEN?.trim()} />
          <Field label="Vehicle VIN" field="TESSIE_VIN" placeholder="5YJ3E1EA4JF000001" data={data} onChange={onChange}
            error={!data.TESSIE_VIN?.trim()} />
        </div>
      )}
      {method === "webhook" && (
        <Field label="Webhook URL" field="KEEP_AWAKE_WEBHOOK_URL" type="password" placeholder="http://homeassistant.local/api/webhook/..." data={data} onChange={onChange}
          error={!data.KEEP_AWAKE_WEBHOOK_URL?.trim()} />
      )}

      {/* Sentry case required when a method is active */}
      {method !== "none" && !data.SENTRY_CASE && (
        <p className="text-xs text-red-400">Select a Sentry Mode behavior above to continue.</p>
      )}
    </div>
  )
}
