import { useState } from "react"
import { Lock, Key, Copy, Check, Loader2, RefreshCw } from "lucide-react"
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

function NasSSHKey() {
  const [pubKey, setPubKey] = useState<string | null>(null)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [copied, setCopied] = useState(false)

  async function fetchKey() {
    setLoading(true)
    setError(null)
    try {
      const res = await fetch("/api/system/ssh-pubkey")
      if (!res.ok) throw new Error("Failed to fetch SSH key")
      const data = await res.json()
      if (data.public_key) {
        setPubKey(data.public_key)
      } else {
        setPubKey(null)
      }
    } catch {
      setPubKey(null)
    } finally {
      setLoading(false)
    }
  }

  async function generateKey() {
    setLoading(true)
    setError(null)
    try {
      const res = await fetch("/api/system/ssh-keygen", { method: "POST" })
      if (!res.ok) {
        const data = await res.json()
        throw new Error(data.error || "Failed to generate SSH key")
      }
      const data = await res.json()
      setPubKey(data.public_key)
    } catch (err) {
      setError(err instanceof Error ? err.message : "Unknown error")
    } finally {
      setLoading(false)
    }
  }

  function copyKey() {
    if (!pubKey) return
    navigator.clipboard.writeText(pubKey)
    setCopied(true)
    setTimeout(() => setCopied(false), 2000)
  }

  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2">
        <Key className="h-4 w-4 text-blue-400" />
        <h4 className="text-sm font-medium text-slate-300">NAS SSH Key (rsync)</h4>
      </div>
      <p className="text-xs text-slate-500">
        Generate an SSH keypair so Sentry USB can connect to your NAS for rsync
        archiving without a password. Copy the public key below and add it to
        your NAS's <code className="rounded bg-white/10 px-1">~/.ssh/authorized_keys</code> file.
      </p>

      {pubKey ? (
        <div className="space-y-2">
          <div className="relative">
            <pre className="overflow-x-auto rounded-lg border border-white/10 bg-white/5 px-3 py-2 pr-10 font-mono text-xs text-slate-300 leading-relaxed">
              {pubKey}
            </pre>
            <button
              onClick={copyKey}
              className="absolute right-2 top-2 rounded p-1 text-slate-500 transition hover:bg-white/10 hover:text-slate-300"
              title="Copy to clipboard"
            >
              {copied ? <Check className="h-4 w-4 text-emerald-400" /> : <Copy className="h-4 w-4" />}
            </button>
          </div>
          <button
            onClick={generateKey}
            disabled={loading}
            className="flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-xs text-slate-400 transition hover:bg-white/10 hover:text-slate-300 disabled:opacity-50"
          >
            {loading ? <Loader2 className="h-3 w-3 animate-spin" /> : <RefreshCw className="h-3 w-3" />}
            Regenerate Key
          </button>
        </div>
      ) : (
        <div className="flex items-center gap-3">
          <button
            onClick={generateKey}
            disabled={loading}
            className="flex items-center gap-1.5 rounded-lg bg-blue-500/20 px-4 py-2 text-sm font-medium text-blue-400 transition hover:bg-blue-500/30 disabled:opacity-50"
          >
            {loading ? <Loader2 className="h-4 w-4 animate-spin" /> : <Key className="h-4 w-4" />}
            Generate SSH Key
          </button>
          <button
            onClick={fetchKey}
            disabled={loading}
            className="flex items-center gap-1.5 rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-sm text-slate-400 transition hover:bg-white/10 hover:text-slate-300 disabled:opacity-50"
          >
            {loading ? <Loader2 className="h-4 w-4 animate-spin" /> : <RefreshCw className="h-4 w-4" />}
            Check Existing
          </button>
        </div>
      )}

      {error && <p className="text-xs text-red-400">{error}</p>}
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

      {/* NAS SSH Key for rsync */}
      <NasSSHKey />
    </div>
  )
}
