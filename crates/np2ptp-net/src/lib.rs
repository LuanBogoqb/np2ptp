//! `np2ptp-net` — the networking layer: real peers exchanging content over libp2p.
//!
//! This is where NP2PTP stops being a local stand-in and starts talking to other
//! machines. It builds on libp2p so we don't reinvent transport, encryption,
//! peer identity, DHT, or NAT traversal:
//!
//! * **QUIC** transport — encrypted (TLS 1.3), multiplexed, connection-migrating.
//! * **request-response** — fetch a manifest by content id, or a chunk by hash.
//! * **Kademlia** — announce (`provide`) and discover (`find_providers`) which
//!   peers hold a given content id, magnet-style.
//! * **identify** — learn peers' listen addresses (fed into Kademlia).
//!
//! The headline capability is [`Network::download`]: given a content id and a
//! provider, pull the manifest and every chunk over QUIC, verifying each chunk
//! against the Merkle root before storing it — the same integrity guarantee the
//! local client has, now over the wire.

mod receipts;

use std::collections::HashMap;
use std::time::Duration;

use futures::StreamExt;
use libp2p::{
    autonat, dcutr, identify, identity, kad, mdns, noise, relay,
    request_response::{self, ProtocolSupport},
    swarm::SwarmEvent,
    upnp, yamux, StreamProtocol, Swarm,
};
use np2ptp_core::{Hash, Manifest};
use np2ptp_rep::{Identity, Ledger, Receipt};
use crate::receipts::{ReceiptBag, ReceiptBagError};
use np2ptp_store::Store;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

// Re-export the libp2p types that appear in this crate's public API, so callers
// (and tests) don't need a direct libp2p dependency.
pub use libp2p::{Multiaddr, PeerId};
pub use np2ptp_rep::Counters;

/// Extract the `PeerId` from a multiaddr that ends in `/p2p/<peer-id>`, if present.
pub fn peer_id_from_multiaddr(addr: &Multiaddr) -> Option<PeerId> {
    // The *last* `/p2p/<id>` is the actual destination. A plain address has
    // just one, but a relay circuit address carries two — .../p2p/<relay>/
    // p2p-circuit/p2p/<target> — and the target, not the relay, is who we're
    // actually talking to once the circuit is established.
    addr.iter()
        .filter_map(|proto| match proto {
            libp2p::multiaddr::Protocol::P2p(id) => Some(id),
            _ => None,
        })
        .last()
}

