import assert from "node:assert/strict"
import test from "node:test"

import { presentBleHealth, type BleHealth } from "./bleHealth.ts"
import * as bleHealthModule from "./bleHealth.ts"

const repairRequired: BleHealth = {
  severity: "red",
  code: "repair_required",
  since_ts: 1_723_000_000,
  label: "Re-pair required",
  guidance:
    "Open Settings, select Re-pair, then tap your key card on the center console.",
}

test("confirmed key rejection stays red and gives the re-pair action", () => {
  assert.deepEqual(presentBleHealth(repairRequired, 0), {
    ...repairRequired,
    repairRequired: true,
  })
})

test("missing backend health falls back to yellow idle instead of red", () => {
  const got = presentBleHealth(null, 86_400)
  assert.equal(got.severity, "yellow")
  assert.equal(got.code, "idle")
  assert.equal(got.repairRequired, false)
})

test("malformed backend health cannot invent a red fault", () => {
  const got = presentBleHealth({ severity: "red" }, 120)
  assert.equal(got.severity, "yellow")
  assert.equal(got.code, "delayed")
  assert.equal(got.repairRequired, false)
})

test("fresh fallback remains connected", () => {
  const got = presentBleHealth(undefined, 15)
  assert.equal(got.severity, "green")
  assert.equal(got.code, "connected")
})

test("fresh stationary telemetry is labeled Idle, never Asleep", () => {
  const deriveVehicleStatusLabel = (
    bleHealthModule as typeof bleHealthModule & {
      deriveVehicleStatusLabel: (
        health: ReturnType<typeof presentBleHealth>,
        isDriving: boolean,
        charging: boolean,
      ) => string
    }
  ).deriveVehicleStatusLabel

  const connected = presentBleHealth(undefined, 15)
  assert.equal(deriveVehicleStatusLabel(connected, false, false), "Idle")
})

test("charging is shown only while BLE health is green", () => {
  const deriveVehicleStatusLabel = (
    bleHealthModule as typeof bleHealthModule & {
      deriveVehicleStatusLabel: (
        health: ReturnType<typeof presentBleHealth>,
        isDriving: boolean,
        charging: boolean,
      ) => string
    }
  ).deriveVehicleStatusLabel

  assert.equal(
    deriveVehicleStatusLabel(presentBleHealth(undefined, 15), false, true),
    "Charging",
  )
  assert.equal(
    deriveVehicleStatusLabel(presentBleHealth(undefined, 86_400), false, true),
    "Telemetry idle",
  )
})

test("charging controls require both green BLE health and backend availability", () => {
  const shouldShowChargingControls = (
    bleHealthModule as typeof bleHealthModule & {
      shouldShowChargingControls: (
        health: ReturnType<typeof presentBleHealth>,
        controlsAvailable: boolean,
        controlsValidUntilTs: number | null,
        nowTs: number,
      ) => boolean
    }
  ).shouldShowChargingControls

  assert.equal(
    shouldShowChargingControls(presentBleHealth(undefined, 15), true, 1_120, 1_060),
    true,
  )
  assert.equal(
    shouldShowChargingControls(presentBleHealth(undefined, 600), true, 1_120, 1_060),
    false,
  )
  assert.equal(
    shouldShowChargingControls(presentBleHealth(undefined, 15), false, 1_120, 1_060),
    false,
  )
  assert.equal(
    shouldShowChargingControls(presentBleHealth(undefined, 15), true, 1_120, 1_121),
    false,
  )
})
