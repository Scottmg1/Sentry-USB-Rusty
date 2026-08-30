import { useEffect, useState } from "react"
import { CheckIcon, CloseIcon, ProgressActivityIcon, VerifiedUserIcon } from "@/components/icons"
import type { StepProps } from "../SetupWizard"
import { cn } from "@/lib/utils"

/**
 * Privacy disclosure and analytics preference. The choice is saved
 * immediately so update checks honor it even if setup is not completed.
 */
export function PrivacyStep(_props: StepProps) {
  const [choice, setChoice] = useState<boolean | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Load existing value on mount so re-running setup shows the current state.
  useEffect(() => {
    fetch("/api/config/preference?key=analytics_opt_in")
      .then((r) => r.json())
      .then((data) => {
        if (typeof data?.value === "boolean") setChoice(data.value)
      })
      .catch(() => {
        // Pref hasn't been set yet — leave as null so neither button is
        // highlighted, forcing an explicit choice.
      })
  }, [])

  async function persist(value: boolean) {
    setSaving(true)
    setError(null)
    try {
      const res = await fetch("/api/config/preference", {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ key: "analytics_opt_in", value }),
      })
      if (!res.ok) throw new Error(`HTTP ${res.status}`)
      setChoice(value)
    } catch (e) {
      setError(`Couldn't save preference: ${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="flex flex-col items-center py-6">
      <div className="mb-6 flex h-20 w-20 items-center justify-center rounded-2xl bg-emerald-500/15">
        <VerifiedUserIcon className="h-10 w-10 text-emerald-400" />
      </div>

      <h2 className="text-center text-2xl font-bold text-slate-100">
        Privacy
      </h2>
      <p className="mt-3 max-w-xl text-center text-sm leading-relaxed text-slate-400">
        Before going further, here's everything Sentry-USB sends from your
        device and when — so you know what's leaving your network before it
        does.
      </p>

      {/* Outbound data-flow disclosure. */}
      <div className="mt-8 w-full max-w-2xl rounded-xl border border-white/10 bg-white/[0.02] p-5">
        <p className="mb-3 text-xs font-semibold uppercase tracking-wider text-slate-400">
          What we send, when, and why
        </p>
        <div className="divide-y divide-white/5">
          <FlowRow
            when="Daily update check"
            what="Software version, CPU architecture, board model"
            why="Detect vulnerable builds, ship compatible binaries"
            note="No device identifier unless you opt in below; the source IP is briefly used for rate limiting."
          />
          <FlowRow
            when="Once per install"
            what="Empty ping with no payload or device identifier"
            why="Count gross install volume on the server"
            note="The source IP is briefly rate-limited; only a daily aggregate count is stored. See the privacy wiki to suppress it."
          />
          <FlowRow
            when="When you use Sentry Cloud"
            what="Your account login + the files you sync"
            why="Sync requires it — the feature can't work otherwise"
            note="Don't sign in to Cloud if you don't want this."
          />
          <FlowRow
            when="When you use AI Support & Help"
            what="Messages, product/software version, SBC model, selected non-secret support settings, the Pi connection's public IP for abuse prevention + only diagnostics you explicitly approve"
            why="Generate product-specific troubleshooting help"
            note="Online AI: messages are processed by Ollama Cloud and stored redacted on Sentry Six servers for up to 90 days after the last activity; maintainers may review them. Approved diagnostics are retained for 7 days."
          />
          <FlowRow
            when="When you submit a wrap or lock chime"
            what="The file you uploaded + your IP for rate-limiting"
            why="Sharing the submission with the community"
            note="No device fingerprint is sent. Your IP is used and retained for rate-limiting and abuse handling."
          />
          <FlowRow
            when="If you enable iOS push notifications"
            what="A randomly-generated device pairing ID"
            why="Routing push notifications to your phone"
            note="Not tied to your hardware. Cleared when you unpair."
          />
        </div>
        <p className="mt-4 text-[11px] leading-relaxed text-slate-500">
          Full policy:{" "}
          <a
            href="https://sentry-six.com/privacy"
            target="_blank"
            rel="noopener noreferrer"
            className="text-slate-400 underline hover:text-slate-300"
          >
            sentry-six.com/privacy
          </a>
          . Source code:{" "}
          <a
            href="https://github.com/Sentry-Six/Sentry-USB-Rusty"
            target="_blank"
            rel="noopener noreferrer"
            className="text-slate-400 underline hover:text-slate-300"
          >
            github.com/Sentry-Six/Sentry-USB-Rusty
          </a>
          .
        </p>
      </div>

      {/* Opt-in — explicit affirmative action, no pre-tick */}
      <div className="mt-6 w-full max-w-2xl rounded-xl border border-white/10 bg-white/[0.02] p-5">
        <p className="text-sm font-semibold text-slate-200">
          Help us count new installs?
        </p>
        <p className="mt-2 text-xs leading-relaxed text-slate-400">
          If you opt in, daily update checks will include a one-way hashed
          device ID (derived from your board's serial number) so we can tell
          how many unique devices are running each version, without double-
          counting reinstalls. You can change this any time in Settings →
          System.
        </p>

        <div className="mt-4 flex flex-col gap-2 sm:flex-row">
          <button
            type="button"
            disabled={saving}
            onClick={() => persist(true)}
            className={cn(
              "flex flex-1 items-center justify-center gap-2 rounded-lg border px-4 py-3 text-sm font-medium transition-colors disabled:opacity-50",
              choice === true
                ? "border-emerald-400/60 bg-emerald-500/15 text-emerald-200"
                : "border-white/10 bg-white/[0.02] text-slate-300 hover:border-white/20 hover:bg-white/[0.05]"
            )}
          >
            {saving && choice !== true ? (
              <ProgressActivityIcon className="h-4 w-4 animate-spin" />
            ) : (
              <CheckIcon className="h-4 w-4" />
            )}
            Yes, count me
          </button>
          <button
            type="button"
            disabled={saving}
            onClick={() => persist(false)}
            className={cn(
              "flex flex-1 items-center justify-center gap-2 rounded-lg border px-4 py-3 text-sm font-medium transition-colors disabled:opacity-50",
              choice === false
                ? "border-slate-400/60 bg-slate-500/15 text-slate-200"
                : "border-white/10 bg-white/[0.02] text-slate-300 hover:border-white/20 hover:bg-white/[0.05]"
            )}
          >
            {saving && choice !== false ? (
              <ProgressActivityIcon className="h-4 w-4 animate-spin" />
            ) : (
              <CloseIcon className="h-4 w-4" />
            )}
            No thanks
          </button>
        </div>

        {choice === null && !error && (
          <p className="mt-3 text-[11px] text-slate-500">
            You can leave this unanswered and continue — no choice means no
            tracking. Default is opted out.
          </p>
        )}
        {choice !== null && !error && (
          <p className="mt-3 text-[11px] text-emerald-300/70">
            Saved. You can change this any time in Settings → System.
          </p>
        )}
        {error && (
          <p className="mt-3 text-[11px] text-rose-400">
            {error}
          </p>
        )}
      </div>
    </div>
  )
}

function FlowRow({
  when,
  what,
  why,
  note,
}: {
  when: string
  what: string
  why: string
  note?: string
}) {
  return (
    <div className="grid grid-cols-1 gap-1 py-3 sm:grid-cols-[140px_1fr]">
      <div className="text-xs font-semibold text-slate-300">{when}</div>
      <div>
        <p className="text-xs text-slate-300">{what}</p>
        <p className="mt-0.5 text-[11px] text-slate-500">{why}</p>
        {note && (
          <p className="mt-1 text-[11px] italic text-slate-500/80">{note}</p>
        )}
      </div>
    </div>
  )
}
