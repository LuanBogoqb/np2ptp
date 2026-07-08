# Signed Receipt Exchange Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire `np2ptp-rep`'s already-built Ed25519 identity/signed-receipt/ledger primitives into `np2ptp-net` so a server's reputation survives restarts and travels to peers it has never directly transacted with.

**Architecture:** `serve`'s already-persisted Ed25519 seed doubles as a `np2ptp_rep::Identity`. Two new request/response messages (`SubmitReceipt`, `GetReceipts`) ride the existing CBOR request-response protocol. A client signs one receipt per completed download and sends it to the server; a server persists what it's been sent and, on first contact with a peer it has no ledger history for, pulls that peer's receipt bag and credits it — using the peer's public key (already exchanged via `identify`) to confirm a presented receipt really is about the peer presenting it.

**Tech Stack:** Rust, libp2p 0.55 (`request_response::cbor`, `identify`), `ed25519-dalek`, `bincode`, `tokio`.

## Global Constraints

- No new libp2p protocol/stream — reuse the existing `request_response::cbor::Behaviour<Request, Response>`.
- No second keypair — `np2ptp_rep::Identity` is always derived from the same 32-byte seed already used for the libp2p `Keypair`.
- `get`/`fetch` keep using a fresh, non-persisted identity every run; this plan does not change their CLI call sites at all — persistence is gated purely on whether `Network::spawn`'s `seed` argument is `Some` or `None`.
- Receipt bag cap is 50 entries, keeping the highest-`bytes` ones.
- A peer's receipt bag is pulled (`GetReceipts`) at most once per peer per process lifetime, gated on "no existing ledger entry for this peer yet," checked at the moment `identify` reveals that peer's public key.
- `reputation()` becomes `served_to_us + credited_by_receipts - we_served` for every `Ledger<K>`, not just the net-facing one.
- Keep `cargo test --workspace` green and `cargo clippy --workspace --all-targets` at 0 warnings after every task.
- On Windows, pass commit messages via `git commit -F <file>` (PowerShell mangles quotes in `-m`).

---

### Task 1: `np2ptp-rep` — reputation formula, receipt crediting, PeerId construction

**Files:**
- Modify: `crates/np2ptp-rep/src/ledger.rs`
- Modify: `crates/np2ptp-rep/src/identity.rs`

**Interfaces:**
- Produces: `Ledger<K>::credit_receipt(&mut self, peer: K, bytes: u64)` (generic over any `K`, no longer requiring `K = PeerId`) — later tasks in `np2ptp-net` call this keyed by the libp2p `PeerId`.
- Produces: `Ledger<K>::reputation()` now includes `credited_by_receipts`.
- Produces: `identity::PeerId::from_bytes(bytes: [u8; 32]) -> PeerId` — later tasks build a `rep::PeerId` from a raw Ed25519 public key learned via libp2p's `identify`.

- [ ] **Step 1: Write the failing tests**

In `crates/np2ptp-rep/src/ledger.rs`, add to the existing `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn credited_receipts_add_to_reputation() {
        let mut l = Ledger::new();
        let client = Identity::from_seed([9u8; 32]);
        let peer = pid(6);
        let r = Receipt::issue(&client, peer, 5000, 1);
        assert!(l.apply_receipt(&r));
        assert_eq!(l.reputation(&peer), 5000);
    }

    #[test]
    fn credit_receipt_adds_without_needing_a_full_receipt_object() {
        let mut l: Ledger<PeerId> = Ledger::new();
        let peer = pid(7);
        l.credit_receipt(peer, 2500);
        assert_eq!(l.counters(&peer).credited_by_receipts, 2500);
        assert_eq!(l.reputation(&peer), 2500);
    }
```

In `crates/np2ptp-rep/src/identity.rs`, add to the existing `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn from_bytes_round_trips_with_a_public_key() {
        let id = Identity::from_seed([11u8; 32]);
        let original = id.peer_id();
        let rebuilt = PeerId::from_bytes(*original.as_bytes());
        assert_eq!(rebuilt, original);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p np2ptp-rep credited_receipts_add_to_reputation credit_receipt_adds_without_needing_a_full_receipt_object from_bytes_round_trips_with_a_public_key`
Expected: FAIL to compile — `credit_receipt` and `PeerId::from_bytes` don't exist yet.

