import assert from "node:assert/strict"
import test from "node:test"

import * as chargeFormat from "./charge-format.ts"

test("live charging values use dashboard units and rounding", () => {
  const formatters = chargeFormat as typeof chargeFormat & {
    fmtCurrent: (amps: number | null | undefined) => string
    fmtVoltage: (volts: number | null | undefined) => string
    fmtChargeRateUnit: (
      mph: number | null | undefined,
      metric: boolean,
    ) => string
  }

  assert.equal(formatters.fmtCurrent(32), "32 A")
  assert.equal(formatters.fmtVoltage(239), "239 V")
  assert.equal(formatters.fmtChargeRateUnit(28.5, false), "29 mph")
  assert.equal(formatters.fmtChargeRateUnit(28.5, true), "46 km/h")
  assert.equal(chargeFormat.fmtEnergy(12.34), "12.3 kWh")
})
