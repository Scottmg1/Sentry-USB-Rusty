import { CardiologyIcon } from "@/components/icons"
import { useKeepAwake } from "@/hooks/useKeepAwake"
import { PrefCard } from "@/components/settings/PrefCard"
import { SegPicker } from "@/components/ui/SegPicker"

type Mode = "" | "manual" | "auto"

const OPTIONS: { value: Mode; label: string; desc: string }[] = [
  { value: "", label: "Off", desc: "Keep-awake disabled" },
  { value: "manual", label: "Manual", desc: "Button on Dashboard with duration picker" },
  { value: "auto", label: "Automatic", desc: "Stays awake while you're browsing" },
]

export function KeepAwakePreference() {
  const { mode, updateMode } = useKeepAwake()
  const current = (mode ?? "") as Mode
  const desc = OPTIONS.find((o) => o.value === current)?.desc

  return (
    <PrefCard
      icon={<CardiologyIcon className="h-3.5 w-3.5" />}
      halo="rose"
      title="Keep Awake"
    >
      <SegPicker<Mode>
        options={OPTIONS.map((o) => ({ value: o.value, label: o.label }))}
        value={current}
        onChange={(v) => updateMode(v)}
      />
      {desc && <p className="t-xs">{desc}</p>}
    </PrefCard>
  )
}
