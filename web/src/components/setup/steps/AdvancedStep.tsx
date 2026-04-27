import { useState, useEffect } from "react"
import { Cog, Thermometer, MapPin, Search, Battery, AlertTriangle } from "lucide-react"
import type { StepProps } from "../SetupWizard"

function Field({ label, field, type = "text", placeholder, data, onChange, hint }: {
  label: string; field: string; type?: string; placeholder?: string
  data: StepProps["data"]; onChange: StepProps["onChange"]; hint?: string
}) {
  return (
    <div>
      <label className="mb-1 block text-sm font-medium text-slate-300">{label}</label>
      <input type={type} value={data[field] ?? ""} onChange={(e) => onChange(field, e.target.value)}
        placeholder={placeholder}
        className="w-full rounded-lg border border-white/10 bg-white/5 px-3 py-2 text-sm text-slate-100 placeholder-slate-600 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25" />
      {hint && <p className="mt-1 text-xs text-slate-600">{hint}</p>}
    </div>
  )
}

const TIMEZONES = [
  "auto",
  // US: prefer the canonical IANA names below — newer Pi OS / Debian
  // releases ship a minimal tzdata that drops the legacy `US/*` aliases,
  // which would make `timedatectl set-timezone` fail. Our backend
  // normalizes US/Eastern → America/New_York etc. for older saved
  // configs, but new installs should pick the canonical name directly.
  // Americas
  "America/Adak", "America/Anchorage", "America/Anguilla", "America/Antigua", "America/Araguaina",
  "America/Argentina/Buenos_Aires", "America/Argentina/Catamarca", "America/Argentina/Cordoba",
  "America/Argentina/Jujuy", "America/Argentina/La_Rioja", "America/Argentina/Mendoza",
  "America/Argentina/Rio_Gallegos", "America/Argentina/Salta", "America/Argentina/San_Juan",
  "America/Argentina/San_Luis", "America/Argentina/Tucuman", "America/Argentina/Ushuaia",
  "America/Aruba", "America/Asuncion", "America/Atikokan", "America/Bahia", "America/Bahia_Banderas",
  "America/Barbados", "America/Belem", "America/Belize", "America/Blanc-Sablon", "America/Boa_Vista",
  "America/Bogota", "America/Boise", "America/Cambridge_Bay", "America/Campo_Grande",
  "America/Cancun", "America/Caracas", "America/Cayenne", "America/Cayman", "America/Chicago",
  "America/Chihuahua", "America/Ciudad_Juarez", "America/Costa_Rica", "America/Creston",
  "America/Cuiaba", "America/Curacao", "America/Danmarkshavn", "America/Dawson", "America/Dawson_Creek",
  "America/Denver", "America/Detroit", "America/Dominica", "America/Edmonton", "America/Eirunepe",
  "America/El_Salvador", "America/Fort_Nelson", "America/Fortaleza", "America/Glace_Bay",
  "America/Goose_Bay", "America/Grand_Turk", "America/Grenada", "America/Guadeloupe",
  "America/Guatemala", "America/Guayaquil", "America/Guyana", "America/Halifax", "America/Havana",
  "America/Hermosillo", "America/Indiana/Indianapolis", "America/Indiana/Knox",
  "America/Indiana/Marengo", "America/Indiana/Petersburg", "America/Indiana/Tell_City",
  "America/Indiana/Vevay", "America/Indiana/Vincennes", "America/Indiana/Winamac",
  "America/Inuvik", "America/Iqaluit", "America/Jamaica", "America/Juneau",
  "America/Kentucky/Louisville", "America/Kentucky/Monticello", "America/Kralendijk",
  "America/La_Paz", "America/Lima", "America/Los_Angeles", "America/Lower_Princes",
  "America/Maceio", "America/Managua", "America/Manaus", "America/Marigot", "America/Martinique",
  "America/Matamoros", "America/Mazatlan", "America/Menominee", "America/Merida",
  "America/Metlakatla", "America/Mexico_City", "America/Miquelon", "America/Moncton",
  "America/Monterrey", "America/Montevideo", "America/Montserrat", "America/Nassau",
  "America/New_York", "America/Nome", "America/Noronha", "America/North_Dakota/Beulah",
  "America/North_Dakota/Center", "America/North_Dakota/New_Salem", "America/Nuuk",
  "America/Ojinaga", "America/Panama", "America/Paramaribo", "America/Phoenix",
  "America/Port-au-Prince", "America/Port_of_Spain", "America/Porto_Velho",
  "America/Puerto_Rico", "America/Punta_Arenas", "America/Rankin_Inlet", "America/Recife",
  "America/Regina", "America/Resolute", "America/Rio_Branco", "America/Santarem",
  "America/Santiago", "America/Santo_Domingo", "America/Sao_Paulo", "America/Scoresbysund",
  "America/Sitka", "America/St_Barthelemy", "America/St_Johns", "America/St_Kitts",
  "America/St_Lucia", "America/St_Thomas", "America/St_Vincent", "America/Swift_Current",
  "America/Tegucigalpa", "America/Thule", "America/Tijuana", "America/Toronto",
  "America/Tortola", "America/Vancouver", "America/Whitehorse", "America/Winnipeg",
  "America/Yakutat",
  // Europe
  "Europe/Amsterdam", "Europe/Andorra", "Europe/Astrakhan", "Europe/Athens", "Europe/Belgrade",
  "Europe/Berlin", "Europe/Bratislava", "Europe/Brussels", "Europe/Bucharest", "Europe/Budapest",
  "Europe/Busingen", "Europe/Chisinau", "Europe/Copenhagen", "Europe/Dublin", "Europe/Gibraltar",
  "Europe/Guernsey", "Europe/Helsinki", "Europe/Isle_of_Man", "Europe/Istanbul", "Europe/Jersey",
  "Europe/Kaliningrad", "Europe/Kirov", "Europe/Kyiv", "Europe/Lisbon", "Europe/Ljubljana",
  "Europe/London", "Europe/Luxembourg", "Europe/Madrid", "Europe/Malta", "Europe/Mariehamn",
  "Europe/Minsk", "Europe/Monaco", "Europe/Moscow", "Europe/Oslo", "Europe/Paris",
  "Europe/Podgorica", "Europe/Prague", "Europe/Riga", "Europe/Rome", "Europe/Samara",
  "Europe/San_Marino", "Europe/Sarajevo", "Europe/Saratov", "Europe/Simferopol", "Europe/Skopje",
  "Europe/Sofia", "Europe/Stockholm", "Europe/Tallinn", "Europe/Tirane", "Europe/Ulyanovsk",
  "Europe/Vaduz", "Europe/Vatican", "Europe/Vienna", "Europe/Vilnius", "Europe/Volgograd",
  "Europe/Warsaw", "Europe/Zagreb", "Europe/Zurich",
  // Asia
  "Asia/Aden", "Asia/Almaty", "Asia/Amman", "Asia/Anadyr", "Asia/Aqtau", "Asia/Aqtobe",
  "Asia/Ashgabat", "Asia/Atyrau", "Asia/Baghdad", "Asia/Bahrain", "Asia/Baku", "Asia/Bangkok",
  "Asia/Barnaul", "Asia/Beirut", "Asia/Bishkek", "Asia/Brunei", "Asia/Chita", "Asia/Colombo",
  "Asia/Damascus", "Asia/Dhaka", "Asia/Dili", "Asia/Dubai", "Asia/Dushanbe", "Asia/Famagusta",
  "Asia/Gaza", "Asia/Hebron", "Asia/Ho_Chi_Minh", "Asia/Hong_Kong", "Asia/Hovd", "Asia/Irkutsk",
  "Asia/Jakarta", "Asia/Jayapura", "Asia/Jerusalem", "Asia/Kabul", "Asia/Kamchatka",
  "Asia/Karachi", "Asia/Kathmandu", "Asia/Khandyga", "Asia/Kolkata", "Asia/Krasnoyarsk",
  "Asia/Kuala_Lumpur", "Asia/Kuching", "Asia/Kuwait", "Asia/Macau", "Asia/Magadan",
  "Asia/Makassar", "Asia/Manila", "Asia/Muscat", "Asia/Nicosia", "Asia/Novokuznetsk",
  "Asia/Novosibirsk", "Asia/Omsk", "Asia/Oral", "Asia/Phnom_Penh", "Asia/Pontianak",
  "Asia/Pyongyang", "Asia/Qatar", "Asia/Qostanay", "Asia/Qyzylorda", "Asia/Riyadh",
  "Asia/Sakhalin", "Asia/Samarkand", "Asia/Seoul", "Asia/Shanghai", "Asia/Singapore",
  "Asia/Srednekolymsk", "Asia/Taipei", "Asia/Tashkent", "Asia/Tbilisi", "Asia/Tehran",
  "Asia/Thimphu", "Asia/Tokyo", "Asia/Tomsk", "Asia/Ulaanbaatar", "Asia/Urumqi",
  "Asia/Ust-Nera", "Asia/Vientiane", "Asia/Vladivostok", "Asia/Yakutsk", "Asia/Yangon",
  "Asia/Yekaterinburg", "Asia/Yerevan",
  // Africa
  "Africa/Abidjan", "Africa/Accra", "Africa/Addis_Ababa", "Africa/Algiers", "Africa/Asmara",
  "Africa/Bamako", "Africa/Bangui", "Africa/Banjul", "Africa/Bissau", "Africa/Blantyre",
  "Africa/Brazzaville", "Africa/Bujumbura", "Africa/Cairo", "Africa/Casablanca", "Africa/Ceuta",
  "Africa/Conakry", "Africa/Dakar", "Africa/Dar_es_Salaam", "Africa/Djibouti", "Africa/Douala",
  "Africa/El_Aaiun", "Africa/Freetown", "Africa/Gaborone", "Africa/Harare", "Africa/Johannesburg",
  "Africa/Juba", "Africa/Kampala", "Africa/Khartoum", "Africa/Kigali", "Africa/Kinshasa",
  "Africa/Lagos", "Africa/Libreville", "Africa/Lome", "Africa/Luanda", "Africa/Lubumbashi",
  "Africa/Lusaka", "Africa/Malabo", "Africa/Maputo", "Africa/Maseru", "Africa/Mbabane",
  "Africa/Mogadishu", "Africa/Monrovia", "Africa/Nairobi", "Africa/Ndjamena", "Africa/Niamey",
  "Africa/Nouakchott", "Africa/Ouagadougou", "Africa/Porto-Novo", "Africa/Sao_Tome",
  "Africa/Tripoli", "Africa/Tunis", "Africa/Windhoek",
  // Australia & Pacific
  "Australia/Adelaide", "Australia/Brisbane", "Australia/Broken_Hill", "Australia/Darwin",
  "Australia/Eucla", "Australia/Hobart", "Australia/Lindeman", "Australia/Lord_Howe",
  "Australia/Melbourne", "Australia/Perth", "Australia/Sydney",
  "Pacific/Apia", "Pacific/Auckland", "Pacific/Bougainville", "Pacific/Chatham",
  "Pacific/Chuuk", "Pacific/Easter", "Pacific/Efate", "Pacific/Fakaofo", "Pacific/Fiji",
  "Pacific/Funafuti", "Pacific/Galapagos", "Pacific/Gambier", "Pacific/Guadalcanal",
  "Pacific/Guam", "Pacific/Honolulu", "Pacific/Kanton", "Pacific/Kiritimati", "Pacific/Kosrae",
  "Pacific/Kwajalein", "Pacific/Majuro", "Pacific/Marquesas", "Pacific/Midway", "Pacific/Nauru",
  "Pacific/Niue", "Pacific/Norfolk", "Pacific/Noumea", "Pacific/Pago_Pago", "Pacific/Palau",
  "Pacific/Pitcairn", "Pacific/Pohnpei", "Pacific/Port_Moresby", "Pacific/Rarotonga",
  "Pacific/Saipan", "Pacific/Tahiti", "Pacific/Tarawa", "Pacific/Tongatapu", "Pacific/Wake",
  "Pacific/Wallis",
  // Indian Ocean
  "Indian/Antananarivo", "Indian/Chagos", "Indian/Christmas", "Indian/Cocos", "Indian/Comoro",
  "Indian/Kerguelen", "Indian/Mahe", "Indian/Maldives", "Indian/Mauritius", "Indian/Mayotte",
  "Indian/Reunion",
  // Atlantic
  "Atlantic/Azores", "Atlantic/Bermuda", "Atlantic/Canary", "Atlantic/Cape_Verde",
  "Atlantic/Faroe", "Atlantic/Madeira", "Atlantic/Reykjavik", "Atlantic/South_Georgia",
  "Atlantic/St_Helena", "Atlantic/Stanley",
  // Arctic / Antarctica
  "Arctic/Longyearbyen",
  "Antarctica/Casey", "Antarctica/Davis", "Antarctica/DumontDUrville", "Antarctica/Macquarie",
  "Antarctica/Mawson", "Antarctica/McMurdo", "Antarctica/Palmer", "Antarctica/Rothera",
  "Antarctica/Syowa", "Antarctica/Troll", "Antarctica/Vostok",
  // UTC
  "Etc/UTC", "Etc/GMT",
  "Etc/GMT+1", "Etc/GMT+2", "Etc/GMT+3", "Etc/GMT+4", "Etc/GMT+5", "Etc/GMT+6",
  "Etc/GMT+7", "Etc/GMT+8", "Etc/GMT+9", "Etc/GMT+10", "Etc/GMT+11", "Etc/GMT+12",
  "Etc/GMT-1", "Etc/GMT-2", "Etc/GMT-3", "Etc/GMT-4", "Etc/GMT-5", "Etc/GMT-6",
  "Etc/GMT-7", "Etc/GMT-8", "Etc/GMT-9", "Etc/GMT-10", "Etc/GMT-11", "Etc/GMT-12",
  "Etc/GMT-13", "Etc/GMT-14",
]