/// Protocol id for the content request-response protocol.
const CONTENT_PROTOCOL: &str = "/np2ptp/content/1";
/// Protocol id advertised via identify.
const IDENTIFY_PROTOCOL: &str = "/np2ptp/id/1";

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("transport/build error: {0}")]
    Build(String),
    #[error("dial error: {0}")]
    Dial(#[from] libp2p::swarm::DialError),
    #[error("listen error: {0}")]
    Listen(#[from] libp2p::TransportError<std::io::Error>),
    #[error("the network task has shut down")]
    Shutdown,
    #[error("store error: {0}")]
    Store(#[from] np2ptp_store::StoreError),
    #[error("ledger persistence: {0}")]
    Ledger(#[from] np2ptp_rep::LedgerError),
    #[error("receipt bag persistence: {0}")]
    Receipts(#[from] ReceiptBagError),
    #[error("request to peer failed")]
    RequestFailed,
    #[error("peer did not have the requested manifest")]
    NoManifest,
    #[error("manifest from peer is invalid or does not match the requested content id")]
    BadManifest,
    #[error("peer was missing chunk {0}")]
    MissingChunk(Hash),
    #[error("chunk from peer failed verification against the content id")]
    BadChunk,
    #[error("no providers found for content id")]
    NoProviders,
}

/// A request: a manifest by content id, a chunk by hash, a RaptorQ repair
/// symbol for a content id by index, a signed receipt to submit, or a pull
/// for this peer's own collected receipts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Manifest([u8; 32]),
    Chunk([u8; 32]),
    Symbol { root: [u8; 32], index: u32 },
    /// A contiguous batch of RaptorQ symbols `[start, start+count)` — far fewer
    /// round-trips than fetching symbols one at a time.
    Symbols { root: [u8; 32], start: u32, count: u32 },
    /// Sent by a client after a completed download: one signed receipt
    /// crediting the server for bytes served this session.
    SubmitReceipt(Receipt),
    /// "Send me the receipts you've collected about yourself" — sent once,
    /// to a peer this node has no ledger history for yet.
    GetReceipts,
}

/// The matching response. `None` means the peer doesn't hold that item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Manifest(Option<Vec<u8>>),
    Chunk(Option<Vec<u8>>),
    Symbol(Option<Vec<u8>>),
    /// Symbols for the requested range (may be shorter than asked near the end,
    /// empty once exhausted).
    Symbols(Vec<Vec<u8>>),
    ReceiptAck,
    /// This peer's own collected receipts (bounded, see `ReceiptBag`).
    Receipts(Vec<Receipt>),
}

/// How many repair symbols a seeder generates per content, on top of the source
/// symbols. More = more resilience to symbol loss at the cost of memory.
const FEC_REPAIR_SYMBOLS: u32 = 64;

/// How many symbols a FEC download requests per round-trip.
const FEC_BATCH: u32 = 128;

/// Per-circuit limits for when this node acts as a relay for someone else.
/// libp2p-relay's own defaults (128 KiB, 2 minutes) are sized for signaling
/// traffic, not content transfer — a real download blows past them on the
/// very first request (a multi-thousand-chunk manifest alone can be well
/// over 1 MB), which is what made relayed transfers fail. 512 MiB / 10 min
/// comfortably moves real content (game installs, ISOs, media) while still
/// bounding how much of the relay operator's bandwidth one circuit can take
/// — chunked/resumable download means a transfer that outgrows one circuit
/// just reconnects and picks up the remaining chunks.
const RELAY_MAX_CIRCUIT_BYTES: u64 = 512 * 1024 * 1024;
const RELAY_MAX_CIRCUIT_DURATION: Duration = Duration::from_secs(10 * 60);

fn relay_config() -> relay::Config {
    relay::Config {
        max_circuit_bytes: RELAY_MAX_CIRCUIT_BYTES,
        max_circuit_duration: RELAY_MAX_CIRCUIT_DURATION,
        ..relay::Config::default()
    }
}

/// The combined libp2p behaviour for an NP2PTP node.
#[derive(libp2p::swarm::NetworkBehaviour)]
struct Behaviour {
    kad: kad::Behaviour<kad::store::MemoryStore>,
    rr: request_response::cbor::Behaviour<Request, Response>,
    identify: identify::Behaviour,
    // NAT traversal: be a relay for others, use relays ourselves, hole-punch, and
    // learn our own reachability.
    relay_server: relay::Behaviour,
    relay_client: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    autonat: autonat::Behaviour,
    // Auto-open a port on the home router (IGD) so the node is reachable from the
    // open internet without a VPN — the single biggest NAT win for home users.
    upnp: upnp::tokio::Behaviour,
    // Zero-config discovery on the same LAN — no tracker/DHT/--peer needed to
    // find another NP2PTP node a few feet away.
    mdns: mdns::tokio::Behaviour,
}

/// Commands sent from a [`Network`] handle to the swarm task.
enum Command {
    Listen { addr: Multiaddr, reply: oneshot::Sender<Result<(), NetError>> },
    Dial { addr: Multiaddr, reply: oneshot::Sender<Result<(), NetError>> },
    Listeners { reply: oneshot::Sender<Vec<Multiaddr>> },
    ExternalAddresses { reply: oneshot::Sender<Vec<Multiaddr>> },
    AddPeer { peer: PeerId, addr: Multiaddr },
    AddExternalAddress { addr: Multiaddr },
    Provide { root: Hash, manifest_bytes: Vec<u8> },
    FindProviders { root: Hash, reply: oneshot::Sender<Vec<PeerId>> },
    Request { peer: PeerId, request: Request, reply: oneshot::Sender<Result<Response, NetError>> },
    SetChokeThreshold { threshold: i64 },
    /// Sign and send a receipt crediting `peer` for `bytes` this node
    /// received from it. Fire-and-forget: the caller doesn't wait for the
    /// server's acknowledgement.
    SubmitReceipt { peer: PeerId, bytes: u64 },
    Reputation { peer: PeerId, reply: oneshot::Sender<i64> },
    PutRecord { key: Vec<u8>, value: Vec<u8>, reply: oneshot::Sender<bool> },
    GetRecord { key: Vec<u8>, reply: oneshot::Sender<Option<Vec<u8>>> },
    ConnectedPeers { reply: oneshot::Sender<Vec<PeerId>> },
    LedgerTotals { reply: oneshot::Sender<np2ptp_rep::Counters> },
}

/// DHT record key for a torrent-infohash -> nptp-root mapping.
fn mapping_key(infohash: &[u8]) -> Vec<u8> {
    let mut tagged = Vec::with_capacity(18 + infohash.len());
    tagged.extend_from_slice(b"np2ptp-bridge:v1:");
    tagged.extend_from_slice(infohash);
    Hash::of(&tagged).as_bytes().to_vec()
}

/// A cloneable handle to a running NP2PTP network node.
#[derive(Clone)]
pub struct Network {
    cmd_tx: mpsc::Sender<Command>,
    local_peer_id: PeerId,
}

impl Network {
    /// Build and spawn a node serving content from `store`. An optional 32-byte
    /// seed makes the peer identity deterministic (useful for tests).
    pub fn spawn(store: Store, seed: Option<[u8; 32]>) -> Result<Network, NetError> {
        // A caller-supplied seed means this node keeps a stable identity
        // across restarts (e.g. `serve`/`relay` persisting `identity.key`)
        // — in that case its ledger and receipt bag are persisted alongside
        // it too. With no seed, generate the random bytes ourselves (instead
        // of letting libp2p's own RNG hide them) so the same 32 bytes build
        // both the libp2p keypair and the paired `np2ptp_rep::Identity`
        // used to sign outgoing receipts.
        let persistent = seed.is_some();
        let mut seed_bytes = match seed {
            Some(bytes) => bytes,
            None => {
                let mut bytes = [0u8; 32];
                getrandom::getrandom(&mut bytes).map_err(|e| NetError::Build(e.to_string()))?;
                bytes
            }
        };
        // Build the rep identity BEFORE the libp2p keypair: `ed25519_from_bytes`
        // zeroizes its input buffer once it's done with it, so building the
        // rep identity second would sign with an all-zero seed.
        let rep_identity = Identity::from_seed(seed_bytes);
        let keypair = identity::Keypair::ed25519_from_bytes(&mut seed_bytes)
            .map_err(|e| NetError::Build(e.to_string()))?;
        let local_peer_id = keypair.public().to_peer_id();
        // Built outside the `with_behaviour` closure (which must be infallible)
        // so its `io::Result` can still propagate through `NetError::Build`.
        let mdns_behaviour = mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)
            .map_err(|e| NetError::Build(e.to_string()))?;

        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_quic()
            .with_relay_client(noise::Config::new, yamux::Config::default)
            .map_err(|e| NetError::Build(e.to_string()))?
            .with_behaviour(|key, relay_client| {
                let peer_id = key.public().to_peer_id();
                Behaviour {
                    kad: kad::Behaviour::new(peer_id, kad::store::MemoryStore::new(peer_id)),
                    rr: request_response::cbor::Behaviour::new(
                        [(StreamProtocol::new(CONTENT_PROTOCOL), ProtocolSupport::Full)],
                        request_response::Config::default(),
                    ),
                    identify: identify::Behaviour::new(identify::Config::new(
                        IDENTIFY_PROTOCOL.to_string(),
                        key.public(),
                    )),
                    relay_server: relay::Behaviour::new(peer_id, relay_config()),
                    relay_client,
                    dcutr: dcutr::Behaviour::new(peer_id),
                    autonat: autonat::Behaviour::new(peer_id, autonat::Config::default()),
                    upnp: upnp::tokio::Behaviour::default(),
                    mdns: mdns_behaviour,
                }
            })
            .map_err(|e| NetError::Build(e.to_string()))?
            .build();

        // Act as a full DHT server: store provider records and answer queries.
        // Without this, libp2p keeps Kademlia in client mode until it confirms
        // external reachability — which never happens on loopback — and the node
        // silently won't serve `get_providers`.
        swarm
            .behaviour_mut()
            .kad
            .set_mode(Some(kad::Mode::Server));

        let (ledger, receipts) = if persistent {
            let root = store.root();
            (
                Ledger::open(root.join("ledger.bin"))?,
                ReceiptBag::open(root.join("receipts.bin"))?,
            )
        } else {
            (Ledger::new(), ReceiptBag::new())
        };

        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        tokio::spawn(EventLoop::new(swarm, store, cmd_rx, ledger, receipts, rep_identity).run());
        Ok(Network { cmd_tx, local_peer_id })
    }

    pub fn local_peer_id(&self) -> PeerId {
        self.local_peer_id
    }

    async fn send(&self, cmd: Command) -> Result<(), NetError> {
        self.cmd_tx.send(cmd).await.map_err(|_| NetError::Shutdown)
    }

    /// Start listening (e.g. `/ip4/0.0.0.0/udp/0/quic-v1`).
    pub async fn listen(&self, addr: Multiaddr) -> Result<(), NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Listen { addr, reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)?
    }

    /// Dial a peer at `addr`.
    pub async fn dial(&self, addr: Multiaddr) -> Result<(), NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Dial { addr, reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)?
    }

    /// The addresses this node is actually listening on (local interfaces).
    pub async fn listeners(&self) -> Result<Vec<Multiaddr>, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Listeners { reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)
    }

    /// Confirmed external (publicly-reachable) addresses, e.g. a UPnP-mapped
    /// router address. These are what remote peers on other networks can dial.
    pub async fn external_addresses(&self) -> Result<Vec<Multiaddr>, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::ExternalAddresses { reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)
    }

    /// Teach Kademlia a peer's address (used to bootstrap discovery).
    pub async fn add_peer(&self, peer: PeerId, addr: Multiaddr) -> Result<(), NetError> {
        self.send(Command::AddPeer { peer, addr }).await
    }

    /// Advertise one of our own externally-reachable addresses. A relay node must
    /// do this so the reservations it grants carry a usable address (otherwise
    /// clients reject them with `NoAddressesInReservation`).
    pub async fn add_external_address(&self, addr: Multiaddr) -> Result<(), NetError> {
        self.send(Command::AddExternalAddress { addr }).await
    }

    /// Announce that this node serves `manifest`'s content (its chunks must
    /// already be in this node's store). Publishes a provider record in the DHT.
    pub async fn provide(&self, manifest: &Manifest) -> Result<(), NetError> {
        let manifest_bytes = manifest.to_nptp().map_err(|_| NetError::BadManifest)?;
        self.send(Command::Provide { root: manifest.root, manifest_bytes }).await
    }

    /// Set the choke threshold: a peer is refused chunks once its reputation
    /// (bytes it served us minus bytes we served it) drops below `-threshold`.
    /// Default is effectively unlimited (no choking).
    pub async fn set_choke_threshold(&self, threshold: i64) -> Result<(), NetError> {
        self.send(Command::SetChokeThreshold { threshold }).await
    }

    /// Credit `peer` with a signed receipt for `bytes` this node received
    /// from it. Called automatically after a successful download; best
    /// effort — a delivery failure does not undo the download.
    pub async fn submit_receipt(&self, peer: PeerId, bytes: u64) -> Result<(), NetError> {
        self.send(Command::SubmitReceipt { peer, bytes }).await
    }

    /// This node's recorded reputation for `peer` (positive = net giver).
    pub async fn reputation(&self, peer: PeerId) -> Result<i64, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Reputation { peer, reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)
    }

    /// Peers this node currently has an open connection to.
    pub async fn connected_peers(&self) -> Result<Vec<PeerId>, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::ConnectedPeers { reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)
    }

    /// Aggregate bytes served/received across every peer this node has
    /// dealt with (see [`Ledger::totals`]).
    pub async fn ledger_totals(&self) -> Result<Counters, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::LedgerTotals { reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)
    }

    /// Store an arbitrary key/value in the DHT (used for the torrent mapping).
    pub async fn put_record(&self, key: Vec<u8>, value: Vec<u8>) -> Result<bool, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::PutRecord { key, value, reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)
    }

    /// Look up a key in the DHT.
    pub async fn get_record(&self, key: Vec<u8>) -> Result<Option<Vec<u8>>, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::GetRecord { key, reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)
    }

    /// Publish a torrent-infohash -> nptp-content-id mapping (bridge registry).
    pub async fn put_mapping(&self, infohash: &[u8], root: Hash) -> Result<bool, NetError> {
        self.put_record(mapping_key(infohash), root.as_bytes().to_vec()).await
    }

    /// Resolve a torrent infohash to an nptp content id via the DHT, if bridged.
    pub async fn get_mapping(&self, infohash: &[u8]) -> Result<Option<Hash>, NetError> {
        let value = self.get_record(mapping_key(infohash)).await?;
        Ok(value.and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok().map(Hash)))
    }

    /// Discover peers that provide `root` via the DHT.
    pub async fn find_providers(&self, root: Hash) -> Result<Vec<PeerId>, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::FindProviders { root, reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)
    }

    async fn request(&self, peer: PeerId, request: Request) -> Result<Response, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Request { peer, request, reply }).await?;
        rx.await.map_err(|_| NetError::Shutdown)?
    }

    /// Fetch a content manifest by id from `peer`, validating it matches `root`.
    pub async fn get_manifest(&self, peer: PeerId, root: Hash) -> Result<Manifest, NetError> {
        match self.request(peer, Request::Manifest(*root.as_bytes())).await? {
            Response::Manifest(Some(bytes)) => {
                let manifest = Manifest::from_nptp(&bytes).map_err(|_| NetError::BadManifest)?;
                // Defend against a lying provider: the manifest must actually be
                // the content we asked for, and be internally consistent.
                if manifest.root != root || !manifest.root_is_consistent() {
                    return Err(NetError::BadManifest);
                }
                Ok(manifest)
            }
            Response::Manifest(None) => Err(NetError::NoManifest),
            _ => Err(NetError::RequestFailed),
        }
    }

    /// Fetch one chunk from `peer` by hash.
    pub async fn fetch_chunk(&self, peer: PeerId, hash: Hash) -> Result<Option<Vec<u8>>, NetError> {
        match self.request(peer, Request::Chunk(*hash.as_bytes())).await? {
            Response::Chunk(data) => Ok(data),
            _ => Err(NetError::RequestFailed),
        }
    }

    /// Fetch one RaptorQ symbol (by index) for `root` from `peer`.
    pub async fn fetch_symbol(
        &self,
        peer: PeerId,
        root: Hash,
        index: u32,
    ) -> Result<Option<Vec<u8>>, NetError> {
        match self.request(peer, Request::Symbol { root: *root.as_bytes(), index }).await? {
            Response::Symbol(data) => Ok(data),
            _ => Err(NetError::RequestFailed),
        }
    }

    /// Fetch a batch of RaptorQ symbols `[start, start+count)` for `root`.
    pub async fn fetch_symbols(
        &self,
        peer: PeerId,
        root: Hash,
        start: u32,
        count: u32,
    ) -> Result<Vec<Vec<u8>>, NetError> {
        match self.request(peer, Request::Symbols { root: *root.as_bytes(), start, count }).await? {
            Response::Symbols(data) => Ok(data),
            _ => Err(NetError::RequestFailed),
        }
    }

    /// Erasure-coded download: pull RaptorQ symbols for `root` from `provider`
    /// until the content can be reconstructed, then verify the reconstruction
    /// against the content id and store it in `into`.
    ///
    /// Unlike [`Network::download`], this needs no specific chunk — *any*
    /// sufficiently large set of symbols works, which is what makes content
    /// survive seeder churn (the "permanence" goal). It is the path the research
    /// harness compares against plain chunk download.
    pub async fn download_fec(
        &self,
        root: Hash,
        provider: PeerId,
        into: &Store,
    ) -> Result<Manifest, NetError> {
        self.download_fec_with_progress(root, provider, into, |_, _| {}).await
    }

    /// Like [`Network::download_fec`], but calls `on_progress(symbols_collected,
    /// symbols_needed)` after each batch of symbols arrives.
    pub async fn download_fec_with_progress(
        &self,
        root: Hash,
        provider: PeerId,
        into: &Store,
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<Manifest, NetError> {
        let manifest = self.get_manifest(provider, root).await?;
        let config = np2ptp_fec::config_for(manifest.total_size, np2ptp_fec::DEFAULT_SYMBOL_SIZE);

        // Only attempt a decode once we likely have enough symbols (decoding is
        // the expensive step, so don't retry it after every batch).
        let symbol_size = np2ptp_fec::DEFAULT_SYMBOL_SIZE as usize;
        let need = (manifest.total_size as usize).div_ceil(symbol_size).max(1);

        let mut symbols: Vec<Vec<u8>> = Vec::new();
        let mut start = 0u32;
        let mut fetched_bytes: u64 = 0;
        let decoded = loop {
            let batch = self.fetch_symbols(provider, root, start, FEC_BATCH).await?;
            let exhausted = batch.is_empty();
            start += batch.len() as u32;
            fetched_bytes += batch.iter().map(|s| s.len() as u64).sum::<u64>();
            symbols.extend(batch);
            on_progress(symbols.len().min(need), need);

            if symbols.len() >= need || exhausted {
                if let Some(data) = np2ptp_fec::decode(&config, manifest.total_size, symbols.clone()) {
                    break data;
                }
                if exhausted {
                    return Err(NetError::MissingChunk(root));
                }
            }
        };

        // The decoded stream must reproduce the requested content id exactly.
        let files = manifest.split_stream(&decoded).map_err(|_| NetError::BadChunk)?;
        let recomputed =
            Manifest::from_files(files.iter().map(|(p, b)| (p.clone(), b.as_slice())), manifest.name.clone());
        if recomputed.root != root {
            return Err(NetError::BadChunk);
        }
        into.ingest_tree(&files, manifest.name.clone())?;
        if fetched_bytes > 0 {
            let _ = self.submit_receipt(provider, fetched_bytes).await;
        }
        Ok(manifest)
    }

    /// Full end-to-end download: fetch the manifest for `root` from `provider`,
    /// then pull every chunk, verifying each against the Merkle root before
    /// storing it in `into`. Chunks already present are skipped. Returns the
    /// validated manifest so the caller can reconstruct files from `into`.
    pub async fn download(
        &self,
        root: Hash,
        provider: PeerId,
        into: &Store,
    ) -> Result<Manifest, NetError> {
        self.download_with_progress(root, provider, into, |_, _| {}).await
    }

    /// Like [`Network::download`], but calls `on_progress(chunks_done,
    /// chunks_total)` — once immediately with however many chunks were
    /// already local, then once per chunk actually pulled over the network.
    pub async fn download_with_progress(
        &self,
        root: Hash,
        provider: PeerId,
        into: &Store,
        mut on_progress: impl FnMut(usize, usize),
    ) -> Result<Manifest, NetError> {
        /// Chunk requests kept in flight at once. Hides per-request latency.
        const PARALLEL: usize = 16;

        let manifest = self.get_manifest(provider, root).await?;
        // get_manifest already validated the chunk list against the root, so a
        // cheap per-chunk content-hash check is sufficient below.

        // Only fetch chunks we don't already have (resume / cross-download dedup).
        let missing: Vec<(usize, Hash)> = manifest
            .chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| !into.has(&c.hash))
            .map(|(i, c)| (i, c.hash))
            .collect();

        let total = manifest.chunks.len();
        let mut done = total - missing.len();
        on_progress(done, total);

        // Fetch concurrently, but store + verify each chunk AS it arrives so we
        // never hold more than a handful of chunks in memory (large content).
        let mut stream = futures::stream::iter(missing)
            .map(|(i, hash)| async move {
                let bytes = self
                    .fetch_chunk(provider, hash)
                    .await?
                    .ok_or(NetError::MissingChunk(hash))?;
                Ok::<(usize, Vec<u8>), NetError>((i, bytes))
            })
            .buffer_unordered(PARALLEL);

        let mut fetched_bytes: u64 = 0;
        while let Some(result) = stream.next().await {
            let (i, bytes) = result?;
            if !manifest.chunk_hash_ok(i, &bytes) {
                return Err(NetError::BadChunk);
            }
            fetched_bytes += bytes.len() as u64;
            into.put(&bytes)?;
            done += 1;
            on_progress(done, total);
        }
        if fetched_bytes > 0 {
            let _ = self.submit_receipt(provider, fetched_bytes).await;
        }
        Ok(manifest)
    }
}