- [ ] **Step 3: Implement**

In `crates/np2ptp-rep/src/ledger.rs`, replace the `reputation` method:

```rust
    /// Reciprocity score: how much a peer has given us (directly, or vouched
    /// for by a valid third-party receipt) beyond what we've given them.
    /// Positive = net giver (favor it), negative = net taker (choke it).
    pub fn reputation(&self, peer: &K) -> i64 {
        let c = self.counters(peer);
        c.served_to_us as i64 + c.credited_by_receipts as i64 - c.we_served as i64
    }
```

Add a new method right after `record_served` in the same generic `impl<K> Ledger<K> where K: Eq + Hash + Clone + Ord` block:

```rust
    /// Credit `peer` with `bytes` on the strength of a receipt whose
    /// cryptographic validity the caller has already checked — this method
    /// itself does no verification, so callers must call it only after
    /// confirming the receipt is genuinely about `peer`.
    pub fn credit_receipt(&mut self, peer: K, bytes: u64) {
        self.peers.entry(peer).or_default().credited_by_receipts += bytes;
    }
```

In `crates/np2ptp-rep/src/identity.rs`, add a constructor next to `from_hex` in `impl PeerId`:

```rust
    /// Build a `PeerId` from a raw 32-byte Ed25519 public key obtained from
    /// somewhere other than an `Identity` of our own (e.g. a peer's public
    /// key learned via a transport's own handshake).
    pub fn from_bytes(bytes: [u8; 32]) -> PeerId {
        PeerId(bytes)
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p np2ptp-rep`
Expected: PASS — all existing tests plus the 3 new ones (existing `reputation_reflects_net_contribution`, `unchoke_favors_reciprocators_over_leeches`, `apply_receipt_credits_only_when_valid` etc. are unaffected since they never set `credited_by_receipts` alongside other fields in a way the new formula changes).

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-rep/src/ledger.rs crates/np2ptp-rep/src/identity.rs
git commit -F- <<'EOF'
feat(rep): count third-party receipts toward reputation

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
```

---

### Task 2: `np2ptp-net` — receipt bag persistence + store root accessor + dependencies

**Files:**
- Modify: `crates/np2ptp-net/Cargo.toml`
- Modify: `crates/np2ptp-store/src/lib.rs`
- Create: `crates/np2ptp-net/src/receipts.rs`

**Interfaces:**
- Consumes: `np2ptp_rep::Receipt` (already exists, `Serialize`/`Deserialize`/`Clone`).
- Produces: `Store::root(&self) -> PathBuf` — the directory a `Store` was opened with.
- Produces: `receipts::ReceiptBag` — `ReceiptBag::new()`, `ReceiptBag::open(path) -> Result<ReceiptBag, ReceiptBagError>`, `ReceiptBag::insert(&mut self, r: Receipt)` (keeps only the 50 highest-`bytes` entries), `ReceiptBag::list(&self) -> &[Receipt]`, `ReceiptBag::save(&self) -> Result<(), ReceiptBagError>`. Task 3 wires this into `Network`.

- [ ] **Step 1: Add dependencies**

In `crates/np2ptp-net/Cargo.toml`, in the `[dependencies]` table, add two new lines and extend the `libp2p` feature list:

```toml
bincode = { workspace = true }
getrandom = "0.2"
```

Change the `libp2p` dependency's `features` array to include `"serde"` (needed so `libp2p::PeerId` can be used as a persisted `Ledger` key in Task 3):

```toml
libp2p = { version = "0.55", features = [
    "tokio",
    "quic",
    "kad",
    "identify",
    "request-response",
    "cbor",
    "macros",
    "relay",
    "dcutr",
    "autonat",
    "upnp",
    "noise",
    "yamux",
    "serde",
] }
```

- [ ] **Step 2: Write the failing test for `Store::root`**

In `crates/np2ptp-store/src/lib.rs`, add to the existing `#[cfg(test)] mod tests`:

```rust
    #[test]
    fn root_returns_the_directory_passed_to_open() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        assert_eq!(store.root(), dir.path());
    }
```

Run: `cargo test -p np2ptp-store root_returns_the_directory_passed_to_open`
Expected: FAIL to compile — `Store::root` doesn't exist yet.

- [ ] **Step 3: Implement `Store::root`**

