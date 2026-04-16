import { type ClassValue, clsx } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

export function formatUptime(seconds: number): string {
  const days = Math.trunc(seconds / (24 * 3600))
  const hours = Math.trunc((seconds % (24 * 3600)) / 3600)
  const minutes = Math.trunc((seconds % 3600) / 60)
  const secs = Math.trunc(seconds % 60)
  let out = ""
  if (days === 1) out = "1 day, "
  else if (days > 1) out = `${days} days, `
  return (
    out +
    hours.toString().padStart(2, "0") +
    ":" +
    minutes.toString().padStart(2, "0") +
    ":" +
    secs.toString().padStart(2, "0")
  )
}

export function formatBytes(bytes: number): string {
  if (bytes > 1024 * 1024 * 1024) {
    return (bytes / (1024 * 1024 * 1024)).toFixed(0) + " GB"
  } else if (bytes > 100 * 1024 * 1024) {
    return (bytes / (1024 * 1024 * 1024)).toFixed(1) + " GB"
  }
  return (bytes / (1024 * 1024)).toFixed(0) + " MB"
}

export function formatTemp(milliCelsius: number, useFahrenheit = false): string {
  const celsius = milliCelsius / 1000
  if (useFahrenheit) {
    return ((celsius * 9) / 5 + 32).toFixed(1) + "°F"
  }
  return celsius.toFixed(1) + "°C"
}