/// What an internally-initiated outbound request (one `EventLoop` sent to
/// itself, not on behalf of a `Network` handle call) was for.
enum InternalRequest {
    GetReceipts,
    SubmitReceiptAck,
}

/// The background task owning the swarm.
struct EventLoop {
    swarm: Swarm<Behaviour>,
    store: Store,
    cmd_rx: mpsc::Receiver<Command>,
    /// Manifests this node is serving, keyed by content id.
    provided: HashMap<Hash, Vec<u8>>,
    /// Outbound requests awaiting a response.
    pending_req: HashMap<request_response::OutboundRequestId, oneshot::Sender<Result<Response, NetError>>>,
    /// In-flight provider lookups: accumulated peers + the waiting caller.
    pending_providers: HashMap<kad::QueryId, (Vec<PeerId>, oneshot::Sender<Vec<PeerId>>)>,
    /// In-flight DHT record put/get queries.
    pending_put: HashMap<kad::QueryId, oneshot::Sender<bool>>,
    pending_get: HashMap<kad::QueryId, oneshot::Sender<Option<Vec<u8>>>>,
    /// Per-peer contribution accounting (incentives).
    ledger: Ledger<PeerId>,
    /// Refuse serving a peer once its reputation falls below `-choke_threshold`.
    choke_threshold: i64,
    /// Cache of RaptorQ symbols per content id, generated lazily on first request.
    symbols: HashMap<Hash, Vec<Vec<u8>>>,
    /// This node's own signing identity, paired with its libp2p keypair.
    rep_identity: Identity,
    /// Bridges a connected peer's libp2p `PeerId` to its Ed25519 `rep::PeerId`,
    /// learned from `identify` — needed to confirm a presented receipt is
    /// genuinely about the peer presenting it.
    rep_peers: HashMap<PeerId, np2ptp_rep::PeerId>,
    /// Receipts collected about this node, presented to new peers on request.
    receipts: ReceiptBag,
    /// Outbound requests this `EventLoop` issued on its own initiative
    /// (not on behalf of an external `Network` handle call), tagged with
    /// what to do once the response arrives.
    pending_internal: HashMap<request_response::OutboundRequestId, InternalRequest>,
    /// Peers we have already sent a `GetReceipts` pull to, so we never send
    /// a second one even if `identify` fires again for the same peer before
    /// the first response arrives.
    receipts_pulled_from: std::collections::HashSet<PeerId>,
    /// Monotonic counter for receipts this node issues (see `Receipt::epoch`).
    next_receipt_epoch: u64,
}

