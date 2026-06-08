//! Tracker HTTP client: announce/discover peers by content id.
//!
//! The tracker (a tiny serverless app, default `https://np2ptp.vercel.app`) is
//! pure discovery — it only swaps contact info, BitTorrent-tracker style. The
//! transfer stays peer-to-peer over QUIC.

use std::error::Error;

use np2ptp_core::Hash;
use np2ptp_net::{Multiaddr, PeerId};
use serde::Deserialize;

pub const DEFAULT_TRACKER: &str = "https://np2ptp.vercel.app";

#[derive(Deserialize)]
struct PeersResp {
    peers: Vec<PeerEntry>,
}

#[derive(Deserialize)]
struct PeerEntry {
    peer: String,
    #[serde(default)]
    addrs: Vec<String>,
    #[serde(default)]
    addr: Option<String>,
}

/// Announce that we serve `cid` at our `addrs` under identity `peer`. Each
/// address is published as a full `/…/p2p/<peer>` multiaddr so a client can dial.
pub async fn announce(
    tracker: &str,
    cid: Hash,
    peer: PeerId,
    addrs: &[Multiaddr],
) -> Result<(), Box<dyn Error>> {
    let addr_strs: Vec<String> = addrs.iter().map(|a| format!("{a}/p2p/{peer}")).collect();
    let body = serde_json::json!({
        "cid": cid.to_hex(),
        "peer": peer.to_string(),
        "addrs": addr_strs,
    });
    reqwest::Client::new()
        .post(format!("{tracker}/announce"))
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// Discover peers serving `cid`. Returns `(peer, dialable multiaddrs)`.
pub async fn get_peers(tracker: &str, cid: Hash) -> Result<Vec<(PeerId, Vec<Multiaddr>)>, Box<dyn Error>> {
    let url = format!("{tracker}/peers?cid={}", cid.to_hex());
    let resp: PeersResp = reqwest::Client::new().get(url).send().await?.json().await?;

    let mut out = Vec::new();
    for entry in resp.peers {
        let Ok(peer) = entry.peer.parse::<PeerId>() else {
            continue;
        };
        let mut addrs: Vec<Multiaddr> = entry.addrs.iter().filter_map(|a| a.parse().ok()).collect();
        if let Some(a) = entry.addr.and_then(|a| a.parse().ok()) {
            addrs.push(a);
        }
        if !addrs.is_empty() {
            out.push((peer, addrs));
        }
    }
    Ok(out)
}
