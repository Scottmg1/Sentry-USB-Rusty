import { useState } from "react"

interface BarItem {
  label: string
  value: number
  color?: string
  subLabel?: string
}

interface BarChartProps {
  data: BarItem[]
  maxValue?: number
  height?: number
  showValues?: boolean
  formatValue?: (v: number) => string
  className?: string
  onBarClick?: (index: number) => void
}

export default function BarChart({
  data,
  maxValue: maxValueProp,
  height = 140,
  showValues = true,
  formatValue = (v) => `${v}`,
  className = "",
  onBarClick,
}: BarChartProps) {
  const [hovered, setHovered] = useState<number | null>(null)
  const maxValue = maxValueProp ?? Math.max(...data.map((d) => d.value), 1)
  const labelHeight = 24
  const valueHeight = showValues ? 18 : 0
  const chartHeight = height - labelHeight - valueHeight

  if (!data.length) return null

  const vbWidth = 600
  const barSlot = vbWidth / data.length
  const scaledPad = Math.min(barSlot * 0.1, 8)

  return (
    <div className={className}>
      <svg width="100%" height={height} viewBox={`0 0 ${vbWidth} ${height}`}>
        {data.map((item, i) => {
          const barH = maxValue > 0 ? (item.value / maxValue) * chartHeight : 0
          const x = i * barSlot + scaledPad
          const w = barSlot - scaledPad * 2
          const y = valueHeight + chartHeight - barH
          const color = item.color || "#3b82f6"
          const isHovered = hovered === i

          return (
            <g
              key={i}
              onMouseEnter={() => setHovered(i)}
              onMouseLeave={() => setHovered(null)}
              onClick={() => onBarClick?.(i)}
              className="cursor-pointer"
            >
              {/* Bar */}
              <rect
                x={x}
                y={y}
                width={w}
                height={Math.max(barH, item.value > 0 ? 2 : 0)}
                rx={4}
                fill={color}
                opacity={isHovered ? 1 : 0.85}
                className="transition-opacity duration-200"
              />

              {/* Value on top */}
              {showValues && item.value > 0 && (
                <text
                  x={x + w / 2}
                  y={y - 4}
                  textAnchor="middle"
                  fill={isHovered ? "#e2e8f0" : "#94a3b8"}
                  fontSize="10"
                  fontWeight="600"
                  fontFamily="Inter, system-ui, sans-serif"
                >
                  {formatValue(item.value)}
                </text>
              )}

              {/* X-axis label */}
              <text
                x={x + w / 2}
                y={height - 4}
                textAnchor="middle"
                fill={isHovered ? "#e2e8f0" : "#64748b"}
                fontSize="10"
                fontFamily="Inter, system-ui, sans-serif"
              >
                {item.label}
              </text>

              {/* Sub label (e.g., disengagement count) */}
              {item.subLabel && (
                <text
                  x={x + w / 2}
                  y={height - 14}
                  textAnchor="middle"
                  fill="#ef4444"
                  fontSize="9"
                  fontWeight="600"
                  fontFamily="Inter, system-ui, sans-serif"
                >
                  {item.subLabel}
                </text>
              )}
            </g>
          )
        })}
      </svg>
    </div>
  )
}