impl EventLoop {
    fn new(
        swarm: Swarm<Behaviour>,
        store: Store,
        cmd_rx: mpsc::Receiver<Command>,
        ledger: Ledger<PeerId>,
        receipts: ReceiptBag,
        rep_identity: Identity,
    ) -> EventLoop {
        EventLoop {
            swarm,
            store,
            cmd_rx,
            provided: HashMap::new(),
            pending_req: HashMap::new(),
            pending_providers: HashMap::new(),
            pending_put: HashMap::new(),
            pending_get: HashMap::new(),
            ledger,
            choke_threshold: i64::MAX, // no choking until configured
            symbols: HashMap::new(),
            rep_identity,
            rep_peers: HashMap::new(),
            receipts,
            pending_internal: HashMap::new(),
            receipts_pulled_from: std::collections::HashSet::new(),
            next_receipt_epoch: 0,
        }
    }

    async fn run(mut self) {
        loop {
            tokio::select! {
                event = self.swarm.select_next_some() => self.on_event(event),
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(cmd) => self.on_command(cmd),
                    None => break, // all handles dropped
                },
            }
        }
    }

    fn on_command(&mut self, cmd: Command) {
        match cmd {
            Command::Listen { addr, reply } => {
                let r = self.swarm.listen_on(addr).map(|_| ()).map_err(NetError::from);
                let _ = reply.send(r);
            }
            Command::Dial { addr, reply } => {
                let r = self.swarm.dial(addr).map_err(NetError::from);
                let _ = reply.send(r);
            }
            Command::Listeners { reply } => {
                let _ = reply.send(self.swarm.listeners().cloned().collect());
            }
            Command::ExternalAddresses { reply } => {
                let _ = reply.send(self.swarm.external_addresses().cloned().collect());
            }
            Command::AddPeer { peer, addr } => {
                self.swarm.behaviour_mut().kad.add_address(&peer, addr);
            }
            Command::AddExternalAddress { addr } => {
                self.swarm.add_external_address(addr);
            }
            Command::Provide { root, manifest_bytes } => {
                self.provided.insert(root, manifest_bytes);
                let key = kad::RecordKey::new(root.as_bytes());
                let _ = self.swarm.behaviour_mut().kad.start_providing(key);
            }
            Command::FindProviders { root, reply } => {
                let key = kad::RecordKey::new(root.as_bytes());
                let id = self.swarm.behaviour_mut().kad.get_providers(key);
                self.pending_providers.insert(id, (Vec::new(), reply));
            }
            Command::Request { peer, request, reply } => {
                let id = self.swarm.behaviour_mut().rr.send_request(&peer, request);
                self.pending_req.insert(id, reply);
            }
            Command::SetChokeThreshold { threshold } => {
                self.choke_threshold = threshold;
            }
            Command::SubmitReceipt { peer, bytes } => {
                if let Some(&server) = self.rep_peers.get(&peer) {
                    let epoch = self.next_receipt_epoch;
                    self.next_receipt_epoch += 1;
                    let receipt = Receipt::issue(&self.rep_identity, server, bytes, epoch);
                    let id = self.swarm.behaviour_mut().rr.send_request(&peer, Request::SubmitReceipt(receipt));
                    self.pending_internal.insert(id, InternalRequest::SubmitReceiptAck);
                }
                // If we haven't identified this peer yet, there's no rep::PeerId
                // to credit — silently skip (best-effort, per submit_receipt's contract).
            }
            Command::Reputation { peer, reply } => {
                let _ = reply.send(self.ledger.reputation(&peer));
            }
            Command::PutRecord { key, value, reply } => {
                let record = kad::Record::new(kad::RecordKey::new(&key), value);
                match self.swarm.behaviour_mut().kad.put_record(record, kad::Quorum::One) {
                    Ok(qid) => {
                        self.pending_put.insert(qid, reply);
                    }
                    Err(_) => {
                        let _ = reply.send(false);
                    }
                }
            }
            Command::GetRecord { key, reply } => {
                let qid = self.swarm.behaviour_mut().kad.get_record(kad::RecordKey::new(&key));
                self.pending_get.insert(qid, reply);
            }
            Command::ConnectedPeers { reply } => {
                let _ = reply.send(self.swarm.connected_peers().cloned().collect());
            }
            Command::LedgerTotals { reply } => {
                let _ = reply.send(self.ledger.totals());
            }
        }
    }

    fn on_event(&mut self, event: SwarmEvent<BehaviourEvent>) {
        match event {
            SwarmEvent::Behaviour(BehaviourEvent::Rr(e)) => self.on_rr(e),
            SwarmEvent::Behaviour(BehaviourEvent::Kad(e)) => self.on_kad(e),
            SwarmEvent::Behaviour(BehaviourEvent::Identify(identify::Event::Received {
                peer_id,
                info,
                ..
            })) => {
                // Feed learned addresses into Kademlia so discovery can route.
                for addr in info.listen_addrs {
                    self.swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                }
                // Bridge this peer's libp2p identity to its Ed25519 rep::PeerId
                // (every np2ptp node's key is Ed25519, so this always succeeds
                // in practice) and, if we have no history for it yet, pull its
                // collected receipts once — reputation that travels, instead
                // of starting cold with every peer we meet.
                if let Ok(ed_pk) = info.public_key.clone().try_into_ed25519() {
                    let rep_peer = np2ptp_rep::PeerId::from_bytes(ed_pk.to_bytes());
                    self.rep_peers.insert(peer_id, rep_peer);
                    if !self.receipts_pulled_from.contains(&peer_id)
                        && self.ledger.counters(&peer_id) == Counters::default()
                    {
                        self.receipts_pulled_from.insert(peer_id);
                        let id = self.swarm.behaviour_mut().rr.send_request(&peer_id, Request::GetReceipts);
                        self.pending_internal.insert(id, InternalRequest::GetReceipts);
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Upnp(event)) => match event {
                upnp::Event::NewExternalAddr(addr) => {
                    eprintln!("upnp: mapped a public address via the router: {addr}");
                }
                upnp::Event::GatewayNotFound => {
                    eprintln!("upnp: no IGD gateway found (router UPnP off/unsupported)");
                }
                upnp::Event::NonRoutableGateway => {
                    eprintln!("upnp: gateway has no public IP (CGNAT?) — needs relay/hole-punch");
                }
                upnp::Event::ExpiredExternalAddr(addr) => {
                    eprintln!("upnp: external address expired: {addr}");
                }
            },
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(event)) => match event {
                mdns::Event::Discovered(found) => {
                    for (peer_id, addr) in found {
                        // Route through Kademlia (so find_providers/get_record can
                        // use it) and dial directly (same LAN, should connect fast).
                        self.swarm.behaviour_mut().kad.add_address(&peer_id, addr.clone());
                        let _ = self.swarm.dial(addr);
                    }
                }
                // Addresses just age out of libp2p-mdns's own table; nothing here
                // depends on pruning them from Kademlia's on our side.
                mdns::Event::Expired(_) => {}
            },
            _ => {}
        }
    }

    fn on_rr(&mut self, event: request_response::Event<Request, Response>) {
        match event {
            request_response::Event::Message { peer, message, .. } => match message {
                request_response::Message::Request { request, channel, .. } => {
                    let response = self.serve(peer, request);
                    let _ = self.swarm.behaviour_mut().rr.send_response(channel, response);
                }
                request_response::Message::Response { request_id, response } => {
                    // Credit the peer for bytes it served us (incentives).
                    if let Response::Chunk(Some(data)) = &response {
                        self.ledger.record_received(peer, data.len() as u64);
                    }
                    match self.pending_internal.remove(&request_id) {
                        Some(InternalRequest::GetReceipts) => self.handle_receipts_response(peer, response),
                        Some(InternalRequest::SubmitReceiptAck) => {} // nothing to do with a bare ack
                        None => {
                            if let Some(reply) = self.pending_req.remove(&request_id) {
                                let _ = reply.send(Ok(response));
                            }
                        }
                    }
                }
            },
            request_response::Event::OutboundFailure { request_id, .. } => {
                self.pending_internal.remove(&request_id);
                if let Some(reply) = self.pending_req.remove(&request_id) {
                    let _ = reply.send(Err(NetError::RequestFailed));
                }
            }
            _ => {}
        }
    }

    /// Handle a peer's answer to our `GetReceipts` pull: verify each receipt
    /// and, only if it's genuinely about the peer that presented it, credit
    /// that peer in our own ledger. Deduplicates by (client, epoch) within
    /// this response so a peer can't inflate its credit by repeating the
    /// same receipt.
    ///
    /// Note: this proves *a* key vouched for the peer, not that the voucher
    /// is a distinct real peer — see "Trust model / limitations" in the
    /// design doc for what this feature does and doesn't defend against.
    fn handle_receipts_response(&mut self, peer: PeerId, response: Response) {
        let Response::Receipts(receipts) = response else { return };
        let Some(&expected_server) = self.rep_peers.get(&peer) else { return };
        let mut seen = std::collections::HashSet::new();
        let mut credited_any = false;
        for r in receipts {
            if r.verify() && r.server == expected_server && seen.insert((r.client, r.epoch)) {
                self.ledger.credit_receipt(peer, r.bytes);
                credited_any = true;
            }
        }
        if credited_any {
            if let Err(e) = self.ledger.save() {
                eprintln!("np2ptp: failed to persist ledger: {e}");
            }
        }
    }

    /// Answer an incoming request from our local state, applying the choke policy
    /// and recording what we serve.
    fn serve(&mut self, peer: PeerId, request: Request) -> Response {
        match request {
            Request::Manifest(root) => {
                Response::Manifest(self.provided.get(&Hash(root)).cloned())
            }
            Request::Chunk(hash) => {
                // Choke peers that have taken far more than they've given back.
                if self.ledger.reputation(&peer) < self.choke_threshold.saturating_neg() {
                    return Response::Chunk(None);
                }
                match self.store.get(&Hash(hash)).ok().flatten() {
                    Some(data) => {
                        self.ledger.record_served(peer, data.len() as u64);
                        Response::Chunk(Some(data))
                    }
                    None => Response::Chunk(None),
                }
            }
            Request::Symbol { root, index } => {
                let sym = self.symbol(Hash(root), index as usize);
                if let Some(bytes) = &sym {
                    self.ledger.record_served(peer, bytes.len() as u64);
                }
                Response::Symbol(sym)
            }
            Request::Symbols { root, start, count } => {
                let syms = self.symbols_range(Hash(root), start as usize, count as usize);
                let served: u64 = syms.iter().map(|s| s.len() as u64).sum();
                if served > 0 {
                    self.ledger.record_served(peer, served);
                }
                Response::Symbols(syms)
            }
            Request::SubmitReceipt(receipt) => {
                // A receipt must actually be about *this* node — otherwise an
                // attacker can submit arbitrary high-`bytes` receipts naming
                // someone else's `server`/a throwaway `client` key, evicting
                // our real earned receipts out of the (capped) bag via the
                // highest-`bytes`-wins insert policy. Also reject the
                // degenerate case of a peer vouching for itself
                // (`client == server`) — only a partial mitigation for
                // Sybil self-dealing (see the design doc's "Trust model /
                // limitations"), but it blocks the laziest form.
                if receipt.verify()
                    && receipt.server == self.rep_identity.peer_id()
                    && receipt.client != receipt.server
                {
                    self.receipts.insert(receipt);
                    if let Err(e) = self.receipts.save() {
                        eprintln!("np2ptp: failed to persist receipts: {e}");
                    }
                }
                Response::ReceiptAck
            }
            Request::GetReceipts => Response::Receipts(self.receipts.list().to_vec()),
        }
    }

    /// Ensure the full RaptorQ symbol set for `root` is generated and cached.
    /// Returns false if we don't have the content to encode.
    fn ensure_symbols(&mut self, root: Hash) -> bool {
        if self.symbols.contains_key(&root) {
            return true;
        }
        let Some(manifest_bytes) = self.provided.get(&root).cloned() else {
            return false;
        };
        let Ok(manifest) = Manifest::from_nptp(&manifest_bytes) else {
            return false;
        };
        let store = &self.store;
        let Ok(full) = manifest.reconstruct(|h| store.get(h).ok().flatten()) else {
            return false;
        };
        let encoded = np2ptp_fec::encode(&full, FEC_REPAIR_SYMBOLS);
        self.symbols.insert(root, encoded.symbols);
        true
    }

    fn symbol(&mut self, root: Hash, index: usize) -> Option<Vec<u8>> {
        if !self.ensure_symbols(root) {
            return None;
        }
        self.symbols.get(&root)?.get(index).cloned()
    }

    fn symbols_range(&mut self, root: Hash, start: usize, count: usize) -> Vec<Vec<u8>> {
        if !self.ensure_symbols(root) {
            return Vec::new();
        }
        let all = self.symbols.get(&root).expect("just ensured");
        if start >= all.len() {
            return Vec::new();
        }
        all[start..(start + count).min(all.len())].to_vec()
    }

    fn on_kad(&mut self, event: kad::Event) {
        let kad::Event::OutboundQueryProgressed { id, result, step, .. } = event else {
            return;
        };
        match result {
            kad::QueryResult::GetProviders(Ok(ok)) => {
                if let Some((acc, _)) = self.pending_providers.get_mut(&id) {
                    if let kad::GetProvidersOk::FoundProviders { providers, .. } = ok {
                        for p in providers {
                            if !acc.contains(&p) {
                                acc.push(p);
                            }
                        }
                    }
                }
                if step.last {
                    if let Some((acc, reply)) = self.pending_providers.remove(&id) {
                        let _ = reply.send(acc);
                    }
                }
            }
            kad::QueryResult::GetRecord(Ok(ok)) => {
                if let kad::GetRecordOk::FoundRecord(rec) = ok {
                    if let Some(reply) = self.pending_get.remove(&id) {
                        let _ = reply.send(Some(rec.record.value));
                    }
                } else if step.last {
                    if let Some(reply) = self.pending_get.remove(&id) {
                        let _ = reply.send(None);
                    }
                }
            }
            kad::QueryResult::GetRecord(Err(_)) => {
                if let Some(reply) = self.pending_get.remove(&id) {
                    let _ = reply.send(None);
                }
            }
            kad::QueryResult::PutRecord(res) => {
                if let Some(reply) = self.pending_put.remove(&id) {
                    let _ = reply.send(res.is_ok());
                }
            }
            _ => {}
        }
    }
}
