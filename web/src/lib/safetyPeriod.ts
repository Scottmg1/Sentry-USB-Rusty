export type SafetyPeriod = "day" | "week" | "month" | "all"

export interface SafetyPeriodStorage {
  getItem(key: string): string | null
  setItem(key: string, value: string): unknown
}

const STORAGE_KEY = "sentryusb-safety-score-period"
const DEFAULT_PERIOD: SafetyPeriod = "month"

function isSafetyPeriod(value: string | null): value is SafetyPeriod {
  return value === "day" || value === "week" || value === "month" || value === "all"
}

export function browserSafetyPeriodStorage(): SafetyPeriodStorage | undefined {
  try {
    return typeof window === "undefined" ? undefined : window.localStorage
  } catch {
    return undefined
  }
}

export function loadSafetyPeriod(storage: SafetyPeriodStorage | undefined): SafetyPeriod {
  try {
    const saved = storage?.getItem(STORAGE_KEY) ?? null
    return isSafetyPeriod(saved) ? saved : DEFAULT_PERIOD
  } catch {
    return DEFAULT_PERIOD
  }
}

export function saveSafetyPeriod(
  storage: SafetyPeriodStorage | undefined,
  period: SafetyPeriod,
): void {
  try {
    storage?.setItem(STORAGE_KEY, period)
  } catch {
    // Storage can be unavailable in privacy modes; the in-memory selection still works.
  }
}
