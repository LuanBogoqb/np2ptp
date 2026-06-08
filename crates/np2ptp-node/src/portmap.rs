//! NAT-PMP / PCP port mapping — router-assisted reachability, complementing the
//! UPnP/IGD support in `np2ptp-net`. Some routers speak NAT-PMP/PCP but not IGD,
//! so trying these widens the "just works at home" coverage before falling back
//! to hole punching + relay.

use std::net::IpAddr;
use std::num::NonZeroU16;

use crab_nat::{InternetProtocol, PortMapping, PortMappingOptions};

/// A live mapping. Keep `mapping` alive for the session — dropping it may delete
/// the mapping on the router.
pub struct Mapped {
    pub external_port: u16,
    /// Held only to keep the mapping alive for the session (its Drop tears the
    /// router mapping down), so it is intentionally never read.
    #[allow(dead_code)]
    pub mapping: PortMapping,
}

/// Try to map `internal_port` (UDP) on the default gateway via PCP, then NAT-PMP.
pub async fn try_map_udp(internal_port: u16) -> Result<Mapped, String> {
    let iface = netdev::get_default_interface().map_err(|e| e.to_string())?;
    let gateway = iface
        .gateway
        .as_ref()
        .and_then(|g| g.ipv4.first().copied())
        .ok_or_else(|| "no default gateway".to_string())?;
    let local = iface
        .ipv4
        .first()
        .map(|n| n.addr())
        .ok_or_else(|| "no local IPv4 address".to_string())?;
    let internal = NonZeroU16::new(internal_port).ok_or_else(|| "invalid port".to_string())?;

    let mapping = PortMapping::new(
        IpAddr::V4(gateway),
        IpAddr::V4(local),
        InternetProtocol::Udp,
        internal,
        PortMappingOptions::default(),
    )
    .await
    .map_err(|e| e.to_string())?;

    Ok(Mapped { external_port: mapping.external_port().get(), mapping })
}

/// Best-effort public IP (the gateway's external IP, for a single layer of NAT).
pub async fn public_ip() -> Option<IpAddr> {
    let text = reqwest::get("https://api.ipify.org").await.ok()?.text().await.ok()?;
    text.trim().parse().ok()
}
