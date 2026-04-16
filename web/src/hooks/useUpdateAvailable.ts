import { useEffect, useState } from "react"

interface UpdateInfo {
  available: boolean
  latestVersion?: string
}

export function useUpdateAvailable(): UpdateInfo {
  const [info, setInfo] = useState<UpdateInfo>({ available: false })

  useEffect(() => {
    function check() {
      fetch("/api/system/update-status")
        .then((r) => r.json())
        .then((data) =>
          setInfo({
            available: !!data.update_available,
            latestVersion: data.latest_version || undefined,
          })
        )
        .catch(() => {})
    }

    check()
    const interval = setInterval(check, 5 * 60 * 1000)
    return () => clearInterval(interval)
  }, [])

  return info
}
