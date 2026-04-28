import { CheckCircle } from "lucide-react"
import type { StepProps } from "../SetupWizard"

const sections = [
  {
    title: "Network",
    fields: ["SENTRYUSB_HOSTNAME", "AP_SSID", "AP_PASS", "AP_IP"],
  },
  {
    title: "Storage",
    fields: ["CAM_SIZE", "MUSIC_SIZE", "LIGHTSHOW_SIZE", "BOOMBOX_SIZE", "DATA_DRIVE", "USE_EXFAT"],
  },
  {
    title: "Archive",
    fields: [
      "ARCHIVE_SYSTEM", "ARCHIVE_SERVER", "SHARE_NAME", "SHARE_USER", "SHARE_PASSWORD",
      "RSYNC_SERVER", "RSYNC_USER", "RSYNC_PATH",
      "RCLONE_DRIVE", "RCLONE_PATH",
      "ARCHIVE_SAVEDCLIPS", "ARCHIVE_SENTRYCLIPS", "ARCHIVE_RECENTCLIPS",
    ],
  },
  {
    title: "Keep Awake",
    fields: ["TESLA_BLE_VIN", "TESLAFI_API_TOKEN", "TESSIE_API_TOKEN", "TESSIE_VIN", "KEEP_AWAKE_WEBHOOK_URL", "SENTRY_CASE"],
  },
  {
    title: "Notifications",
    fields: [
      "NOTIFICATION_TITLE",
      "PUSHOVER_ENABLED", "GOTIFY_ENABLED", "DISCORD_ENABLED", "TELEGRAM_ENABLED",
      "IFTTT_ENABLED", "SLACK_ENABLED", "SIGNAL_ENABLED", "MATRIX_ENABLED",
      "SNS_ENABLED", "WEBHOOK_ENABLED",
    ],
  },
  {
    title: "Security",
    fields: ["WEB_USERNAME", "WEB_PASSWORD"],
  },
  {
    title: "Advanced",
    fields: [
      "TIME_ZONE", "ARCHIVE_DELAY", "SNAPSHOT_INTERVAL",
      "TEMPERATURE_UNIT", "TEMPERATURE_WARNING", "TEMPERATURE_CAUTION", "TEMPERATURE_INTERVAL", "TEMPERATURE_POSTARCHIVE",
      "RTC_BATTERY_ENABLED", "RTC_TRICKLE_CHARGE",
      "INCREASE_ROOT_SIZE", "CPU_GOVERNOR", "REPO", "BRANCH",
    ],
  },
]

function formatReviewValue(key: string, value: string, data: StepProps["data"]): string {
  const sensitiveKeys = ["WIFIPASS", "SHARE_PASSWORD", "AP_PASS", "WEB_PASSWORD",
    "TESLAFI_API_TOKEN", "TESSIE_API_TOKEN", "AWS_SECRET_ACCESS_KEY",
    "MATRIX_PASSWORD", "PUSHOVER_APP_KEY", "GOTIFY_APP_TOKEN",
    "TELEGRAM_BOT_TOKEN", "IFTTT_KEY"]
  if (sensitiveKeys.includes(key) && value) {
    return "••••••••"
  }
  if ((key === "TEMPERATURE_WARNING" || key === "TEMPERATURE_CAUTION") && value) {
    const num = parseFloat(value)
    if (!isNaN(num)) {
      if (data.TEMPERATURE_UNIT === "F") {
        return ((num * 9) / 5 + 32).toFixed(1) + "°F"
      }
      return num.toFixed(1) + "°C"
    }
  }
  if (key === "TEMPERATURE_UNIT") {
    return value === "F" ? "Fahrenheit" : "Celsius"
  }
  if (key === "RTC_BATTERY_ENABLED" || key === "RTC_TRICKLE_CHARGE") {
    return value === "true" ? "Enabled" : "Disabled"
  }
  return value
}

export function ReviewStep({ data, setupAlreadyFinished }: StepProps) {
  const configuredCount = Object.entries(data).filter(([k, v]) => !k.startsWith("_") && v && v.trim() !== "").length

  // Fields that are locked once initial setup has completed — surfaced
  // greyed-out with a "(locked)" suffix so the user understands why the
  // value isn't editable in the wizard.
  const lockedKeys = new Set(setupAlreadyFinished ? ["INCREASE_ROOT_SIZE"] : [])

  return (
    <div className="space-y-5">
      <div className="flex items-center gap-3">
        <CheckCircle className="h-5 w-5 text-emerald-400" />
        <div>
          <h3 className="text-lg font-semibold text-slate-100">Review Configuration</h3>
          <p className="text-xs text-slate-500">
            {configuredCount} settings configured. Review below then click
            &quot;Apply &amp; Run Setup&quot;.
          </p>
        </div>
      </div>

      {sections.map((section) => {
        const activeFields = section.fields.filter((f) => data[f] && data[f].trim() !== "")
        if (activeFields.length === 0) return null

        return (
          <div key={section.title} className="rounded-lg border border-white/5 bg-white/[0.02]">
            <div className="border-b border-white/5 px-4 py-2">
              <h4 className="text-xs font-semibold uppercase tracking-wider text-slate-500">
                {section.title}
              </h4>
            </div>
            <div className="divide-y divide-white/5">
              {activeFields.map((field) => {
                const locked = lockedKeys.has(field)
                return (
                  <div key={field} className="flex items-center justify-between px-4 py-2">
                    <span className={`font-mono text-xs ${locked ? "text-slate-600" : "text-slate-500"}`}>{field}</span>
                    <span className={`max-w-[200px] truncate text-right text-sm ${locked ? "text-slate-600" : "text-slate-300"}`}>
                      {formatReviewValue(field, data[field], data)}
                      {locked && <span className="ml-1 text-xs text-slate-700">(locked)</span>}
                    </span>
                  </div>
                )
              })}
            </div>
          </div>
        )
      })}

      <div className="rounded-lg border border-amber-500/20 bg-amber-500/5 px-4 py-3">
        <p className="text-sm text-amber-300">
          Clicking &quot;Apply &amp; Run Setup&quot; will save the configuration and
          start the setup process. The device may reboot during setup.
        </p>
      </div>
    </div>
  )
}
