interface SparklineProps {
  data: number[]
  width?: number
  height?: number
  color?: string
  showArea?: boolean
  className?: string
}

export default function Sparkline({
  data,
  width = 100,
  height = 24,
  color = "#34d399",
  showArea = true,
  className = "",
}: SparklineProps) {
  if (!data.length) return null

  const max = Math.max(...data, 1)
  const min = Math.min(...data, 0)
  const range = max - min || 1
  const padding = 2

  const points = data.map((v, i) => {
    const x = padding + (i / Math.max(data.length - 1, 1)) * (width - padding * 2)
    const y = padding + (1 - (v - min) / range) * (height - padding * 2)
    return `${x},${y}`
  })

  const pathData = `M${points.join(" L")}`
  const areaPath = `${pathData} L${width - padding},${height - padding} L${padding},${height - padding} Z`

  return (
    <svg
      width={width}
      height={height}
      viewBox={`0 0 ${width} ${height}`}
      className={className}
    >
      {showArea && (
        <path
          d={areaPath}
          fill={color}
          opacity={0.15}
        />
      )}
      <path
        d={pathData}
        fill="none"
        stroke={color}
        strokeWidth={1.5}
        strokeLinecap="round"
        strokeLinejoin="round"
      />
    </svg>
  )
}
