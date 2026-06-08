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

use std::collections::HashMap;

use futures::StreamExt;
use libp2p::{
    autonat, dcutr, identify, identity, kad, noise, relay,
    request_response::{self, ProtocolSupport},
    swarm::SwarmEvent,
    yamux, StreamProtocol, Swarm,
};
use np2ptp_core::{Hash, Manifest};
use np2ptp_rep::Ledger;
use np2ptp_store::Store;
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

// Re-export the libp2p types that appear in this crate's public API, so callers
// (and tests) don't need a direct libp2p dependency.
pub use libp2p::{Multiaddr, PeerId};

/// Extract the `PeerId` from a multiaddr that ends in `/p2p/<peer-id>`, if present.
pub fn peer_id_from_multiaddr(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|proto| match proto {
        libp2p::multiaddr::Protocol::P2p(id) => Some(id),
        _ => None,
    })
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

/// A request: a manifest by content id, a chunk by hash, or a RaptorQ repair
/// symbol for a content id by index.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Manifest([u8; 32]),
    Chunk([u8; 32]),
    Symbol { root: [u8; 32], index: u32 },
    /// A contiguous batch of RaptorQ symbols `[start, start+count)` — far fewer
    /// round-trips than fetching symbols one at a time.
    Symbols { root: [u8; 32], start: u32, count: u32 },
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
}

/// How many repair symbols a seeder generates per content, on top of the source
/// symbols. More = more resilience to symbol loss at the cost of memory.
const FEC_REPAIR_SYMBOLS: u32 = 64;

/// How many symbols a FEC download requests per round-trip.
const FEC_BATCH: u32 = 128;

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
}

/// Commands sent from a [`Network`] handle to the swarm task.
enum Command {
    Listen { addr: Multiaddr, reply: oneshot::Sender<Result<(), NetError>> },
    Dial { addr: Multiaddr, reply: oneshot::Sender<Result<(), NetError>> },
    Listeners { reply: oneshot::Sender<Vec<Multiaddr>> },
    AddPeer { peer: PeerId, addr: Multiaddr },
    AddExternalAddress { addr: Multiaddr },
    Provide { root: Hash, manifest_bytes: Vec<u8> },
    FindProviders { root: Hash, reply: oneshot::Sender<Vec<PeerId>> },
    Request { peer: PeerId, request: Request, reply: oneshot::Sender<Result<Response, NetError>> },
    SetChokeThreshold { threshold: i64 },
    Reputation { peer: PeerId, reply: oneshot::Sender<i64> },
    PutRecord { key: Vec<u8>, value: Vec<u8>, reply: oneshot::Sender<bool> },
    GetRecord { key: Vec<u8>, reply: oneshot::Sender<Option<Vec<u8>>> },
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
        let keypair = match seed {
            Some(mut bytes) => identity::Keypair::ed25519_from_bytes(&mut bytes)
                .map_err(|e| NetError::Build(e.to_string()))?,
            None => identity::Keypair::generate_ed25519(),
        };
        let local_peer_id = keypair.public().to_peer_id();

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
                    relay_server: relay::Behaviour::new(peer_id, relay::Config::default()),
                    relay_client,
                    dcutr: dcutr::Behaviour::new(peer_id),
                    autonat: autonat::Behaviour::new(peer_id, autonat::Config::default()),
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

        let (cmd_tx, cmd_rx) = mpsc::channel(64);
        tokio::spawn(EventLoop::new(swarm, store, cmd_rx).run());
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

    /// The addresses this node is actually listening on.
    pub async fn listeners(&self) -> Result<Vec<Multiaddr>, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Listeners { reply }).await?;
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

    /// This node's recorded reputation for `peer` (positive = net giver).
    pub async fn reputation(&self, peer: PeerId) -> Result<i64, NetError> {
        let (reply, rx) = oneshot::channel();
        self.send(Command::Reputation { peer, reply }).await?;
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
        let manifest = self.get_manifest(provider, root).await?;
        let config = np2ptp_fec::config_for(manifest.total_size, np2ptp_fec::DEFAULT_SYMBOL_SIZE);

        // Only attempt a decode once we likely have enough symbols (decoding is
        // the expensive step, so don't retry it after every batch).
        let symbol_size = np2ptp_fec::DEFAULT_SYMBOL_SIZE as usize;
        let need = (manifest.total_size as usize).div_ceil(symbol_size).max(1);

        let mut symbols: Vec<Vec<u8>> = Vec::new();
        let mut start = 0u32;
        let decoded = loop {
            let batch = self.fetch_symbols(provider, root, start, FEC_BATCH).await?;
            let exhausted = batch.is_empty();
            start += batch.len() as u32;
            symbols.extend(batch);

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

        while let Some(result) = stream.next().await {
            let (i, bytes) = result?;
            if !manifest.chunk_hash_ok(i, &bytes) {
                return Err(NetError::BadChunk);
            }
            into.put(&bytes)?;
        }
        Ok(manifest)
    }
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
}

impl EventLoop {
    fn new(swarm: Swarm<Behaviour>, store: Store, cmd_rx: mpsc::Receiver<Command>) -> EventLoop {
        EventLoop {
            swarm,
            store,
            cmd_rx,
            provided: HashMap::new(),
            pending_req: HashMap::new(),
            pending_providers: HashMap::new(),
            pending_put: HashMap::new(),
            pending_get: HashMap::new(),
            ledger: Ledger::new(),
            choke_threshold: i64::MAX, // no choking until configured
            symbols: HashMap::new(),
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
            }
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
                    if let Some(reply) = self.pending_req.remove(&request_id) {
                        let _ = reply.send(Ok(response));
                    }
                }
            },
            request_response::Event::OutboundFailure { request_id, .. } => {
                if let Some(reply) = self.pending_req.remove(&request_id) {
                    let _ = reply.send(Err(NetError::RequestFailed));
                }
            }
            _ => {}
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
