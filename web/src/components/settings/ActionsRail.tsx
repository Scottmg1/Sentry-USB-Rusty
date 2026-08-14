import { useEffect, useRef, useState } from "react"
import type { ReactNode, ComponentType } from "react"
import { CheckCircleIcon, ErrorIcon, ProgressActivityIcon } from "@/components/icons"
import { cn } from "@/lib/utils"

type IconType = ComponentType<{ className?: string }>

type ChipState = "idle" | "loading" | "success" | "error"

export interface ActionChipProps {
  icon: IconType
  label: ReactNode
  variant?: "default" | "danger" | "accent"
  /** Renders to the right of the label (e.g. status pill). Hidden during feedback states. */
  trailing?: ReactNode
  disabled?: boolean
  /** Return text for custom feedback, "confirm" to suppress it, or throw on failure. */
  onClick: () => void | string | Promise<void | string>
  /** Message shown on success when handler doesn't return its own string. Default: "Done". */
  successMessage?: string
  /** Fallback error message when the thrown error has none. Default: "Failed". */
  errorMessage?: string
}

function ActionChip({
  icon: Icon,
  label,
  variant = "default",
  trailing,
  disabled,
  onClick,
  successMessage = "Done",
  errorMessage = "Failed",
}: ActionChipProps) {
  const [state, setState] = useState<ChipState>("idle")
  const [feedbackMsg, setFeedbackMsg] = useState<string | null>(null)
  const timeoutRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  useEffect(
    () => () => {
      if (timeoutRef.current) clearTimeout(timeoutRef.current)
    },
    []
  )

  function scheduleRevert() {
    if (timeoutRef.current) clearTimeout(timeoutRef.current)
    timeoutRef.current = setTimeout(() => {
      setState("idle")
      setFeedbackMsg(null)
    }, 3000)
  }

  async function handleClick() {
    if (state === "loading") return
    if (timeoutRef.current) clearTimeout(timeoutRef.current)
    setState("loading")
    setFeedbackMsg(null)
    try {
      const result = await onClick()
      if (result === "confirm") {
        // The caller owns feedback for two-step confirmation.
        setState("idle")
        return
      }
      setState("success")
      setFeedbackMsg(typeof result === "string" ? result : successMessage)
      scheduleRevert()
    } catch (err) {
      setState("error")
      setFeedbackMsg(err instanceof Error && err.message ? err.message : errorMessage)
      scheduleRevert()
    }
  }

  const ActiveIcon =
    state === "loading"
      ? ProgressActivityIcon
      : state === "success"
      ? CheckCircleIcon
      : state === "error"
      ? ErrorIcon
      : Icon

  return (
    <button
      type="button"
      disabled={disabled || state === "loading"}
      onClick={handleClick}
      className={cn(
        "action-chip",
        // Feedback colors override the action variant.
        state === "idle" && variant === "danger" && "action-chip--danger",
        state === "idle" && variant === "accent" && "action-chip--accent",
        state === "loading" && "text-blue-400",
        state === "success" && "text-emerald-400",
        state === "error" && "text-red-400"
      )}
    >
      <ActiveIcon className={cn("h-3.5 w-3.5", state === "loading" && "animate-spin")} />
      <span>{feedbackMsg ?? label}</span>
      {state === "idle" && trailing}
    </button>
  )
}

interface ActionsRailProps {
  /** Non-destructive actions, left-aligned. */
  actions: ActionChipProps[]
  /** Destructive actions, right-aligned via flex spacer. */
  danger?: ActionChipProps[]
}

export function ActionsRail({ actions, danger = [] }: ActionsRailProps) {
  return (
    <div className="actions-rail">
      {actions.map((a, i) => (
        <ActionChip key={i} {...a} />
      ))}
      {danger.length > 0 && (
        <>
          <span className="sep" />
          {danger.map((a, i) => (
            <ActionChip key={`d-${i}`} {...a} variant="danger" />
          ))}
        </>
      )}
    </div>
  )
}