In `crates/np2ptp-store/src/lib.rs`, add this method to `impl Store`, right after `open`:

```rust
    /// The directory this store was opened with (what was passed to
    /// [`Store::open`]) — used by callers that need to keep sidecar files
    /// (e.g. a persisted network identity or ledger) next to the store.
    pub fn root(&self) -> PathBuf {
        self.objects.parent().expect("objects is always <dir>/objects").to_path_buf()
    }
```

Run: `cargo test -p np2ptp-store`
Expected: PASS.

- [ ] **Step 4: Write the failing tests for `ReceiptBag`**

Create `crates/np2ptp-net/src/receipts.rs`:

```rust
//! Receipts collected *about this node* — proof, signed by past clients,
//! that this node served them bytes. Presented to a new peer on request
//! (`GetReceipts`) so reputation travels even to peers with no direct
//! history, instead of resetting to zero on every new connection.

use std::fs;
use std::path::{Path, PathBuf};

use np2ptp_rep::Receipt;

/// Keep at most this many receipts, favoring the highest-value ones.
const MAX_RECEIPTS: usize = 50;

#[derive(Debug, thiserror::Error)]
pub enum ReceiptBagError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization: {0}")]
    Codec(#[from] bincode::Error),
}

pub struct ReceiptBag {
    receipts: Vec<Receipt>,
    path: Option<PathBuf>,
}

impl ReceiptBag {
    pub fn new() -> ReceiptBag {
        ReceiptBag { receipts: Vec::new(), path: None }
    }

    /// Open a bag persisted at `path`, or start empty and bind to it.
    pub fn open(path: impl AsRef<Path>) -> Result<ReceiptBag, ReceiptBagError> {
        let path = path.as_ref().to_path_buf();
        let receipts = match fs::read(&path) {
            Ok(bytes) => bincode::deserialize(&bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(ReceiptBag { receipts, path: Some(path) })
    }

    /// Insert `r`, then keep only the `MAX_RECEIPTS` highest-`bytes` entries.
    pub fn insert(&mut self, r: Receipt) {
        self.receipts.push(r);
        self.receipts.sort_by(|a, b| b.bytes.cmp(&a.bytes));
        self.receipts.truncate(MAX_RECEIPTS);
    }

    pub fn list(&self) -> &[Receipt] {
        &self.receipts
    }

    /// Persist to the bound path (no-op if created via `new`, without one).
    pub fn save(&self) -> Result<(), ReceiptBagError> {
        if let Some(path) = &self.path {
            let tmp = path.with_extension("tmp");
            fs::write(&tmp, bincode::serialize(&self.receipts)?)?;
            fs::rename(&tmp, path)?;
        }
        Ok(())
    }
}

impl Default for ReceiptBag {
    fn default() -> Self {
        ReceiptBag::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use np2ptp_rep::Identity;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp_path() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("np2ptp-net-receipts-{}-{}.bin", std::process::id(), n))
    }

    #[test]
    fn open_missing_path_starts_empty() {
        let bag = ReceiptBag::open(tmp_path()).unwrap();
        assert!(bag.list().is_empty());
    }

    #[test]
    fn insert_keeps_only_the_highest_value_receipts_once_over_cap() {
        let mut bag = ReceiptBag::new();
        let client = Identity::from_seed([1u8; 32]);
        let server = Identity::from_seed([2u8; 32]).peer_id();
        for i in 0..60u64 {
            bag.insert(Receipt::issue(&client, server, i * 100, i));
        }
        assert_eq!(bag.list().len(), MAX_RECEIPTS);
        let min_bytes = bag.list().iter().map(|r| r.bytes).min().unwrap();
        assert_eq!(min_bytes, 1000); // kept i*100 for i in 10..=59
    }

    #[test]
    fn save_and_reopen_round_trips() {
        let path = tmp_path();
        let client = Identity::from_seed([3u8; 32]);
        let server = Identity::from_seed([4u8; 32]).peer_id();
        {
            let mut bag = ReceiptBag::open(&path).unwrap();
            bag.insert(Receipt::issue(&client, server, 4096, 1));
            bag.save().unwrap();
        }
        let reopened = ReceiptBag::open(&path).unwrap();
        assert_eq!(reopened.list().len(), 1);
        assert_eq!(reopened.list()[0].bytes, 4096);
        let _ = fs::remove_file(&path);
    }
}
```

