import { useState } from "react"
import { Eye, EyeOff } from "lucide-react"

interface SecretInputProps {
  value: string
  onChange: (value: string) => void
  placeholder?: string
  className?: string
}

export function SecretInput({ value, onChange, placeholder, className }: SecretInputProps) {
  const [visible, setVisible] = useState(false)

  return (
    <div className="relative">
      <input
        type={visible ? "text" : "password"}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        placeholder={placeholder}
        className={className}
      />
      <button
        type="button"
        tabIndex={-1}
        onClick={() => setVisible((v) => !v)}
        className="absolute right-2 top-1/2 -translate-y-1/2 rounded p-1 text-slate-500 transition-colors hover:text-slate-300"
      >
        {visible ? <EyeOff className="h-3.5 w-3.5" /> : <Eye className="h-3.5 w-3.5" />}
      </button>
    </div>
  )
}
