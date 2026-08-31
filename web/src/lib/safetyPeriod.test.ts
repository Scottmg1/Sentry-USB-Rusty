import assert from "node:assert/strict"
import test from "node:test"

import {
  loadSafetyPeriod,
  saveSafetyPeriod,
  type SafetyPeriodStorage,
} from "./safetyPeriod.ts"

function memoryStorage(): SafetyPeriodStorage {
  const values = new Map<string, string>()
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
  }
}

test("All remains selected after leaving and reopening Safety Score", () => {
  const storage = memoryStorage()

  saveSafetyPeriod(storage, "all")

  assert.equal(loadSafetyPeriod(storage), "all")
})

test("missing or invalid saved periods fall back to the 30-day view", () => {
  const empty = memoryStorage()
  assert.equal(loadSafetyPeriod(empty), "month")

  const invalid: SafetyPeriodStorage = {
    getItem: () => "year",
    setItem: () => {},
  }
  assert.equal(loadSafetyPeriod(invalid), "month")
})

test("blocked browser storage falls back safely without breaking selection", () => {
  const blocked: SafetyPeriodStorage = {
    getItem: () => {
      throw new Error("blocked")
    },
    setItem: () => {
      throw new Error("blocked")
    },
  }

  assert.equal(loadSafetyPeriod(blocked), "month")
  assert.doesNotThrow(() => saveSafetyPeriod(blocked, "all"))
})