Register the module in `crates/np2ptp-net/src/lib.rs` by adding this line right after the existing `//!` doc comment block, before the first `use` statement:

```rust
mod receipts;
```

Run: `cargo test -p np2ptp-net --lib`
Expected: PASS — this is a self-contained new module with its own tests; nothing else references it yet, so it compiles and runs standalone.

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-net/Cargo.toml crates/np2ptp-net/src/lib.rs crates/np2ptp-net/src/receipts.rs crates/np2ptp-store/src/lib.rs
git commit -F- <<'EOF'
feat(net): add receipt bag persistence and Store::root

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
```

---

### Task 3: `np2ptp-net` — wire protocol, identity bridging, and automatic receipt exchange

**Files:**
- Modify: `crates/np2ptp-net/src/lib.rs`
- Modify: `crates/np2ptp-net/tests/two_nodes.rs`

**Interfaces:**
- Consumes: `crate::receipts::{ReceiptBag, ReceiptBagError}` (Task 2), `Ledger::credit_receipt`, `identity::PeerId::from_bytes` (Task 1), `Store::root` (Task 2).
- Produces: `Network::submit_receipt(&self, peer: PeerId, bytes: u64) -> Result<(), NetError>`. `Network::spawn`'s persistence behavior: ledger + receipt bag are loaded from `{store.root()}/ledger.bin` and `{store.root()}/receipts.bin` whenever `seed` is `Some`, and are fresh/unpersisted when `seed` is `None` — no change to `Network::spawn`'s signature.

This is one task because `Request`/`Response`'s new variants, `EventLoop`'s new constructor/fields, and every match over them change together — splitting it would leave the crate non-compiling (a non-exhaustive `match`) between commits.

- [ ] **Step 1: Update imports and errors**

In `crates/np2ptp-net/src/lib.rs`, replace this line:

```rust
use np2ptp_rep::Ledger;
```

with:

```rust
use np2ptp_rep::{Identity, Ledger, Receipt};
use crate::receipts::{ReceiptBag, ReceiptBagError};
```

Add two variants to `NetError` (right after the existing `Store` variant):

```rust
    #[error("ledger persistence: {0}")]
    Ledger(#[from] np2ptp_rep::LedgerError),
    #[error("receipt bag persistence: {0}")]
    Receipts(#[from] ReceiptBagError),
```

- [ ] **Step 2: Extend the wire protocol**

In `crates/np2ptp-net/src/lib.rs`, replace the `Request` enum:

```rust
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
```

Replace the `Response` enum:

```rust
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
```

- [ ] **Step 3: Add the `SubmitReceipt` command and handle method**

In the `Command` enum, add a variant right after `SetChokeThreshold`:

```rust
    /// Sign and send a receipt crediting `peer` for `bytes` this node
    /// received from it. Fire-and-forget: the caller doesn't wait for the
    /// server's acknowledgement.
    SubmitReceipt { peer: PeerId, bytes: u64 },
```

In `impl Network`, add a new method right after `set_choke_threshold`:

```rust
    /// Credit `peer` with a signed receipt for `bytes` this node received
    /// from it. Called automatically after a successful download; best
    /// effort — a delivery failure does not undo the download.
    pub async fn submit_receipt(&self, peer: PeerId, bytes: u64) -> Result<(), NetError> {
        self.send(Command::SubmitReceipt { peer, bytes }).await
    }
```

- [ ] **Step 4: Issue a receipt at the end of a successful download**

In `download_with_progress`, replace the body from the `let mut stream = ...` line to the end:

```rust
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
```

In `download_fec_with_progress`, replace the body from `let mut symbols: Vec<Vec<u8>> = Vec::new();` to the end:

```rust
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
```

- [ ] **Step 5: Rework `Network::spawn` to build a paired identity and load persisted state**

Replace the start of `Network::spawn`, from `pub fn spawn` through the `local_peer_id` line:

```rust
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
```

Leave the swarm-building block (`let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)` through `.set_mode(Some(kad::Mode::Server));`) unchanged.

Replace the tail of `spawn`, from `let (cmd_tx, cmd_rx) = mpsc::channel(64);` to the end:

```rust
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
```

- [ ] **Step 6: Extend `EventLoop`'s state and constructor**

In the `EventLoop` struct definition, add fields after `symbols`:

```rust
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
    /// Monotonic counter for receipts this node issues (see `Receipt::epoch`).
    next_receipt_epoch: u64,
```

Add this enum right before the `EventLoop` struct definition:

```rust
/// What an internally-initiated outbound request (one `EventLoop` sent to
/// itself, not on behalf of a `Network` handle call) was for.
enum InternalRequest {
    GetReceipts,
    SubmitReceiptAck,
}
```

Replace `EventLoop::new`:

```rust
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
            next_receipt_epoch: 0,
        }
    }
