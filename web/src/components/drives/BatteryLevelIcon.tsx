import {
  BatteryAndroidFrame1Icon,
  BatteryAndroidFrame2Icon,
  BatteryAndroidFrame3Icon,
  BatteryAndroidFrame4Icon,
  BatteryAndroidFrame5Icon,
  BatteryAndroidFrame6Icon,
  BatteryAndroidFrameFullIcon,
} from "@/components/icons"

// Seven glyphs divide charge levels into equal bands.
const STEPS = [
  BatteryAndroidFrame1Icon,
  BatteryAndroidFrame2Icon,
  BatteryAndroidFrame3Icon,
  BatteryAndroidFrame4Icon,
  BatteryAndroidFrame5Icon,
  BatteryAndroidFrame6Icon,
]

/** Battery glyph selected by state of charge; unknown values use the full icon. */
export function BatteryLevelIcon({
  pct,
  className = "h-4 w-4",
}: {
  pct?: number
  className?: string
}) {
  const Icon =
    pct === undefined || pct >= (100 * 6) / 7
      ? BatteryAndroidFrameFullIcon
      : STEPS[Math.max(0, Math.min(5, Math.floor((pct / 100) * 7)))]
  return <Icon className={className} aria-hidden />
}