function TempInput({
  label,
  field,
  data,
  onChange,
  placeholder,
  useFahrenheit,
}: {
  label: string
  field: string
  data: StepProps["data"]
  onChange: StepProps["onChange"]
  placeholder: string
  useFahrenheit: boolean
}) {
  // Convert from milli-°C stored value to display value
  const raw = data[field] ?? ""
  let displayVal = raw
  // If value looks like milli-°C (>=1000), convert for display
  if (raw && parseInt(raw) >= 1000) {
    const celsius = parseInt(raw) / 1000
    displayVal = useFahrenheit
      ? ((celsius * 9 / 5) + 32).toFixed(1)
      : celsius.toFixed(1)
  } else if (raw && useFahrenheit && parseFloat(raw) > 0) {
    // Already in °C display format, convert to °F
    displayVal = ((parseFloat(raw) * 9 / 5) + 32).toFixed(1)
  }

  const unit = useFahrenheit ? "°F" : "°C"

  return (
    <div>
      <label className="mb-1 block text-sm font-medium text-slate-300">{label}</label>
      <div className="relative">
        <input
          type="text"
          inputMode="decimal"
          value={displayVal}
          onChange={(e) => {
            const v = e.target.value.replace(/[^0-9.]/g, "")
            // Store as °C (will be converted to milli-°C on save)
            if (useFahrenheit && v) {
              const f = parseFloat(v)
              if (!isNaN(f)) {
                onChange(field, ((f - 32) * 5 / 9).toFixed(1))
                return
              }
            }
            onChange(field, v)
          }}
          placeholder={placeholder}
          className="w-full rounded-lg border border-white/10 bg-white/5 px-3 py-2 pr-10 text-sm text-slate-100 placeholder-slate-600 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"
        />
        <span className="absolute right-3 top-1/2 -translate-y-1/2 text-xs font-medium text-slate-500">{unit}</span>
      </div>
    </div>
  )
}