```

- [ ] **Step 7: Handle the new command**

In `on_command`, add a new match arm after `SetChokeThreshold`:

```rust
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
```

- [ ] **Step 8: Bridge libp2p and rep identities via `identify`, and pull a new peer's receipts**

Replace the `identify::Event::Received` arm in `on_event`:

```rust
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
                    if self.ledger.counters(&peer_id) == Counters::default() {
                        let id = self.swarm.behaviour_mut().rr.send_request(&peer_id, Request::GetReceipts);
                        self.pending_internal.insert(id, InternalRequest::GetReceipts);
                    }
                }
            }
```

- [ ] **Step 9: Dispatch internal responses and add the receipts-response handler**

In `on_rr`, replace the `Message::Response` arm:

```rust
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
```

Add this method right after `on_rr`:

```rust
    /// Handle a peer's answer to our `GetReceipts` pull: verify each receipt
    /// and, only if it's genuinely about the peer that presented it, credit
    /// that peer in our own ledger.
    fn handle_receipts_response(&mut self, peer: PeerId, response: Response) {
        let Response::Receipts(receipts) = response else { return };
        let Some(&expected_server) = self.rep_peers.get(&peer) else { return };
        for r in receipts {
            if r.verify() && r.server == expected_server {
                self.ledger.credit_receipt(peer, r.bytes);
            }
        }
        if let Err(e) = self.ledger.save() {
            eprintln!("np2ptp: failed to persist ledger: {e}");
        }
    }
```

- [ ] **Step 10: Serve the two new request kinds**

In `serve`, add two match arms (order doesn't matter; append after `Request::Symbols`):

```rust
            Request::SubmitReceipt(receipt) => {
                if receipt.verify() {
                    self.receipts.insert(receipt);
                    if let Err(e) = self.receipts.save() {
                        eprintln!("np2ptp: failed to persist receipts: {e}");
                    }
                }
                Response::ReceiptAck
            }
            Request::GetReceipts => Response::Receipts(self.receipts.list().to_vec()),
