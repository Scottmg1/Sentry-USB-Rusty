import { useEffect, type ReactNode } from "react"
import { createPortal } from "react-dom"
import { CloseIcon } from "@/components/icons"
import { cn } from "@/lib/utils"

interface ModalProps {
  title: ReactNode
  onClose: () => void
  /** Set false to suppress backdrop-click and Esc close (e.g. restart-in-progress). */
  dismissable?: boolean
  size?: "sm" | "md" | "lg"
  footer?: ReactNode
  children: ReactNode
  className?: string
}

const SIZE_MAX: Record<NonNullable<ModalProps["size"]>, string> = {
  sm: "420px",
  md: "560px",
  lg: "768px",
}

export function Modal({
  title,
  onClose,
  dismissable = true,
  size = "md",
  footer,
  children,
  className,
}: ModalProps) {
  useEffect(() => {
    if (!dismissable) return
    function onKey(e: KeyboardEvent) {
      if (e.key === "Escape") onClose()
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [dismissable, onClose])

  // Portal outside filtered/positioned ancestors that would contain the overlay.
  return createPortal(
    <div
      className="modal-shell"
      onClick={dismissable ? onClose : undefined}
      role="dialog"
      aria-modal="true"
    >
      <div
        className={cn("glass-card modal-card", className)}
        style={{ maxWidth: SIZE_MAX[size] }}
        onClick={(e) => e.stopPropagation()}
      >
        <div className="modal-header">
          <span className="modal-title">{title}</span>
          {dismissable && (
            <button className="modal-close" onClick={onClose} aria-label="Close">
              <CloseIcon className="h-4 w-4" />
            </button>
          )}
        </div>
        <div className="modal-body">{children}</div>
        {footer && (
          <div className="border-t border-white/5 px-4 py-3">{footer}</div>
        )}
      </div>
    </div>,
    document.body
  )
}
