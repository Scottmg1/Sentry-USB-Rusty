import {
  BatteryAndroidFrame1Icon,
  BatteryAndroidFrame2Icon,
  BatteryAndroidFrame3Icon,
  BatteryAndroidFrame4Icon,
  BatteryAndroidFrame5Icon,
  BatteryAndroidFrame6Icon,
  BatteryAndroidFrameFullIcon,
} from "@/components/icons"

// The full battery_android_frame ladder. Material ships six partial fills
// plus full, so the range splits into seven even bands of ~14.3%.
const STEPS = [
  BatteryAndroidFrame1Icon,
  BatteryAndroidFrame2Icon,
  BatteryAndroidFrame3Icon,
  BatteryAndroidFrame4Icon,
  BatteryAndroidFrame5Icon,
  BatteryAndroidFrame6Icon,
]

/**
 * Battery glyph that tracks the state of charge shown beside it, so a 20%
 * readout doesn't sit next to a full battery. Falls back to full when the
 * percentage is unknown, matching how the rest of the UI renders missing
 * telemetry.
 */
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