```

- [ ] **Step 11: Run the existing test suite**

Run: `cargo test -p np2ptp-net`
Expected: PASS — every existing test in `crates/np2ptp-net/tests/two_nodes.rs` and `src/lib.rs` still passes unchanged; `Ledger<PeerId>` is still keyed the same way it always was, only the formula and construction path changed.

- [ ] **Step 12: Add the end-to-end receipt test**

Append to `crates/np2ptp-net/tests/two_nodes.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receipt_from_a_download_lets_a_third_party_credit_the_server() {
    // A serves some content; B downloads it, which should automatically send
    // A a signed receipt crediting A for the bytes served.
    let a_dir = TmpDir::new();
    let a_store = Store::open(a_dir.path()).unwrap();
    let data = sample(300_000, 70);
    let manifest = a_store.ingest(&data, None).unwrap();
    let root = manifest.root;

    let a = Network::spawn(a_store, Some([70u8; 32])).unwrap();
    a.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let a_addr = first_listen_addr(&a).await;
    let a_peer = a.local_peer_id();
    a.provide(&manifest).await.unwrap();

    let b_dir = TmpDir::new();
    let b = Network::spawn(Store::open(b_dir.path()).unwrap(), Some([71u8; 32])).unwrap();
    let b_store = Store::open(b_dir.path()).unwrap();
    b.dial(a_addr.clone()).await.unwrap();

    for _ in 0..100 {
        if b.download(root, a_peer, &b_store).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // C has no prior history with A. On connecting, C should pull A's receipt
    // bag (which now holds B's receipt crediting A), verify it, and credit A
    // — even though C never transacted with A directly.
    let c_dir = TmpDir::new();
    let c = Network::spawn(Store::open(c_dir.path()).unwrap(), Some([72u8; 32])).unwrap();
    c.dial(a_addr).await.unwrap();

    let mut credited = false;
    for _ in 0..100 {
        if c.reputation(a_peer).await.unwrap() > 0 {
            credited = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(credited, "C should credit A via A's receipt from B, despite no direct history");
}
```

- [ ] **Step 13: Add the choke-bypass-via-receipt test**

The `Receipt::verify()` function itself (signature tampering, server-swapping,
forged signatures) is already exhaustively covered by `np2ptp-rep`'s existing
tests (`tampering_with_bytes_breaks_verification`,
`swapping_the_server_breaks_verification`, `forged_signature_fails`), which
both `serve`'s `SubmitReceipt` handling and `handle_receipts_response` rely
on directly — no new net-level test re-proves that. What *is* still
untested at the network layer is the actual product behavior a receipt
enables: bypassing the choke on first contact. Append to
`crates/np2ptp-net/tests/two_nodes.rs`:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_peer_with_a_receipt_is_not_choked() {
    // B earns a receipt by serving a small file to an earlier downloader.
    let b_dir = TmpDir::new();
    let b_store = Store::open(b_dir.path()).unwrap();
    let voucher_data = sample(50_000, 60);
    let voucher_manifest = b_store.ingest(&voucher_data, None).unwrap();
    let voucher_root = voucher_manifest.root;

    let b = Network::spawn(b_store, Some([60u8; 32])).unwrap();
    b.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let b_addr = first_listen_addr(&b).await;
    let b_peer = b.local_peer_id();
    b.provide(&voucher_manifest).await.unwrap();

    let earlier_dir = TmpDir::new();
    let earlier = Network::spawn(Store::open(earlier_dir.path()).unwrap(), Some([61u8; 32])).unwrap();
    let earlier_store = Store::open(earlier_dir.path()).unwrap();
    earlier.dial(b_addr.clone()).await.unwrap();
    let mut earned = false;
    for _ in 0..100 {
        if earlier.download(voucher_root, b_peer, &earlier_store).await.is_ok() {
            earned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(earned, "setup: B must earn a receipt from the earlier download");
    // Give B's EventLoop a moment to receive and store the auto-submitted receipt.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A strict seed serving the real content: any net-negative peer is
    // choked after its first freebie chunk.
    let data = sample(300_000, 61);
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let seed = Network::spawn(seed_store, Some([62u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();
    seed.set_choke_threshold(0).await.unwrap();

    // B, cold to the seed but carrying a receipt, should not be choked.
    let b_download_dir = TmpDir::new();
    let b_download_store = Store::open(b_download_dir.path()).unwrap();
    b.dial(seed_addr.clone()).await.unwrap();
    let mut b_completed = false;
    for _ in 0..60 {
        if b.download(root, seed_peer, &b_download_store).await.is_ok() {
            b_completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(b_completed, "a peer vouched for by a receipt should not be choked");

    // A, equally cold but with no receipt, should be choked.
    let a_dir = TmpDir::new();
    let a = Network::spawn(Store::open(a_dir.path()).unwrap(), Some([63u8; 32])).unwrap();
    let a_store = Store::open(a_dir.path()).unwrap();
    a.dial(seed_addr).await.unwrap();
    let mut a_completed = false;
    for _ in 0..60 {
        if a.download(root, seed_peer, &a_store).await.is_ok() {
            a_completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!a_completed, "a cold peer with no receipt should still be choked");
}
```

- [ ] **Step 14: Run the full test suite again**

Run: `cargo test -p np2ptp-net`
Expected: PASS, including both new tests: `receipt_from_a_download_lets_a_third_party_credit_the_server` and `cold_peer_with_a_receipt_is_not_choked`.

Run: `cargo clippy -p np2ptp-net --all-targets`
Expected: 0 warnings.

- [ ] **Step 15: Commit**

```bash
git add crates/np2ptp-net/src/lib.rs crates/np2ptp-net/tests/two_nodes.rs
git commit -F- <<'EOF'
feat(net): exchange signed receipts so reputation survives restarts and travels to new peers

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
```

---

### Task 4: `np2ptp-sim` — measure that a receipt bypasses the choke for a peer with no direct history

**Files:**
- Modify: `crates/np2ptp-sim/src/lib.rs`
- Modify: `crates/np2ptp-sim/src/main.rs`

**Interfaces:**
- Consumes: `Network::download`, `Network::set_choke_threshold`, `Network::dial`, `Network::provide` (all already public); the automatic receipt issuance from Task 3 (no direct call needed — it happens inside `download`).
- Produces: `pub async fn receipt_bootstraps_trust() -> ReceiptTrustResult { pub cold_peer_completed: bool, pub vouched_peer_completed: bool }`, wired into `main.rs`'s report.

- [ ] **Step 1: Write the failing test**

In `crates/np2ptp-sim/src/lib.rs`, add to the existing `#[cfg(test)] mod tests`:

```rust
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_receipt_lets_a_cold_peer_bypass_the_choke() {
        let r = receipt_bootstraps_trust().await;
        assert!(r.vouched_peer_completed, "a peer vouched for by a receipt should not be choked");
        assert!(!r.cold_peer_completed, "a peer with no history and no receipt should be choked");
    }
```

Run: `cargo test -p np2ptp-sim a_receipt_lets_a_cold_peer_bypass_the_choke`
Expected: FAIL to compile — `receipt_bootstraps_trust` doesn't exist yet.

- [ ] **Step 2: Implement the scenario**

In `crates/np2ptp-sim/src/lib.rs`, add after the `fec_cost` scenario ends (`FecCostResult { size: data.len(), chunk_ms, fec_ms } }`) and before the `#[cfg(test)] mod tests` block:

```rust
// ---- scenario 5: receipt-bootstrapped trust ------------------------------

#[derive(Debug, Clone, Copy)]
pub struct ReceiptTrustResult {
    pub cold_peer_completed: bool,
    pub vouched_peer_completed: bool,
}

/// A strict server (choke threshold 0, so any net-negative peer is cut off
/// after its first freebie chunk) meets two peers it has no history with:
/// one carries a signed receipt proving it served bytes to someone else
/// earlier; the other has nothing. The vouched-for peer should be credited
/// on first contact (via `GetReceipts`) and finish; the cold peer should be
/// choked like any other unproven leech.
pub async fn receipt_bootstraps_trust() -> ReceiptTrustResult {
    // "B" earns a receipt by serving a small file to some earlier downloader.
    let b_dir = TmpDir::new();
    let b_store = Store::open(b_dir.path()).unwrap();
    let voucher_data = sample(50_000, 20);
    let voucher_manifest = b_store.ingest(&voucher_data, None).unwrap();
    let voucher_root = voucher_manifest.root;

    let b = Network::spawn(b_store, None).unwrap();
    b.listen(QUIC_LISTEN.parse().unwrap()).await.unwrap();
    let b_addr = first_listen_addr(&b).await;
    let b_peer = b.local_peer_id();
    b.provide(&voucher_manifest).await.unwrap();

    let earlier_dir = TmpDir::new();
    let earlier = Network::spawn(Store::open(earlier_dir.path()).unwrap(), None).unwrap();
    let earlier_store = Store::open(earlier_dir.path()).unwrap();
    earlier.dial(b_addr).await.unwrap();
    assert!(
        download_until(&earlier, voucher_root, b_peer, &earlier_store, 100).await,
        "setup: the earlier download must complete so B earns a receipt"
    );
    // Give B's EventLoop a moment to receive and store the auto-submitted receipt.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // The real content everyone wants, served by a strict seed.
    let data = sample(300_000, 21); // multi-chunk
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let seed = Network::spawn(seed_store, None).unwrap();
    seed.listen(QUIC_LISTEN.parse().unwrap()).await.unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();
    seed.set_choke_threshold(0).await.unwrap();

    // B (carrying a receipt) connects cold to the seed.
    let b_download_dir = TmpDir::new();
    let b_download_store = Store::open(b_download_dir.path()).unwrap();
    b.dial(seed_addr.clone()).await.unwrap();
    let vouched_peer_completed = download_until(&b, root, seed_peer, &b_download_store, 60).await;

    // A, an equally cold peer with no receipts at all, connects to the same seed.
    let a_dir = TmpDir::new();
    let a = Network::spawn(Store::open(a_dir.path()).unwrap(), None).unwrap();
    let a_store = Store::open(a_dir.path()).unwrap();
    a.dial(seed_addr).await.unwrap();
    let cold_peer_completed = download_until(&a, root, seed_peer, &a_store, 60).await;

    ReceiptTrustResult { cold_peer_completed, vouched_peer_completed }
}
```

Run: `cargo test -p np2ptp-sim a_receipt_lets_a_cold_peer_bypass_the_choke`
Expected: PASS.

- [ ] **Step 3: Wire the scenario into the report**

In `crates/np2ptp-sim/src/main.rs`, update the import list:

```rust
use np2ptp_sim::{
    dedup, fec_cost, freeride, permanence, receipt_bootstraps_trust, DedupResult, FecCostResult,
    FreerideResult, PermanenceResult, ReceiptTrustResult,
};
```

Add a field to `Results`:

```rust
struct Results {
    dedup: DedupResult,
    perm_with: PermanenceResult,
    perm_without: PermanenceResult,
    freeride_off: FreerideResult,
    freeride_on: FreerideResult,
    receipt_trust: ReceiptTrustResult,
    fec: FecCostResult,
}
```

Add it to the `Results` construction in `main`:

```rust
    let r = Results {
        dedup: dedup(),
        perm_with: permanence(true).await,
        perm_without: permanence(false).await,
        freeride_off: freeride(false).await,
        freeride_on: freeride(true).await,
        receipt_trust: receipt_bootstraps_trust().await,
        fec: fec_cost().await,
    };
```

In `print_console`, add a line after the free-riding one:

```rust
    println!("[4] Receipt-bootstrapped trust: cold peer (no receipt) completes = {}, vouched peer (receipt) completes = {}",
        b(r.receipt_trust.cold_peer_completed), b(r.receipt_trust.vouched_peer_completed));
```

Renumber the existing `[4] FEC cost` line to `[5]`.

In `build_csv`, add two rows after the freeride ones:

```rust
    s.push_str(&format!("receipt_trust,cold_peer_completes,{}\n", r.receipt_trust.cold_peer_completed as u8));
    s.push_str(&format!("receipt_trust,vouched_peer_completes,{}\n", r.receipt_trust.vouched_peer_completed as u8));
```

In `build_markdown`, add a table row after the free-riding rows (in the format string, right after the `Free-riding | leech completes - choke ON` row) and a matching format argument in the same relative position, and add an interpretation bullet after the Incentives one:

```
         | Receipt-bootstrapped trust | cold peer (no receipt) completes | **{}** |\n\
         | Receipt-bootstrapped trust | vouched peer (receipt) completes | **{}** |\n\
```

```rust
         - **Receipt-bootstrapped trust** - a peer that earned a signed receipt serving\n  \
           someone else earlier is credited by a brand-new peer on first contact and is not\n  \
           choked, while an equally cold peer with no receipt is — reputation that travels,\n  \
           not memoryless tit-for-tat.\n\
```

with the two matching `b(r.receipt_trust.cold_peer_completed)` / `b(r.receipt_trust.vouched_peer_completed)` arguments added to the `format!` call's argument list, in the same order as the two new `{}` placeholders.

- [ ] **Step 4: Run the full report and workspace check**

Run: `cargo run --release -p np2ptp-sim`
Expected: prints all 5 scenarios including `[4] Receipt-bootstrapped trust: cold peer (no receipt) completes = no, vouched peer (receipt) completes = yes`, and writes `reports/REPORT.md` / `reports/results.csv` including the new rows.

Run: `cargo test --workspace`
Expected: all tests green (~90 tests: the prior ~80 plus 3 new `np2ptp-rep` tests, 3 new `np2ptp-net` unit tests in `receipts.rs`, 2 new `np2ptp-net` integration tests in `two_nodes.rs`, 1 new `np2ptp-store` test, and 1 new `np2ptp-sim` test).

Run: `cargo clippy --workspace --all-targets`
Expected: 0 warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-sim/src/lib.rs crates/np2ptp-sim/src/main.rs
git commit -F- <<'EOF'
feat(sim): measure receipt-bootstrapped trust bypassing the choke

Co-Authored-By: Claude Sonnet 5 <noreply@anthropic.com>
EOF
```
