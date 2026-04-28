import { useState } from "react"
import { Key, Copy, Check, Loader2, RefreshCw } from "lucide-react"

export function NasSSHKey({
  pubKey,
  setPubKey,
}: {
  pubKey: string | null
  setPubKey: (v: string | null) => void
}) {
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
      setPubKey(data.public_key || null)
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

  async function copyKey() {
    if (!pubKey) return
    // The Pi serves over plain HTTP on the LAN. navigator.clipboard is gated
    // on a "secure context" in modern browsers, so calling it from
    // http://sentryusb.local throws or rejects. Try modern API first, fall
    // back to legacy execCommand which works on http://.
    let ok = false
    if (navigator.clipboard && window.isSecureContext) {
      try {
        await navigator.clipboard.writeText(pubKey)
        ok = true
      } catch {
        // fall through
      }
    }
    if (!ok) {
      const ta = document.createElement("textarea")
      ta.value = pubKey
      ta.setAttribute("readonly", "")
      ta.style.position = "fixed"
      ta.style.top = "0"
      ta.style.left = "0"
      ta.style.opacity = "0"
      document.body.appendChild(ta)
      ta.focus()
      ta.select()
      try {
        ok = document.execCommand("copy")
      } catch {
        ok = false
      }
      document.body.removeChild(ta)
    }
    if (ok) {
      setCopied(true)
      setTimeout(() => setCopied(false), 2000)
    } else {
      setError("Copy failed — select the key text manually with your mouse.")
    }
  }

  return (
    <div className="space-y-3 rounded-lg border border-white/5 bg-white/[0.02] p-4">
      <div className="flex items-center gap-2">
        <Key className="h-4 w-4 text-blue-400" />
        <h4 className="text-sm font-medium text-slate-300">NAS SSH Key</h4>
      </div>
      <div className="space-y-2 text-xs text-slate-500">
        <p>
          Generate an SSH key so Sentry USB can rsync to your server without a password.
          To install it on your server:
        </p>
        <ol className="ml-4 list-decimal space-y-1">
          <li>Click <strong>Generate SSH Key</strong> below.</li>
          <li>Copy the public key with the copy button.</li>
          <li>
            SSH into your server as the user above and run:
            <pre className="mt-1 overflow-x-auto rounded bg-black/40 px-2 py-1 font-mono text-[11px] leading-relaxed text-slate-300">
{`mkdir -p ~/.ssh && chmod 700 ~/.ssh
echo "<paste-key-here>" >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys`}
            </pre>
          </li>
          <li>Click <strong>Test Connection</strong> above to verify.</li>
        </ol>
        <p className="text-slate-600">
          The test uses the <code className="rounded bg-white/10 px-1">ssh</code> client
          built into Sentry USB — no extra packages need to be installed on this device.
          The test is permissive about host-key verification; the real archive job uses
          strict host-key checking. If archiving later fails with{" "}
          <em>"Host key verification failed"</em>, SSH into the Pi and run{" "}
          <code className="rounded bg-white/10 px-1">ssh-keyscan -H your-server &gt;&gt; /root/.ssh/known_hosts</code>.
        </p>
      </div>

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
