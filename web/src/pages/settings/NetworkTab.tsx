import { LanIcon, WifiIcon } from "@/components/icons"
import { PrefCard } from "@/components/settings/PrefCard"
import { SectionErrorBoundary } from "@/components/ErrorBoundary"
import { Row } from "@/components/ui/StatusTile"
import { Pill } from "@/components/ui/Pill"
import CloudPairingSection from "@/components/CloudPairingSection"
import { AwayModeControl } from "@/components/settings/sections/AwayModeControl"
import { BleEnableToggle } from "@/components/settings/sections/BleEnableToggle"
import { BlePairButton } from "@/components/settings/sections/BlePairButton"
import type { PiStatus } from "@/lib/api"

interface Props {
  status: PiStatus | null
  /** Forwarded to AwayModeControl so its disabled-state CTA can re-launch
   *  the Setup Wizard. */
  onOpenWizard?: () => void
  openTravelMode?: boolean
  onTravelModeClose?: () => void
}

export function NetworkTab({ status, onOpenWizard, openTravelMode, onTravelModeClose }: Props) {
  const wifiConnected = !!status?.wifi_ssid
  const ethConnected =
    !!status?.ether_speed && status.ether_speed !== "Unknown!"

  return (
    // One unified 2-column grid for the whole tab so every card aligns to the
    // same two columns. The old layout stacked three separate containers — a
    // 50/50 grid (WiFi/Ethernet), a masonry PrefGrid (BLE), and another grid
    // with a different breakpoint (Away/Cloud) — so column edges never lined
    // up row-to-row. Cards now pair up in order and collapse to one column
    // below lg (where two ~360px cards would get cramped).
    <div className="grid grid-cols-1 gap-2.5 lg:grid-cols-2">
      {/* Network interfaces */}
      <PrefCard
        icon={<WifiIcon className="h-3.5 w-3.5" />}
        halo={wifiConnected ? "accent" : "slate"}
        title="WiFi"
        badge={wifiConnected ? <Pill kind="accent">Connected</Pill> : null}
      >
        {wifiConnected && status ? (
          <>
            <div className="t-md font-semibold">{status.wifi_ssid}</div>
            <Row
              label="IP"
              value={<span className="t-mono">{status.wifi_ip || "—"}</span>}
            />
            {status.wifi_strength && (
              <Row label="Signal" value={status.wifi_strength} />
            )}
          </>
        ) : (
          <p className="t-xs">
            No WiFi configured. Use the Setup Wizard to scan and connect.
          </p>
        )}
      </PrefCard>

      <PrefCard
        icon={<LanIcon className="h-3.5 w-3.5" />}
        halo={ethConnected ? "accent" : "slate"}
        title="Ethernet"
        badge={
          ethConnected && status ? <Pill kind="accent">{status.ether_speed}</Pill> : null
        }
      >
        {ethConnected && status ? (
          <>
            <Row
              label="IP"
              value={<span className="t-mono">{status.ether_ip || "—"}</span>}
            />
            <Row label="Link" value={status.ether_speed} />
          </>
        ) : (
          <p className="t-xs">No Ethernet link detected.</p>
        )}
      </PrefCard>

      {/* Tesla BLE — enable the telemetry / keep-awake features, then pair.
          Kept adjacent so "enable" sits right next to "pair".
          The self-fetching sections each get an error boundary (this tab
          doesn't use PrefGrid, which wraps its children automatically): a
          render crash in one card must not blank the whole app. */}
      <SectionErrorBoundary>
        <BleEnableToggle />
      </SectionErrorBoundary>
      <SectionErrorBoundary>
        <BlePairButton />
      </SectionErrorBoundary>

      {/* Remote access */}
      <SectionErrorBoundary>
        <AwayModeControl
          onOpenWizard={onOpenWizard}
          openTravelMode={openTravelMode}
          onTravelModeClose={onTravelModeClose}
        />
      </SectionErrorBoundary>
      <SectionErrorBoundary>
        <CloudPairingSection />
      </SectionErrorBoundary>
    </div>
  )
}
