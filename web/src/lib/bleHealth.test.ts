import assert from "node:assert/strict"
import test from "node:test"

import { presentBleHealth, type BleHealth } from "./bleHealth.ts"

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
