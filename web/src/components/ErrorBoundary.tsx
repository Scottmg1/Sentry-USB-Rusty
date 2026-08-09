import { Component, type ReactNode } from "react"
import { RotateRightIcon, WarningIcon } from "@/components/icons"

interface Props {
  children: ReactNode
  /** Rendered in place of children after a render error. Receives the error
   *  and a reset callback that re-attempts rendering the children. */
  fallback: (error: Error, reset: () => void) => ReactNode
}

interface State {
  error: Error | null
}

/**
 * React unmounts the entire root when a render error reaches the top of the
 * tree — on the in-car browser that's a blank page with no way back short of
 * a manual reload. Boundaries turn that into a contained fallback.
 */
export class ErrorBoundary extends Component<Props, State> {
  state: State = { error: null }

  static getDerivedStateFromError(error: Error): State {
    return { error }
  }

  reset = () => this.setState({ error: null })

  render() {
    if (this.state.error) {
      return this.props.fallback(this.state.error, this.reset)
    }
    return this.props.children
  }
}

/**
 * Card-shaped boundary for the settings grids: a crashing section degrades
 * to one broken card (with the message and a retry) while the rest of the
 * page keeps working. Sections that render several cards in a fragment
 * collapse to a single fallback card — acceptable for an error state.
 */
export function SectionErrorBoundary({ children }: { children: ReactNode }) {
  return (
    <ErrorBoundary
      fallback={(error, reset) => (
        <div className="glass-card flex flex-col gap-2 p-3.5">
          <div className="flex items-center gap-2 text-xs font-medium text-amber-400">
            <WarningIcon className="h-3.5 w-3.5 shrink-0" />
            This card hit an error
          </div>
          <p className="break-all text-[11px] text-slate-500">
            {String(error?.message ?? error)}
          </p>
          <button
            type="button"
            onClick={reset}
            className="inline-flex w-fit items-center gap-1.5 rounded-md border border-white/10 bg-white/5 px-2.5 py-1 text-xs text-slate-200 transition-colors hover:border-white/20"
          >
            <RotateRightIcon className="h-3 w-3" /> Retry
          </button>
        </div>
      )}
    >
      {children}
    </ErrorBoundary>
  )
}
