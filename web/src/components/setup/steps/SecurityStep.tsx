import { Lock } from "lucide-react"
import type { StepProps } from "../SetupWizard"
import { SecretInput } from "../SecretInput"
import { cn } from "@/lib/utils"

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

export function SecurityStep({ data, onChange }: StepProps) {
  return (
    <div className="space-y-6">
      <div className="flex items-center gap-2">
        <Lock className="h-4 w-4 text-blue-400" />
        <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
          Security
        </h3>
      </div>

      {/* Web UI auth */}
      <div>
        <p className="mb-3 text-xs text-slate-500">
          Protect the web interface with a username and password. Recommended if
          using a WiFi Access Point.
        </p>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Web Username" field="WEB_USERNAME" placeholder="pi" data={data} onChange={onChange}
            hint="Leave empty to disable web auth" />
          <Field label="Web Password" field="WEB_PASSWORD" type="password" placeholder="password" data={data} onChange={onChange}
            error={!!(data.WEB_USERNAME?.trim() && !data.WEB_PASSWORD?.trim())} />
        </div>
      </div>
    </div>
  )
}
