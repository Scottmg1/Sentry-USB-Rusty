import { BatteryAndroidFrameBoltIcon, BoltIcon, PowerIcon, SpeedIcon } from "@/components/icons"

import { SectionHeading, StatTile } from "@/components/drives/StatTile"

export interface ChargeDetail {
  powerKw?: number | null
  currentA?: number | null
  voltageV?: number | null
  rateMph?: number | null
  energyAddedKwh?: number | null
  limitSoc?: number | null
  rangeMi?: number | null
}

function fmt(
  v: number | null | undefined,
  suffix: string,
  digits = 0,
): string {
  if (v === null || v === undefined) return "—"
  return `${v.toFixed(digits)}${suffix}`
}

export function ChargingSection({ charge }: { charge: ChargeDetail }) {
  return (
    <>
      <SectionHeading>Charging</SectionHeading>
      <div className="grid grid-cols-2 gap-4 sm:grid-cols-3">
        <StatTile
          label="Power"
          value={fmt(charge.powerKw, " kW")}
          icon={<BoltIcon className="h-4 w-4" />}
          info="Power the charger is delivering."
        />
        <StatTile
          label="Current"
          value={fmt(charge.currentA, " A")}
          icon={<PowerIcon className="h-4 w-4" />}
          info="Actual current the charger is supplying."
        />
        <StatTile
          label="Voltage"
          value={fmt(charge.voltageV, " V")}
          icon={<BoltIcon className="h-4 w-4" />}
        />
        <StatTile
          label="Charge rate"
          value={fmt(charge.rateMph, " mi/hr")}
          icon={<SpeedIcon className="h-4 w-4" />}
          info="Range added per hour at the current rate."
        />
        <StatTile
          label="Energy added"
          value={fmt(charge.energyAddedKwh, " kWh", 1)}
          icon={<BatteryAndroidFrameBoltIcon className="h-4 w-4" />}
          info="Energy added this charging session."
        />
        <StatTile
          label="Charge limit"
          value={fmt(charge.limitSoc, "%")}
          icon={<BatteryAndroidFrameBoltIcon className="h-4 w-4" />}
          info="Target state of charge."
        />
        <StatTile
          label="Range"
          value={fmt(charge.rangeMi, " mi")}
          icon={<SpeedIcon className="h-4 w-4" />}
          info="Estimated rated range."
        />
      </div>
    </>
  )
}
