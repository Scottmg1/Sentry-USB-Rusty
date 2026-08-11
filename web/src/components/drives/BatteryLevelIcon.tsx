import {
  BatteryAndroidFrame1Icon,
  BatteryAndroidFrame2Icon,
  BatteryAndroidFrame4Icon,
  BatteryAndroidFrameFullIcon,
} from "@/components/icons"

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
    pct === undefined || pct >= 75
      ? BatteryAndroidFrameFullIcon
      : pct >= 40
        ? BatteryAndroidFrame4Icon
        : pct >= 15
          ? BatteryAndroidFrame2Icon
          : BatteryAndroidFrame1Icon
  return <Icon className={className} aria-hidden />
}