export function AdvancedStep({ data, onChange }: StepProps) {
  const [tzSearch, setTzSearch] = useState("")
  const [isPi5, setIsPi5] = useState(false)
  const useFahrenheit = data.TEMPERATURE_UNIT === "F"

  useEffect(() => {
    fetch("/api/status")
      .then((r) => r.json())
      .then((s) => {
        if (s.sbc_model && s.sbc_model.includes("Raspberry Pi 5")) {
          setIsPi5(true)
        }
      })
      .catch(() => {})
  }, [])

  const filteredTz = tzSearch
    ? TIMEZONES.filter(tz => tz.toLowerCase().includes(tzSearch.toLowerCase()))
    : TIMEZONES

  return (
    <div className="space-y-6">
      {/* Timezone */}
      <div>
        <label className="mb-1 block text-sm font-medium text-slate-300">Time Zone</label>
        <div className="relative mb-2">
          <Search className="absolute left-3 top-2.5 h-3.5 w-3.5 text-slate-600" />
          <input
            type="text"
            value={tzSearch}
            onChange={(e) => setTzSearch(e.target.value)}
            placeholder="Search timezones..."
            className="w-full rounded-lg border border-white/10 bg-white/5 py-2 pl-9 pr-3 text-sm text-slate-100 placeholder-slate-600 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25"
          />
        </div>
        <select
          value={data.TIME_ZONE ?? "auto"}
          onChange={(e) => onChange("TIME_ZONE", e.target.value)}
          size={6}
          className="w-full rounded-lg border border-white/10 bg-slate-900 px-3 py-2 text-sm text-slate-100 outline-none transition focus:border-blue-500/50 focus:ring-1 focus:ring-blue-500/25 [&>option]:bg-slate-900 [&>option]:text-slate-100 [&>option:checked]:bg-blue-600"
        >
          {filteredTz.map(tz => (
            <option key={tz} value={tz}>{tz === "auto" ? "auto (detect automatically)" : tz}</option>
          ))}
        </select>
        <p className="mt-1 text-xs text-slate-600">
          Selected: <span className="font-medium text-blue-400">{data.TIME_ZONE || "auto"}</span>
        </p>
      </div>

      {/* Archive tuning */}
      <div>
        <div className="mb-3 flex items-center gap-2">
          <Cog className="h-4 w-4 text-blue-400" />
          <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
            Archive Tuning
          </h3>
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Archive Delay (seconds)" field="ARCHIVE_DELAY" placeholder="20"
            data={data} onChange={onChange} hint="Delay between WiFi connect and archiving start" />
          <Field label="Snapshot Interval (seconds)" field="SNAPSHOT_INTERVAL" placeholder="default"
            data={data} onChange={onChange} hint="Set ~2 min shorter than car's RecentClips retention" />
        </div>
      </div>

      {/* Temperature monitoring */}
      <div>
        <div className="mb-3 flex items-center justify-between">
          <div className="flex items-center gap-2">
            <Thermometer className="h-4 w-4 text-blue-400" />
            <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
              Temperature Monitoring
            </h3>
          </div>
          <div className="flex overflow-hidden rounded-lg border border-white/10">
            <button type="button" onClick={() => onChange("TEMPERATURE_UNIT", "C")}
              className={`px-2.5 py-1 text-xs font-medium transition-colors ${!useFahrenheit ? "bg-blue-500 text-white" : "text-slate-500 hover:text-slate-300"}`}>
              °C
            </button>
            <button type="button" onClick={() => onChange("TEMPERATURE_UNIT", "F")}
              className={`px-2.5 py-1 text-xs font-medium transition-colors ${useFahrenheit ? "bg-blue-500 text-white" : "text-slate-500 hover:text-slate-300"}`}>
              °F
            </button>
          </div>
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          <TempInput label="Warning Threshold" field="TEMPERATURE_WARNING"
            placeholder={useFahrenheit ? "154.4" : "68.0"} data={data} onChange={onChange} useFahrenheit={useFahrenheit} />
          <TempInput label="Caution Threshold" field="TEMPERATURE_CAUTION"
            placeholder={useFahrenheit ? "131.0" : "55.0"} data={data} onChange={onChange} useFahrenheit={useFahrenheit} />
          <Field label="Log Interval (minutes)" field="TEMPERATURE_INTERVAL" placeholder="60"
            data={data} onChange={onChange} hint="How often to log temperature readings" />
        </div>
        <label className="mt-3 flex cursor-pointer items-center gap-2">
          <input type="checkbox" checked={(data.TEMPERATURE_POSTARCHIVE ?? "true") === "true"}
            onChange={(e) => onChange("TEMPERATURE_POSTARCHIVE", e.target.checked ? "true" : "false")}
            className="h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500" />
          <span className="text-sm text-slate-300">Log temperature after each archive</span>
        </label>
      </div>

      {/* RTC Battery (Pi 5 only) */}
      {isPi5 && (
        <div>
          <div className="mb-3 flex items-center gap-2">
            <Battery className="h-4 w-4 text-blue-400" />
            <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
              RTC Battery
            </h3>
            <span className="rounded-full bg-blue-500/15 px-2 py-0.5 text-[10px] font-medium text-blue-400">
              Pi 5
            </span>
          </div>
          <p className="mb-3 text-xs text-slate-500">
            The Raspberry Pi 5 has a built-in real-time clock. With a battery on the J5 header,
            your Pi maintains accurate time even without network access.
          </p>
          <label className="flex cursor-pointer items-center gap-2">
            <input type="checkbox" checked={data.RTC_BATTERY_ENABLED === "true"}
              onChange={(e) => onChange("RTC_BATTERY_ENABLED", e.target.checked ? "true" : "false")}
              className="h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500" />
            <span className="text-sm text-slate-300">Enable RTC Battery support</span>
          </label>
          {data.RTC_BATTERY_ENABLED === "true" && (
            <>
              <div className="mt-3 rounded-lg border border-amber-500/20 bg-amber-500/5 p-4">
                <div className="flex items-start gap-3">
                  <AlertTriangle className="mt-0.5 h-5 w-5 shrink-0 text-amber-400" />
                  <div>
                    <p className="text-sm font-semibold text-amber-200">Hardware Required</p>
                    <p className="mt-1 text-xs text-slate-400">
                      You <strong className="text-amber-300">must</strong> have an RTC battery physically
                      connected to the J5 header on your Raspberry Pi 5 before enabling this option.
                    </p>
                    <p className="mt-2 text-xs text-slate-400">
                      Without a battery installed, your Pi will lose accurate time on every power
                      loss — worse than the default behavior.
                    </p>
                    <ul className="mt-2 space-y-1 text-xs text-slate-400">
                      <li>• Disables fake-hwclock (software time persistence)</li>
                      <li>• Enables hardware clock sync using the Pi 5's built-in RTC</li>
                      <li>• Maintains accurate time even without network access</li>
                    </ul>
                  </div>
                </div>
              </div>

              <label className="mt-4 flex cursor-pointer items-center gap-2">
                <input type="checkbox" checked={data.RTC_TRICKLE_CHARGE === "true"}
                  onChange={(e) => {
                    if (!e.target.checked) {
                      onChange("RTC_TRICKLE_CHARGE", "false")
                      onChange("_RTC_TRICKLE_ACK", "false")
                    } else {
                      onChange("RTC_TRICKLE_CHARGE", data._RTC_TRICKLE_ACK === "true" ? "true" : "false")
                    }
                    onChange("_RTC_TRICKLE_TOGGLE", e.target.checked ? "true" : "false")
                  }}
                  className="h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500" />
                <span className="text-sm text-slate-300">Enable trickle charging</span>
              </label>

              {data._RTC_TRICKLE_TOGGLE === "true" && (
                <div className="mt-3 rounded-lg border border-red-500/30 bg-red-500/10 p-4">
                  <div className="flex items-start gap-3">
                    <AlertTriangle className="mt-0.5 h-5 w-5 shrink-0 text-red-400" />
                    <div>
                      <p className="text-sm font-semibold text-red-300">Rechargeable Battery Required</p>
                      <p className="mt-1 text-xs text-slate-400">
                        Trickle charging is <strong className="text-red-300">ONLY</strong> safe with rechargeable
                        batteries (ML-2020, ML-2032, LIR2032). Using this with a standard non-rechargeable CR2032
                        battery may cause the battery to leak, rupture, or damage your Raspberry Pi.
                      </p>
                      <label className="mt-3 flex cursor-pointer items-start gap-2">
                        <input type="checkbox" checked={data._RTC_TRICKLE_ACK === "true"}
                          onChange={(e) => {
                            onChange("_RTC_TRICKLE_ACK", e.target.checked ? "true" : "false")
                            onChange("RTC_TRICKLE_CHARGE", e.target.checked ? "true" : "false")
                          }}
                          className="mt-0.5 h-4 w-4 rounded border-white/20 bg-white/5 accent-red-500" />
                        <span className="text-xs text-slate-300">
                          I confirm my RTC battery is rechargeable and accept all risk. Sentry-USB assumes no
                          responsibility for damage caused by enabling trickle charging with an incompatible battery.
                        </span>
                      </label>
                    </div>
                  </div>
                </div>
              )}
            </>
          )}
        </div>
      )}

      {/* System tuning */}
      <div>
        <div className="mb-3 flex items-center gap-2">
          <Cog className="h-4 w-4 text-blue-400" />
          <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
            System Tuning
          </h3>
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="Increase Root Size" field="INCREASE_ROOT_SIZE" placeholder="e.g. 500M or 2G"
            data={data} onChange={onChange} hint="Extra space for packages — must include suffix M or G. Applied during the root-shrink phase only; once shrink completes, changing this requires a reflash." />
          <Field label="Additional Packages" field="INSTALL_USER_REQUESTED_PACKAGES" placeholder="iftop mosh sysstat"
            data={data} onChange={onChange} hint="Space-separated list of apt packages" />
          <Field label="CPU Governor" field="CPU_GOVERNOR" placeholder="conservative"
            data={data} onChange={onChange} hint="Leave empty for Sentry USB defaults" />
          <Field label="Dirty Background Bytes" field="DIRTY_BACKGROUND_BYTES" placeholder="65536"
            data={data} onChange={onChange} hint="VM write-back tuning. Leave empty for defaults." />
        </div>
      </div>

      {/* Drive Map */}
      <div>
        <div className="mb-3 flex items-center gap-2">
          <MapPin className="h-4 w-4 text-blue-400" />
          <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
            Drive Map
          </h3>
        </div>
        <p className="mb-3 text-xs text-slate-500">
          Automatically extract GPS data from dashcam clips after archiving and build a map of all your drives.
        </p>
        <label className="flex cursor-pointer items-center gap-2">
          <input type="checkbox" checked={(data.DRIVE_MAP_ENABLED ?? "true") === "true"}
            onChange={(e) => onChange("DRIVE_MAP_ENABLED", e.target.checked ? "true" : "false")}
            className="h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500" />
          <span className="text-sm text-slate-300">Enable drive map processing after archive</span>
        </label>
        {(data.DRIVE_MAP_ENABLED ?? "true") === "true" && (
          <>
            <label className="mt-2 flex cursor-pointer items-center gap-2">
              <input type="checkbox" checked={(data.DRIVE_MAP_WHILE_AWAY ?? "true") === "true"}
                onChange={(e) => onChange("DRIVE_MAP_WHILE_AWAY", e.target.checked ? "true" : "false")}
                className="h-4 w-4 rounded border-white/20 bg-white/5 accent-blue-500" />
              <span className="text-sm text-slate-300">Map drives while away</span>
            </label>
            <p className="ml-6 text-xs text-slate-600">
              Process new clips after each snapshot while the car is away. Reduces processing time when you arrive home.
              Disable if you experience overheating issues.
            </p>
          </>
        )}
        <div className="mt-3">
          <label className="mb-1 block text-sm font-medium text-slate-300">Distance Unit</label>
          <div className="flex overflow-hidden rounded-lg border border-white/10 w-fit">
            <button type="button" onClick={() => onChange("DRIVE_MAP_UNIT", "mi")}
              className={`px-3 py-1.5 text-xs font-medium transition-colors ${(data.DRIVE_MAP_UNIT ?? "mi") === "mi" ? "bg-blue-500 text-white" : "text-slate-500 hover:text-slate-300"}`}>
              Miles
            </button>
            <button type="button" onClick={() => onChange("DRIVE_MAP_UNIT", "km")}
              className={`px-3 py-1.5 text-xs font-medium transition-colors ${data.DRIVE_MAP_UNIT === "km" ? "bg-blue-500 text-white" : "text-slate-500 hover:text-slate-300"}`}>
              Kilometers
            </button>
          </div>
        </div>
      </div>

      {/* Source */}
      <div>
        <div className="mb-3 flex items-center gap-2">
          <Cog className="h-4 w-4 text-blue-400" />
          <h3 className="text-sm font-semibold uppercase tracking-wider text-slate-400">
            Update Source
          </h3>
        </div>
        <div className="grid gap-3 sm:grid-cols-2">
          <Field label="GitHub Repo" field="REPO" placeholder="Sentry-Six"
            data={data} onChange={onChange} hint="GitHub user/org for update scripts" />
          <Field label="Branch" field="BRANCH" placeholder="main"
            data={data} onChange={onChange} />
        </div>
      </div>
    </div>
  )
}
