import { useEffect, useState } from "react"
import {
  CheckIcon,
  CloseIcon,
  DescriptionIcon,
  DownloadIcon,
  ProgressActivityIcon,
  SettingsIcon,
  VerifiedUserIcon,
  WandStarsIcon,
} from "@/components/icons"
import { PrefCard, PrefGrid } from "@/components/settings/PrefCard"
import { ConfigBackupSection } from "@/components/settings/sections/ConfigBackupSection"
import { StorageRepairCard } from "@/components/settings/sections/StorageRepairCard"
import { cn } from "@/lib/utils"

interface Props {
  onOpenRawConfig: () => void
  onOpenWizard: () => void
  /** App version, written into the export file header. */
  version?: string | null
  /** Device hostname, written into the export file header. */
  hostname?: string | null
}

/**
 * Settings → System. Consolidates what used to be three sparse, low-traffic
 * tabs — Backups, About, and Privacy — into one admin/maintenance surface:
 *
 *   - Config Backup / Export / Raw Config   (was: Backups tab)
 *   - Setup Wizard + Resources              (was: About tab)
 *   - Analytics opt-in + disclosure         (was: Privacy tab)
 */
export function SystemTab({ onOpenRawConfig, onOpenWizard, version, hostname }: Props) {
  // Export the device's full configuration as a bash-sourceable .conf file.
  // Active settings become `export KEY='value'` lines; defaults become
  // `# export KEY='value'`. Rusty-only Web-UI preferences (the JSON kv-store
  // at /mutable/.sentryusb_preferences.json) are appended as `# preference:`
  // comment lines for export completeness without polluting the bash
  // namespace if the file is ever sourced. Single quotes inside values are
  // escaped via the standard '\'' trick so the file stays valid bash.
  async function exportConfig(): Promise<void> {
    try {
      const [configRes, prefsRes] = await Promise.all([
        fetch("/api/setup/config"),
        fetch("/api/config/preference"),
      ])
      if (!configRes.ok) throw new Error("Failed to read config")
      const config = (await configRes.json()) as Record<
        string,
        { value: string; active: boolean }
      >
      const prefs = prefsRes.ok
        ? ((await prefsRes.json()) as Record<string, unknown>)
        : {}

      const now = new Date().toISOString()
      const ver = version || "unknown"
      const host = hostname || "sentryusb"
      const escape = (s: string) => (s ?? "").replace(/'/g, "'\\''")

      let content = ""
      content += `# sentryusb.conf — exported from Sentry USB UI\n`
      content += `# Exported:  ${now}\n`
      content += `# Hostname:  ${host}\n`
      content += `# Version:   ${ver}\n`
      content += `#\n`
      content += `# This file is bash-sourceable. Active settings are 'export' lines;\n`
      content += `# inactive/default values are commented out for reference.\n`
      content += `\n`
      content += `# === Setup configuration ===\n`

      const keys = Object.keys(config).sort()
      for (const k of keys) {
        const e = config[k]
        const v = escape(e.value ?? "")
        if (e.active) {
          content += `export ${k}='${v}'\n`
        } else {
          content += `# export ${k}='${v}'\n`
        }
      }

      const prefKeys = Object.keys(prefs).sort()
      if (prefKeys.length > 0) {
        content += `\n`
        content += `# === Web UI preferences (Sentry USB Rusty) ===\n`
        content += `# Managed via the web UI; stored in /mutable/.sentryusb_preferences.json.\n`
        content += `# Listed here for export completeness — these are NOT sourced by bash.\n`
        for (const k of prefKeys) {
          const v = prefs[k]
          content += `# preference: ${k} = ${JSON.stringify(v)}\n`
        }
      }

      const blob = new Blob([content], { type: "text/plain" })
      const url = URL.createObjectURL(blob)
      const a = document.createElement("a")
      a.href = url
      a.download = "sentryusb.conf"
      a.click()
      URL.revokeObjectURL(url)
    } catch {
      /* ignore */
    }
  }

  return (
    <PrefGrid min={300}>
      {/* --- Config Backup + Configuration, grouped into one masonry column
          so the Configuration card sits directly UNDER Config Backup. The
          height-balanced grid won't reliably stack two separate siblings,
          so we wrap them in a single column unit. --- */}
      <div className="flex flex-col gap-2.5">
        <ConfigBackupSection />
        <PrefCard
          icon={<SettingsIcon className="h-3.5 w-3.5" />}
          halo="slate"
          title="Configuration"
        >
          <p className="t-xs">
            Edit individual settings keys directly, or export your full active
            configuration as a single shell-format file — handy for migrating to a
            new Pi or sharing a recipe.
          </p>
          <div className="flex flex-wrap gap-2">
            <button
              onClick={onOpenRawConfig}
              className="rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10"
            >
              <SettingsIcon className="mr-1.5 inline h-3.5 w-3.5" />
              Open editor
            </button>
            <button
              onClick={exportConfig}
              className="rounded-lg border border-white/10 bg-white/5 px-3 py-1.5 text-xs font-medium text-slate-300 transition-colors hover:bg-white/10"
            >
              <DownloadIcon className="mr-1.5 inline h-3.5 w-3.5" />
              Download sentryusb.conf
            </button>
          </div>
        </PrefCard>
      </div>

      {/* --- Storage repair (external-SSD XFS recovery) --- */}
      <StorageRepairCard />

      {/* --- Setup Wizard + Resources (was: About) --- */}
      <PrefCard
        icon={<WandStarsIcon className="h-3.5 w-3.5" />}
        halo="accent"
        title="Setup Wizard"
      >
        <p className="t-xs">
          Re-run the first-time setup wizard to reconfigure WiFi, drives, time zones and units.
          Safe to run any time — your existing config is the starting point.
        </p>
        <button
          onClick={onOpenWizard}
          className="self-start rounded-lg bg-blue-500/15 px-3 py-1.5 text-xs font-medium text-blue-400 transition-colors hover:bg-blue-500/25"
        >
          Launch Wizard
        </button>
        <div className="tile-divider" />
        <p className="section-label">Resources</p>
        <div className="flex flex-col gap-1">
          <a
            href="https://github.com/Sentry-Six/Sentry-USB-Rusty"
            target="_blank"
            rel="noopener noreferrer"
            className="t-sm text-blue-400 hover:text-blue-300"
          >
            GitHub repository ↗
          </a>
          <a
            href="https://discord.gg/9QZEzVwdnt"
            target="_blank"
            rel="noopener noreferrer"
            className="t-sm text-violet-400 hover:text-violet-300"
          >
            Discord community ↗
          </a>
        </div>
      </PrefCard>

      {/* --- Privacy (was: Privacy tab) --- */}
      <PrivacyCards />
    </PrefGrid>
  )
}

/** Settings → System privacy disclosure and analytics preference. */
function PrivacyCards() {
  const [choice, setChoice] = useState<boolean | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [loaded, setLoaded] = useState(false)

  useEffect(() => {
    fetch("/api/config/preference?key=analytics_opt_in")
      .then((r) => r.json())
      .then((data) => {
        if (typeof data?.value === "boolean") setChoice(data.value)
        setLoaded(true)
      })
      .catch(() => setLoaded(true))
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
      setError(`Couldn't save: ${e instanceof Error ? e.message : String(e)}`)
    } finally {
      setSaving(false)
    }
  }

  return (
    <>
      <PrefCard
        icon={<VerifiedUserIcon className="h-3.5 w-3.5" />}
        halo="accent"
        title="Analytics opt-in"
      >
        <p className="t-xs">
          When opted in, daily update checks include a one-way hashed device
          ID (derived from your board serial) so we can count unique installs
          without double-counting reinstalls. When opted out, no
          device-derived identifier is sent on future update checks; normal
          connection metadata such as the source IP is briefly used for rate
          limiting. Opting out does not automatically erase a row sent earlier;
          email privacy@sentry-six.com to request its deletion.
        </p>

        <div className="mt-1 flex flex-col gap-2 sm:flex-row">
          <button
            type="button"
            disabled={saving || !loaded}
            onClick={() => persist(true)}
            className={cn(
              "flex flex-1 items-center justify-center gap-2 rounded-lg border px-3 py-2 text-xs font-medium transition-colors disabled:opacity-50",
              choice === true
                ? "border-emerald-400/60 bg-emerald-500/15 text-emerald-200"
                : "border-white/10 bg-white/[0.02] text-slate-300 hover:border-white/20 hover:bg-white/[0.05]"
            )}
          >
            {saving && choice !== true ? (
              <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <CheckIcon className="h-3.5 w-3.5" />
            )}
            Opted in
          </button>
          <button
            type="button"
            disabled={saving || !loaded}
            onClick={() => persist(false)}
            className={cn(
              "flex flex-1 items-center justify-center gap-2 rounded-lg border px-3 py-2 text-xs font-medium transition-colors disabled:opacity-50",
              choice === false
                ? "border-slate-400/60 bg-slate-500/15 text-slate-200"
                : "border-white/10 bg-white/[0.02] text-slate-300 hover:border-white/20 hover:bg-white/[0.05]"
            )}
          >
            {saving && choice !== false ? (
              <ProgressActivityIcon className="h-3.5 w-3.5 animate-spin" />
            ) : (
              <CloseIcon className="h-3.5 w-3.5" />
            )}
            Opted out
          </button>
        </div>

        {choice === null && loaded && !error && (
          <p className="mt-2 text-[11px] text-slate-500">
            Not set. Default is opted out — no choice means no tracking.
          </p>
        )}
        {error && <p className="mt-2 text-[11px] text-rose-400">{error}</p>}
      </PrefCard>

      <PrefCard
        icon={<DescriptionIcon className="h-3.5 w-3.5" />}
        halo="slate"
        title="What we send, and when"
      >
        <div className="divide-y divide-white/5">
          <FlowRow
            when="Daily update check"
            what="Version, architecture, board model"
            note="Device identifier only if opted in above."
          />
          <FlowRow
            when="Once per install"
            what="Empty ping with no payload or device identifier"
            note="The source IP is briefly rate-limited; only a daily aggregate count is stored."
          />
          <FlowRow
            when="Sentry Cloud (if signed in)"
            what="Account credentials + synced files"
            note="Stop using Cloud to stop this."
          />
          <FlowRow
            when="AI Support & Help (when used)"
            what="Messages, product/software version, SBC model, selected non-secret support settings, the Pi connection's public IP for abuse prevention + diagnostics you explicitly approve"
            note="Ollama Cloud processes messages. Redacted chats stay on Sentry Six servers up to 90 days after the last activity and may be reviewed; approved diagnostics stay 7 days."
          />
          <FlowRow
            when="Wraps / lock chime submissions"
            what="The file + your IP for rate-limiting"
            note="No device fingerprint."
          />
          <FlowRow
            when="iOS push pairing"
            what="A random pairing ID"
            note="Not tied to your hardware."
          />
        </div>
        <div className="tile-divider" />
        <div className="flex flex-col gap-1">
          <a
            href="https://sentry-six.com/privacy"
            target="_blank"
            rel="noopener noreferrer"
            className="t-sm text-blue-400 hover:text-blue-300"
          >
            Full privacy policy ↗
          </a>
          <a
            href="https://github.com/Sentry-Six/Sentry-USB-Rusty/wiki/Privacy"
            target="_blank"
            rel="noopener noreferrer"
            className="t-sm text-blue-400 hover:text-blue-300"
          >
            Wiki: what each flow does ↗
          </a>
        </div>
      </PrefCard>
    </>
  )
}

function FlowRow({
  when,
  what,
  note,
}: {
  when: string
  what: string
  note?: string
}) {
  return (
    <div className="py-2">
      <p className="text-xs font-semibold text-slate-300">{when}</p>
      <p className="mt-0.5 text-[11px] text-slate-400">{what}</p>
      {note && (
        <p className="mt-0.5 text-[11px] italic text-slate-500/80">{note}</p>
      )}
    </div>
  )
}
