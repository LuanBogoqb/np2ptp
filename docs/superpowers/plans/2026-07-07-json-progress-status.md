# JSON Progress/Status Output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an opt-in `--json` flag to `pack`/`get`/`fetch`/`serve` that emits NDJSON progress/status/result/error events on stdout, so a non-Rust launcher (e.g. a C# game launcher spawning `np2ptp` as a child process) can drive a progress bar and read `serve`'s live peer/tracker/byte-count status without any FFI.

**Architecture:** Every chunking/download function gains an additive `_with_progress` sibling taking a `FnMut` callback — no existing public signature changes, so nothing already written breaks. The CLI (`np2ptp-node/src/main.rs`) is the only layer that knows about JSON; it either calls the plain functions (text mode, unchanged) or the `_with_progress` ones with a callback that prints throttled `serde_json::json!(...)` lines. `serve` additionally gets a second, faster `tokio::time::interval` for periodic status ticks, alongside its existing tracker re-announce interval.

**Tech Stack:** Rust, existing `serde_json` dependency (no new crates), `tokio::time::interval`, `tokio::select!`.

## Global Constraints

- No existing public function signature changes anywhere in `np2ptp-store`, `np2ptp-node` (lib.rs), or `np2ptp-net` — only additive `_with_progress` functions and new read-only accessors (`connected_peers`, `ledger_totals`, `Ledger::totals`).
- Without `--json`, every command's behavior and output must be byte-for-byte identical to before this plan.
- With `--json`: every line printed is one JSON object with `event` and `op` fields; progress lines are throttled to at most one per ~100ms (never one line per chunk); errors are also emitted as one JSON line to stdout (in addition to the existing generic stderr print in `run()`, which is left untouched); exit codes are unchanged.
- All work happens on the `dev` branch (already checked out) and is committed there; do not touch `main`.
- Spec: `docs/superpowers/specs/2026-07-07-json-progress-status-design.md` — re-read it if anything below is ambiguous.

---

### Task 1: `Ledger::totals()` — aggregate peer counters

**Files:**
- Modify: `crates/np2ptp-rep/src/ledger.rs`
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `impl<K: Eq + Hash + Clone + Ord> Ledger<K> { pub fn totals(&self) -> Counters }` — sums `served_to_us`, `we_served`, `credited_by_receipts` across every peer entry.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/np2ptp-rep/src/ledger.rs` (after `unchoke_favors_reciprocators_over_leeches`):

```rust
#[test]
fn totals_sums_across_all_peers() {
    let mut l = Ledger::new();
    let a = pid(1);
    let b = pid(2);
    l.record_received(a, 1000);
    l.record_served(a, 100);
    l.record_received(b, 500);
    l.record_served(b, 50);
    let t = l.totals();
    assert_eq!(t.served_to_us, 1500);
    assert_eq!(t.we_served, 150);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p np2ptp-rep totals_sums_across_all_peers`
Expected: FAIL to compile — `no method named 'totals' found for struct 'Ledger<PeerId>'`

- [ ] **Step 3: Write minimal implementation**

In `crates/np2ptp-rep/src/ledger.rs`, in the `impl<K> Ledger<K> where K: Eq + Hash + Clone + Ord` block, add after `pub fn counters(...)`:

```rust
    /// Sum of every peer's counters — one aggregate figure (e.g. "total bytes
    /// served across all peers") instead of per-peer detail.
    pub fn totals(&self) -> Counters {
        let mut total = Counters::default();
        for c in self.peers.values() {
            total.served_to_us += c.served_to_us;
            total.we_served += c.we_served;
            total.credited_by_receipts += c.credited_by_receipts;
        }
        total
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p np2ptp-rep totals_sums_across_all_peers`
Expected: PASS (1 passed)

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-rep/src/ledger.rs
git commit -m "np2ptp-rep: add Ledger::totals() for aggregate peer stats"
```

---

### Task 2: `Network::connected_peers()` and `Network::ledger_totals()`

**Files:**
- Modify: `crates/np2ptp-net/src/lib.rs`
- Test: `crates/np2ptp-net/tests/two_nodes.rs`

**Interfaces:**
- Consumes: `Task 1`'s `Ledger::totals() -> Counters`.
- Produces: `Network::connected_peers(&self) -> Result<Vec<PeerId>, NetError>`, `Network::ledger_totals(&self) -> Result<Counters, NetError>`, and a re-export `pub use np2ptp_rep::Counters;` so downstream crates (`np2ptp-node`, which does not depend on `np2ptp-rep`) can name the type as `np2ptp_net::Counters`.

- [ ] **Step 1: Write the failing test**

Add to `crates/np2ptp-net/tests/two_nodes.rs` (after `discovers_provider_via_dht`):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connected_peers_and_ledger_totals_reflect_a_transfer() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 6);
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;

    let seed = Network::spawn(seed_store, Some([90u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([91u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    let client_peer = client.local_peer_id();
    client.dial(seed_addr).await.unwrap();

    for _ in 0..100 {
        if client.download(root, seed_peer, &client_store).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Both sides should see the other in their connected-peer list.
    let mut seed_sees_client = false;
    for _ in 0..50 {
        if seed.connected_peers().await.unwrap().contains(&client_peer) {
            seed_sees_client = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(seed_sees_client, "seed should list the client as connected");

    // The seed served the whole file to someone; ledger totals must reflect it.
    let totals = seed.ledger_totals().await.unwrap();
    assert_eq!(totals.we_served, data.len() as u64);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p np2ptp-net --test two_nodes connected_peers_and_ledger_totals_reflect_a_transfer`
Expected: FAIL to compile — `no method named 'connected_peers' found for struct 'Network'`

- [ ] **Step 3: Write minimal implementation**

In `crates/np2ptp-net/src/lib.rs`, add to the `Command` enum (after `GetRecord`):

```rust
    ConnectedPeers { reply: oneshot::Sender<Vec<PeerId>> },
    LedgerTotals { reply: oneshot::Sender<np2ptp_rep::Counters> },
```

Add near the top, alongside the existing re-export line `pub use libp2p::{Multiaddr, PeerId};`:

```rust
pub use np2ptp_rep::Counters;
```

Add two public methods on `impl Network` (after `pub async fn reputation(...)`):

```rust
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
```

In the `EventLoop::on_command` match (next to the `Command::Reputation { peer, reply }` arm), add:

```rust
            Command::ConnectedPeers { reply } => {
                let _ = reply.send(self.swarm.connected_peers().cloned().collect());
            }
            Command::LedgerTotals { reply } => {
                let _ = reply.send(self.ledger.totals());
            }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p np2ptp-net --test two_nodes connected_peers_and_ledger_totals_reflect_a_transfer`
Expected: PASS (1 passed)

Also run the full net test suite to confirm nothing else broke:
Run: `cargo test -p np2ptp-net`
Expected: all pass (existing tests + the new one)

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-net/src/lib.rs crates/np2ptp-net/tests/two_nodes.rs
git commit -m "np2ptp-net: add connected_peers() and ledger_totals() accessors"
```

---

### Task 3: Store progress hooks for `pack`

**Files:**
- Modify: `crates/np2ptp-store/src/lib.rs`
- Test: same file, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `Store::ingest_tree_files_with_progress(&self, files: &[(String, PathBuf)], name: Option<String>, on_progress: impl FnMut(u64, u64, bool)) -> Result<Manifest, StoreError>`
  - `Store::ingest_tree_files_no_copy_with_progress(&self, files: &[(String, PathBuf)], name: Option<String>, on_progress: impl FnMut(u64, u64, bool)) -> Result<Manifest, StoreError>`
  - Callback signature is `(bytes_done, bytes_total, chunk_was_new)`, cumulative across every file in `files`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/np2ptp-store/src/lib.rs` (after `no_copy_pack_detects_a_changed_source_file`):

```rust
    #[test]
    fn pack_with_progress_reports_bytes_and_dedup_flag() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();

        let fdir = TmpDir::new();
        let fpath = fdir.path().join("f.bin");
        let data = sample(500_000, 20);
        std::fs::write(&fpath, &data).unwrap();

        let mut calls: Vec<(u64, u64, bool)> = Vec::new();
        let manifest = store
            .ingest_tree_files_with_progress(
                &[("f.bin".to_string(), fpath.clone())],
                None,
                |done, total, is_new| calls.push((done, total, is_new)),
            )
            .unwrap();

        assert!(!calls.is_empty());
        // Every call reports the same (correct) total; done is monotonic and
        // reaches it on the last call.
        let total = calls[0].1;
        assert_eq!(total, data.len() as u64);
        assert!(calls.windows(2).all(|w| w[0].0 <= w[1].0));
        assert_eq!(calls.last().unwrap().0, total);
        // First pack of brand-new content: every chunk is new.
        assert!(calls.iter().all(|(_, _, is_new)| *is_new));

        // Re-packing the identical file into the SAME store must report every
        // chunk as a dedup hit (not new).
        let mut calls2: Vec<(u64, u64, bool)> = Vec::new();
        store
            .ingest_tree_files_with_progress(
                &[("f.bin".to_string(), fpath)],
                None,
                |done, total, is_new| calls2.push((done, total, is_new)),
            )
            .unwrap();
        assert!(calls2.iter().all(|(_, _, is_new)| !is_new));
        assert_eq!(manifest.total_size, data.len() as u64);
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p np2ptp-store pack_with_progress_reports_bytes_and_dedup_flag`
Expected: FAIL to compile — `no method named 'ingest_tree_files_with_progress' found for struct 'Store'`

- [ ] **Step 3: Write minimal implementation**

In `crates/np2ptp-store/src/lib.rs`, replace the existing `ingest_file_streaming_impl` with a version that takes and calls a progress callback:

```rust
    fn ingest_file_streaming_impl(
        &self,
        path: &Path,
        no_copy: bool,
        mut on_progress: impl FnMut(u64, u64, bool),
    ) -> Result<(Vec<ChunkRef>, u64), StoreError> {
        // Resolved once up front: the reference must still be valid from a
        // different working directory in a later `serve` process.
        let source = if no_copy { Some(fs::canonicalize(path)?) } else { None };
        let total = fs::metadata(path)?.len();
        let reader = BufReader::new(File::open(path)?);
        let mut refs = Vec::new();
        let mut offset = 0u64;
        for chunk in StreamCDC::new(reader, MIN_CHUNK, AVG_CHUNK, MAX_CHUNK) {
            let chunk = chunk.map_err(|e| io::Error::other(e.to_string()))?;
            let length = chunk.data.len() as u32;
            let (hash, is_new) = match &source {
                Some(src) => {
                    let hash = Hash::of(&chunk.data);
                    let is_new = !self.has(&hash);
                    self.add_reference(hash, src, offset, length)?;
                    (hash, is_new)
                }
                None => self.put(&chunk.data)?,
            };
            refs.push(ChunkRef { hash, offset, length });
            offset += length as u64;
            on_progress(offset, total, is_new);
        }
        Ok((refs, offset))
    }
```

Update the two existing public callers to pass a no-op callback (find `ingest_file_streaming` and `ingest_file_streaming_no_copy`):

```rust
    pub fn ingest_file_streaming(&self, path: &Path) -> Result<(Vec<ChunkRef>, u64), StoreError> {
        self.ingest_file_streaming_impl(path, false, |_, _, _| {})
    }

    pub fn ingest_file_streaming_no_copy(&self, path: &Path) -> Result<(Vec<ChunkRef>, u64), StoreError> {
        self.ingest_file_streaming_impl(path, true, |_, _, _| {})
    }
```

Replace `ingest_tree_files_impl` so it threads a global byte offset/total through to each file's `ingest_file_streaming_impl` call:

```rust
    fn ingest_tree_files_impl(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
        no_copy: bool,
        mut on_progress: impl FnMut(u64, u64, bool),
    ) -> Result<Manifest, StoreError> {
        let total: u64 = files
            .iter()
            .map(|(_, p)| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum();
        let mut chunks: Vec<ChunkRef> = Vec::new();
        let mut entries: Vec<FileEntry> = Vec::new();
        let mut global: u64 = 0;
        for (rel, disk) in files {
            let base = global;
            let (refs, size) = self.ingest_file_streaming_impl(disk, no_copy, |done, _file_total, is_new| {
                on_progress(base + done, total, is_new);
            })?;
            let chunk_start = chunks.len();
            for r in &refs {
                chunks.push(ChunkRef { hash: r.hash, offset: global + r.offset, length: r.length });
            }
            entries.push(FileEntry { path: rel.clone(), size, chunk_start, chunk_count: refs.len() });
            global += size;
        }
        let hashes: Vec<Hash> = chunks.iter().map(|c| c.hash).collect();
        Ok(Manifest { root: merkle_root(&hashes), total_size: global, chunks, files: entries, name })
    }
```

Update the two existing public callers (find `ingest_tree_files` and `ingest_tree_files_no_copy`) to pass a no-op callback:

```rust
    pub fn ingest_tree_files(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
    ) -> Result<Manifest, StoreError> {
        self.ingest_tree_files_impl(files, name, false, |_, _, _| {})
    }

    pub fn ingest_tree_files_no_copy(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
    ) -> Result<Manifest, StoreError> {
        self.ingest_tree_files_impl(files, name, true, |_, _, _| {})
    }
```

Add the two new public `_with_progress` wrappers right after those:

```rust
    /// Like [`Store::ingest_tree_files`], but calls `on_progress(bytes_done,
    /// bytes_total, chunk_was_new)` as each chunk is processed — `bytes_total`
    /// is the sum of every file's size, known upfront; `chunk_was_new` is
    /// false for a chunk that was already in the store (a dedup hit).
    pub fn ingest_tree_files_with_progress(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
        on_progress: impl FnMut(u64, u64, bool),
    ) -> Result<Manifest, StoreError> {
        self.ingest_tree_files_impl(files, name, false, on_progress)
    }

    /// Like [`Store::ingest_tree_files_no_copy`], with the same progress
    /// callback as [`Store::ingest_tree_files_with_progress`].
    pub fn ingest_tree_files_no_copy_with_progress(
        &self,
        files: &[(String, PathBuf)],
        name: Option<String>,
        on_progress: impl FnMut(u64, u64, bool),
    ) -> Result<Manifest, StoreError> {
        self.ingest_tree_files_impl(files, name, true, on_progress)
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p np2ptp-store pack_with_progress_reports_bytes_and_dedup_flag`
Expected: PASS

Also run the full store suite to confirm nothing else broke:
Run: `cargo test -p np2ptp-store`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-store/src/lib.rs
git commit -m "np2ptp-store: add progress-reporting pack variants"
```

---

### Task 4: `np2ptp_node::download_with_progress` (offline path, used by `get`)

**Files:**
- Modify: `crates/np2ptp-node/src/lib.rs`
- Test: `crates/np2ptp-node/tests/integration.rs`

**Interfaces:**
- Produces: `pub fn download_with_progress<S: ChunkSource>(manifest: &Manifest, source: &S, local: &Store, on_progress: impl FnMut(usize, usize)) -> Result<DownloadReport, NodeError>` — callback is `(chunks_done, chunks_total)`, called once per chunk (whether fetched or deduped) after it's accounted for.

- [ ] **Step 1: Write the failing test**

Add to `crates/np2ptp-node/tests/integration.rs` (check the file's existing test names first so this doesn't collide; add near the other `download`-related tests):

```rust
#[test]
fn download_with_progress_reports_every_chunk_once() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 77);
    let manifest = seed_store.ingest(&data, None).unwrap();

    let client_dir = TmpDir::new();
    let source = StoreSource::open(seed_dir.path()).unwrap();
    let local = Store::open(client_dir.path()).unwrap();

    let mut calls: Vec<(usize, usize)> = Vec::new();
    let report = download_with_progress(&manifest, &source, &local, |done, total| {
        calls.push((done, total));
    })
    .unwrap();

    let total = manifest.chunks.len();
    assert!(total > 1, "want a multi-chunk transfer");
    assert_eq!(calls.len(), total);
    assert_eq!(calls.last().unwrap(), &(total, total));
    assert_eq!(report.fetched, total);
    assert_eq!(report.deduped, 0);
}
```

Add `download_with_progress` to the existing `use np2ptp_node::{...}` import line at the top of the file.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p np2ptp-node --test integration download_with_progress_reports_every_chunk_once`
Expected: FAIL to compile — `unresolved import` / `cannot find function 'download_with_progress'`

- [ ] **Step 3: Write minimal implementation**

In `crates/np2ptp-node/src/lib.rs`, replace the existing `download` function with:

```rust
/// The **client**: fetch every chunk named by `manifest` from `source` into
/// `local`, verifying each chunk against the Merkle root before storing it.
///
/// Chunks already in `local` are skipped, so re-downloading content that shares
/// chunks with something you already have is nearly free. A chunk that fails
/// verification aborts the download with [`NodeError::BadChunk`] — a lying peer
/// is caught immediately, before its bytes can corrupt the output.
pub fn download<S: ChunkSource>(
    manifest: &Manifest,
    source: &S,
    local: &Store,
) -> Result<DownloadReport, NodeError> {
    download_with_progress(manifest, source, local, |_, _| {})
}

/// Like [`download`], but calls `on_progress(chunks_done, chunks_total)` once
/// per chunk (fetched or deduped) as it's accounted for.
pub fn download_with_progress<S: ChunkSource>(
    manifest: &Manifest,
    source: &S,
    local: &Store,
    mut on_progress: impl FnMut(usize, usize),
) -> Result<DownloadReport, NodeError> {
    // Validate the chunk list against the Merkle root once; then a cheap
    // per-chunk content-hash check is enough (and stays O(n) at scale).
    if !manifest.root_is_consistent() {
        return Err(NodeError::BadChunk { index: 0 });
    }
    let total = manifest.chunks.len();
    let mut fetched = 0;
    let mut deduped = 0;
    for (i, cref) in manifest.chunks.iter().enumerate() {
        if local.has(&cref.hash) {
            deduped += 1;
        } else {
            let bytes = source
                .fetch(&cref.hash)?
                .ok_or(NodeError::MissingChunk(cref.hash))?;
            if !manifest.chunk_hash_ok(i, &bytes) {
                return Err(NodeError::BadChunk { index: i });
            }
            local.put(&bytes)?;
            fetched += 1;
        }
        on_progress(i + 1, total);
    }
    Ok(DownloadReport { fetched, deduped })
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p np2ptp-node --test integration download_with_progress_reports_every_chunk_once`
Expected: PASS

Also run the full integration suite:
Run: `cargo test -p np2ptp-node --test integration`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-node/src/lib.rs crates/np2ptp-node/tests/integration.rs
git commit -m "np2ptp-node: add download_with_progress for the offline get path"
```

---

### Task 5: `Network::download_with_progress` (real network path, used by `fetch`)

**Files:**
- Modify: `crates/np2ptp-net/src/lib.rs`
- Test: `crates/np2ptp-net/tests/two_nodes.rs`

**Interfaces:**
- Produces: `Network::download_with_progress(&self, root: Hash, provider: PeerId, into: &Store, on_progress: impl FnMut(usize, usize)) -> Result<Manifest, NetError>` — callback is `(chunks_done, chunks_total)`; `chunks_done` starts at however many were already local (dedup) before any network fetch, then increments per chunk actually pulled over the wire.

- [ ] **Step 1: Write the failing test**

Add to `crates/np2ptp-net/tests/two_nodes.rs` (after the Task 2 test):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn download_with_progress_reaches_total() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(300_000, 9);
    let manifest = seed_store.ingest(&data, None).unwrap();
    let root = manifest.root;
    let total_chunks = manifest.chunks.len();
    assert!(total_chunks > 1, "want a multi-chunk transfer");

    let seed = Network::spawn(seed_store, Some([95u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([96u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    client.dial(seed_addr).await.unwrap();

    let mut last_done = std::sync::Arc::new(std::sync::Mutex::new(0usize));
    let mut ok = false;
    for _ in 0..100 {
        let done_cell = last_done.clone();
        let result = client
            .download_with_progress(root, seed_peer, &client_store, move |done, _total| {
                *done_cell.lock().unwrap() = done;
            })
            .await;
        if result.is_ok() {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(ok, "download should complete");
    assert_eq!(*last_done.lock().unwrap(), total_chunks);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p np2ptp-net --test two_nodes download_with_progress_reaches_total`
Expected: FAIL to compile — `no method named 'download_with_progress' found for struct 'Network'`

- [ ] **Step 3: Write minimal implementation**

In `crates/np2ptp-net/src/lib.rs`, replace the existing `download` method with:

```rust
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

        while let Some(result) = stream.next().await {
            let (i, bytes) = result?;
            if !manifest.chunk_hash_ok(i, &bytes) {
                return Err(NetError::BadChunk);
            }
            into.put(&bytes)?;
            done += 1;
            on_progress(done, total);
        }
        Ok(manifest)
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p np2ptp-net --test two_nodes download_with_progress_reaches_total`
Expected: PASS

Also run the full net suite:
Run: `cargo test -p np2ptp-net`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-net/src/lib.rs crates/np2ptp-net/tests/two_nodes.rs
git commit -m "np2ptp-net: add download_with_progress for the fetch path"
```

---

### Task 6: `Network::download_fec_with_progress`

**Files:**
- Modify: `crates/np2ptp-net/src/lib.rs`
- Test: `crates/np2ptp-net/tests/two_nodes.rs`

**Interfaces:**
- Consumes: nothing new from earlier tasks.
- Produces: `Network::download_fec_with_progress(&self, root: Hash, provider: PeerId, into: &Store, on_progress: impl FnMut(usize, usize)) -> Result<Manifest, NetError>` — callback is `(symbols_collected, symbols_needed)`, called after each batch of RaptorQ symbols arrives.

- [ ] **Step 1: Write the failing test**

Add to `crates/np2ptp-net/tests/two_nodes.rs` (after the Task 5 test):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fec_download_with_progress_reaches_need() {
    let seed_dir = TmpDir::new();
    let seed_store = Store::open(seed_dir.path()).unwrap();
    let data = sample(250_000, 11);
    let manifest = seed_store.ingest(&data, Some("vid.bin".into())).unwrap();
    let root = manifest.root;

    let seed = Network::spawn(seed_store, Some([97u8; 32])).unwrap();
    seed.listen("/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap())
        .await
        .unwrap();
    let seed_addr = first_listen_addr(&seed).await;
    let seed_peer = seed.local_peer_id();
    seed.provide(&manifest).await.unwrap();

    let client_dir = TmpDir::new();
    let client = Network::spawn(Store::open(client_dir.path()).unwrap(), Some([98u8; 32])).unwrap();
    let client_store = Store::open(client_dir.path()).unwrap();
    client.dial(seed_addr).await.unwrap();

    let last_call = std::sync::Arc::new(std::sync::Mutex::new((0usize, 0usize)));
    let mut downloaded = None;
    for _ in 0..100 {
        let cell = last_call.clone();
        let result = client
            .download_fec_with_progress(root, seed_peer, &client_store, move |done, need| {
                *cell.lock().unwrap() = (done, need);
            })
            .await;
        if let Ok(m) = result {
            downloaded = Some(m);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let got = downloaded.expect("FEC download should complete");
    assert_eq!(client_store.export(&got).unwrap(), data);
    let (done, need) = *last_call.lock().unwrap();
    assert!(done >= need && need > 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p np2ptp-net --test two_nodes fec_download_with_progress_reaches_need`
Expected: FAIL to compile — `no method named 'download_fec_with_progress' found for struct 'Network'`

- [ ] **Step 3: Write minimal implementation**

In `crates/np2ptp-net/src/lib.rs`, replace the existing `download_fec` method with:

```rust
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
        let decoded = loop {
            let batch = self.fetch_symbols(provider, root, start, FEC_BATCH).await?;
            let exhausted = batch.is_empty();
            start += batch.len() as u32;
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
        Ok(manifest)
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p np2ptp-net --test two_nodes fec_download_with_progress_reaches_need`
Expected: PASS

Also run the full net suite:
Run: `cargo test -p np2ptp-net`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-net/src/lib.rs crates/np2ptp-net/tests/two_nodes.rs
git commit -m "np2ptp-net: add download_fec_with_progress"
```

---

### Task 7: CLI — `--json` flag parsing and generic error event in `run()`

**Files:**
- Modify: `crates/np2ptp-node/src/main.rs`

**Interfaces:**
- Consumes: nothing new from earlier tasks (this is pure CLI plumbing).
- Produces: a generic behavior in `run()` — if `--json` appears anywhere in argv and a `cmd_*` function returns `Err`, one `{"event":"error","op":<subcommand>,"message":<string>}` line is printed to stdout before the error propagates (existing stderr print in `main()` is untouched). This has no unit test of its own; it's exercised by Task 12's integration test failure-path coverage is optional — the important behavior (success-path JSON) is what Task 12 checks. This task is verified manually in Step 2 below since it changes `run()`'s control flow, which has no existing test harness of its own.

- [ ] **Step 1: Write the minimal implementation**

In `crates/np2ptp-node/src/main.rs`, replace `run()`:

```rust
fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let json = args.iter().any(|a| a == "--json");
    let op = args.first().cloned().unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("pack") => cmd_pack(&args[1..]),
        Some("info") => cmd_info(&args[1..]),
        Some("get") => cmd_get(&args[1..]),
        Some("serve") => cmd_serve(&args[1..]),
        Some("fetch") => cmd_fetch(&args[1..]),
        Some("relay") => cmd_relay(&args[1..]),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            print_usage();
            Err("unknown command".into())
        }
    };
    if let Err(e) = &result {
        if json {
            println!(
                "{}",
                serde_json::json!({"event": "error", "op": op, "message": e.to_string()})
            );
        }
    }
    result
}
```

- [ ] **Step 2: Manually verify the build compiles and existing behavior is unchanged**

Run: `cargo build -p np2ptp-node`
Expected: builds with no errors.

Run: `cargo run -q -p np2ptp-node --bin np2ptp -- badcommand`
Expected: prints `unknown command: badcommand` followed by usage to stderr, same as before this change (no `--json` was passed, so no JSON line).

Run: `cargo run -q -p np2ptp-node --bin np2ptp -- badcommand --json`
Expected: same stderr output as above, PLUS one line on stdout: `{"event":"error","op":"badcommand","message":"unknown command"}`.

- [ ] **Step 3: Commit**

```bash
git add crates/np2ptp-node/src/main.rs
git commit -m "np2ptp-node: emit a JSON error event in run() when --json is set"
```

---

### Task 8: CLI — `--json` for `pack`

**Files:**
- Modify: `crates/np2ptp-node/src/main.rs` (`cmd_pack`)

**Interfaces:**
- Consumes: `Task 3`'s `Store::ingest_tree_files_with_progress` / `ingest_tree_files_no_copy_with_progress`.

- [ ] **Step 1: Write the minimal implementation**

Replace `cmd_pack` in `crates/np2ptp-node/src/main.rs`:

```rust
fn cmd_pack(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, flags) = parse(args, &["--out", "--store", "--name"]);
    let input = *pos.first().ok_or("pack: missing <input> file or directory")?;

    let store_dir = flags.get("store").map(String::as_str).unwrap_or(DEFAULT_STORE);
    let store = Store::open(store_dir)?;
    // Default is to copy chunks into the store (safe if `input` moves/changes
    // afterward). --no-copy references `input` in place instead, so seeding it
    // doesn't cost a second copy of the file on disk — but `input` must stay
    // where it is, unchanged, for as long as you serve it.
    let no_copy = flags.contains_key("no-copy");
    let json = flags.contains_key("json");

    let name = flags.get("name").cloned().or_else(|| {
        Path::new(input).file_name().map(|s| s.to_string_lossy().into_owned())
    });

    let mut chunks_new = 0usize;
    let mut last_emit = std::time::Instant::now();
    let mut on_progress = |done: u64, total: u64, is_new: bool| {
        if is_new {
            chunks_new += 1;
        }
        if json {
            let now = std::time::Instant::now();
            if done == total || now.duration_since(last_emit) >= Duration::from_millis(100) {
                last_emit = now;
                println!(
                    "{}",
                    serde_json::json!({"event":"progress","op":"pack","bytes_done":done,"bytes_total":total})
                );
            }
        }
    };

    // A directory is packed as a tree of files; a single file as one blob. Both
    // stream from disk so packing huge content doesn't load it into memory.
    let manifest = if fs::metadata(input)?.is_dir() {
        let files = read_dir_paths(Path::new(input))?;
        if files.is_empty() {
            return Err(format!("pack: directory {input} contains no files").into());
        }
        if no_copy {
            store.ingest_tree_files_no_copy_with_progress(&files, name, &mut on_progress)?
        } else {
            store.ingest_tree_files_with_progress(&files, name, &mut on_progress)?
        }
    } else {
        let file_name = name
            .clone()
            .or_else(|| Path::new(input).file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "data".to_string());
        let entry = [(file_name, Path::new(input).to_path_buf())];
        if no_copy {
            store.ingest_tree_files_no_copy_with_progress(&entry, name, &mut on_progress)?
        } else {
            store.ingest_tree_files_with_progress(&entry, name, &mut on_progress)?
        }
    };

    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| format!("{input}.nptp"));
    fs::write(&out, manifest.to_nptp()?)?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "event":"result","op":"pack",
                "root": manifest.uri(),
                "chunks_total": manifest.chunks.len(),
                "chunks_new": chunks_new,
                "bytes_total": manifest.total_size,
            })
        );
    } else {
        println!("packed {input} ({} bytes) -> {out}", manifest.total_size);
        println!(
            "  files: {}   chunks: {}   store: {store_dir}",
            manifest.files.len(),
            manifest.chunks.len()
        );
        if no_copy {
            println!("  (--no-copy: chunks reference {input} in place — keep it there and unchanged)");
        }
        println!("  link:  {}", manifest.uri());
    }
    Ok(())
}
```

- [ ] **Step 2: Manually verify**

Run:
```sh
cargo build -p np2ptp-node
$env:TEMP_TEST = "$env:TEMP\np2ptp-plan-test"
Remove-Item -Recurse -Force $env:TEMP_TEST -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $env:TEMP_TEST | Out-Null
$bytes = New-Object byte[] (2MB); (New-Object Random).NextBytes($bytes)
[IO.File]::WriteAllBytes("$env:TEMP_TEST\f.bin", $bytes)
cargo run -q -p np2ptp-node --bin np2ptp -- pack "$env:TEMP_TEST\f.bin" --store "$env:TEMP_TEST\store" --out "$env:TEMP_TEST\f.nptp" --json
```
Expected: every stdout line is valid JSON (eyeball it — the last line has `"event":"result"` and a `root` starting with `np2ptp:`); no human text lines mixed in.

Run the same command WITHOUT `--json` and confirm the original human-readable text output is unchanged.

- [ ] **Step 3: Commit**

```bash
git add crates/np2ptp-node/src/main.rs
git commit -m "np2ptp-node: wire --json progress/result events into pack"
```

---

### Task 9: CLI — `--json` for `get`

**Files:**
- Modify: `crates/np2ptp-node/src/main.rs` (`cmd_get`)

**Interfaces:**
- Consumes: `Task 4`'s `download_with_progress`.

- [ ] **Step 1: Write the minimal implementation**

Replace `cmd_get` in `crates/np2ptp-node/src/main.rs`:

```rust
fn cmd_get(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, flags) = parse(args, &["--source", "--store", "--out"]);
    let file = *pos.first().ok_or("get: missing <file.nptp>")?;
    let manifest = Manifest::from_nptp(&fs::read(file)?)?;

    let source_dir = flags
        .get("source")
        .ok_or("get: --source <store-dir> is required (a seed's store)")?;
    let source = StoreSource::open(source_dir)?;

    let store_dir = flags.get("store").map(String::as_str).unwrap_or(DEFAULT_STORE);
    let local = Store::open(store_dir)?;
    let json = flags.contains_key("json");

    let mut last_emit = std::time::Instant::now();
    let mut on_progress = |done: usize, total: usize| {
        if json {
            let now = std::time::Instant::now();
            if done == total || now.duration_since(last_emit) >= Duration::from_millis(100) {
                last_emit = now;
                println!(
                    "{}",
                    serde_json::json!({"event":"progress","op":"get","chunks_done":done,"chunks_total":total})
                );
            }
        }
    };

    let report = download_with_progress(&manifest, &source, &local, &mut on_progress)?;
    let dest = write_output(&local, &manifest, flags.get("out").cloned())?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "event":"result","op":"get",
                "path": dest,
                "chunks_fetched": report.fetched,
                "chunks_deduped": report.deduped,
            })
        );
    } else {
        println!("downloaded {} ({} bytes) -> {dest}", manifest.uri(), manifest.total_size);
        println!(
            "  fetched {} chunks, {} already local (deduped)",
            report.fetched, report.deduped
        );
    }
    Ok(())
}
```

Update the `use np2ptp_node::{download, read_dir_paths, StoreSource};` import at the top of the file to also bring in `download_with_progress`:

```rust
use np2ptp_node::{download_with_progress, read_dir_paths, StoreSource};
```

(Drop the now-unused `download` import — `cmd_get` calls `download_with_progress` directly; nothing else in `main.rs` calls plain `download`.)

- [ ] **Step 2: Manually verify**

Run:
```sh
cargo build -p np2ptp-node
cargo run -q -p np2ptp-node --bin np2ptp -- get "$env:TEMP_TEST\f.nptp" --source "$env:TEMP_TEST\store" --store "$env:TEMP_TEST\client-store" --out "$env:TEMP_TEST\restored" --json
```
Expected: valid JSON lines, final one `"event":"result","op":"get"` with `chunks_fetched`/`chunks_deduped`. Confirm `$env:TEMP_TEST\restored` contains the reconstructed file, byte-identical to `f.bin` (same check as done earlier in this session).

- [ ] **Step 3: Commit**

```bash
git add crates/np2ptp-node/src/main.rs
git commit -m "np2ptp-node: wire --json progress/result events into get"
```

---

### Task 10: CLI — `--json` for `fetch`

**Files:**
- Modify: `crates/np2ptp-node/src/main.rs` (`cmd_fetch`)

**Interfaces:**
- Consumes: `Task 5`'s `Network::download_with_progress`, `Task 6`'s `Network::download_fec_with_progress`.

- [ ] **Step 1: Write the minimal implementation**

In `cmd_fetch`, add `let json = flags.contains_key("json");` next to the existing `let use_fec = flags.contains_key("fec");` line.

Inside the `rt.block_on(async move { ... })` block, right before the `'outer: for (peer, addrs) in &candidates {` loop, add the progress closure:

```rust
        let mut last_emit = std::time::Instant::now();
        let mut on_progress = |done: usize, total: usize| {
            if json {
                let now = std::time::Instant::now();
                if done == total || now.duration_since(last_emit) >= Duration::from_millis(100) {
                    last_emit = now;
                    println!(
                        "{}",
                        serde_json::json!({"event":"progress","op":"fetch","chunks_done":done,"chunks_total":total})
                    );
                }
            }
        };
```

Guard the two "discovering peers" println!s (inside the `None => { ... }` match arm building `candidates`) with `if !json`:

```rust
            None => {
                if !json {
                    println!("discovering peers for {} via {tracker_url} ...", root.to_hex());
                }
                let found = tracker::get_peers(&tracker_url, root).await?;
                if found.is_empty() {
                    return Err("no peers found on the tracker for this content (and no --peer given)".into());
                }
                if !json {
                    println!("  found {} peer(s)", found.len());
                }
                found
            }
```

Change the two `net.download(...)`/`net.download_fec(...)` calls to their `_with_progress` siblings:

```rust
                let attempt = if use_fec {
                    net.download_fec_with_progress(root, *peer, &into, &mut on_progress).await
                } else {
                    net.download_with_progress(root, *peer, &into, &mut on_progress).await
                };
```

Replace the tail (from `let manifest = manifest.ok_or_else(...)` to the end of the `block_on` body):

```rust
        let manifest =
            manifest.ok_or_else(|| format!("download failed: {}", last_err.unwrap_or_default()))?;

        let dest = write_output(&into, &manifest, out_flag)?;
        if json {
            println!(
                "{}",
                serde_json::json!({
                    "event":"result","op":"fetch",
                    "root": manifest.uri(),
                    "path": dest,
                    "bytes_total": manifest.total_size,
                })
            );
        } else {
            println!("fetched {} ({} bytes) -> {dest}", manifest.uri(), manifest.total_size);
        }
        Ok::<(), Box<dyn Error>>(())
    })
}
```

- [ ] **Step 2: Manually verify**

This needs a real network round trip; reuse the `serve`/`fetch` pair against localhost the way it was done earlier in this session (see the `end_to_end_download_over_quic`-style flow, or simplest: `serve` in one terminal, `fetch --peer <addr> --json` in another) and confirm stdout is clean NDJSON with a final `result` event. If a live two-process check isn't practical in this environment, it is acceptable to rely on Task 5/6's automated tests for the underlying progress mechanics and do a compile-only check here:

Run: `cargo build -p np2ptp-node`
Expected: builds with no errors.

- [ ] **Step 3: Commit**

```bash
git add crates/np2ptp-node/src/main.rs
git commit -m "np2ptp-node: wire --json progress/result events into fetch"
```

---

### Task 11: CLI — `--json` and periodic status for `serve`

**Files:**
- Modify: `crates/np2ptp-node/src/main.rs` (`cmd_serve`)

**Interfaces:**
- Consumes: `Task 2`'s `Network::connected_peers()`, `Network::ledger_totals()`.

- [ ] **Step 1: Write the minimal implementation**

In `cmd_serve`, add `let json = flags.contains_key("json");` next to the other flag reads near the top (alongside `let no_tracker = flags.contains_key("no-tracker");`).

Guard the existing announce/status println!s that currently run unconditionally with `if !json` where they're purely human text (the `println!("serving {...}")`, the `direct fetch:` block, the relay-related prints, etc. — leave all of those exactly as they are; they only need guarding where noted below, to avoid mixing text into an NDJSON stream when a launcher has requested `--json`). Specifically, find:

```rust
        if no_tracker {
            println!("\nProviding on the DHT. Press Ctrl-C to stop.");
            tokio::signal::ctrl_c().await?;
        } else {
            println!("\nProviding on the DHT + announcing to {tracker_url}. Press Ctrl-C to stop.");
            // Re-announce periodically (TTL ~30 min) — frequent enough to pick up
            // a UPnP-mapped public address once the router responds.
            let mut interval = tokio::time::interval(Duration::from_secs(120));
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    _ = interval.tick() => {
                        // Announce local listen addresses AND any public (UPnP)
                        // external address so peers on other networks can reach us.
                        let mut addrs = net.listeners().await.unwrap_or_default();
                        for ext in net.external_addresses().await.unwrap_or_default() {
                            if !addrs.contains(&ext) {
                                addrs.push(ext);
                            }
                        }
                        if let Err(e) = tracker::announce(&tracker_url, manifest.root, peer, &addrs).await {
                            eprintln!("  (tracker announce failed: {e})");
                        }
                    }
                }
            }
        }
        println!("\nstopped.");
        Ok::<(), Box<dyn Error>>(())
    })
}
```

and replace that whole block with a single unified loop that always ticks a status interval (used only when `json`) alongside the tracker-announce interval (used only when `!no_tracker`), so status reporting doesn't depend on whether a tracker is in use:

```rust
        if !json {
            if no_tracker {
                println!("\nProviding on the DHT. Press Ctrl-C to stop.");
            } else {
                println!("\nProviding on the DHT + announcing to {tracker_url}. Press Ctrl-C to stop.");
            }
        }
        let mut announce_interval = tokio::time::interval(Duration::from_secs(120));
        let mut status_interval = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                _ = announce_interval.tick(), if !no_tracker => {
                    // Announce local listen addresses AND any public (UPnP)
                    // external address so peers on other networks can reach us.
                    let mut addrs = net.listeners().await.unwrap_or_default();
                    for ext in net.external_addresses().await.unwrap_or_default() {
                        if !addrs.contains(&ext) {
                            addrs.push(ext);
                        }
                    }
                    if let Err(e) = tracker::announce(&tracker_url, manifest.root, peer, &addrs).await {
                        if !json {
                            eprintln!("  (tracker announce failed: {e})");
                        }
                    }
                }
                _ = status_interval.tick(), if json => {
                    let peers = net.connected_peers().await.unwrap_or_default();
                    let totals = net.ledger_totals().await.unwrap_or_default();
                    let tracker = if no_tracker {
                        serde_json::Value::Null
                    } else {
                        serde_json::Value::String(tracker_url.clone())
                    };
                    println!(
                        "{}",
                        serde_json::json!({
                            "event":"status","op":"serve",
                            "peers": peers.len(),
                            "tracker": tracker,
                            "bytes_served": totals.we_served,
                            "bytes_received": totals.served_to_us,
                        })
                    );
                }
            }
        }
        if !json {
            println!("\nstopped.");
        }
        Ok::<(), Box<dyn Error>>(())
    })
}
```

`Network::ledger_totals()` returns `Result<np2ptp_net::Counters, NetError>`; `.unwrap_or_default()` requires `Counters: Default`, which it already derives (see Task 1/2) — no further change needed for that.

- [ ] **Step 2: Manually verify**

Run (in one terminal):
```sh
cargo build -p np2ptp-node
cargo run -q -p np2ptp-node --bin np2ptp -- serve "$env:TEMP_TEST\f.nptp" --store "$env:TEMP_TEST\store" --no-relay --json
```
Expected: within ~2 seconds, a line like `{"event":"status","op":"serve","peers":0,"tracker":"https://nptp.bogotec.uk","bytes_served":0,"bytes_received":0}` appears, then repeats every ~2s. No other text mixed in. Ctrl-C stops it cleanly (no `"stopped."` line, since json mode suppresses it).

Run the same command without `--json` and confirm the original text output (the `serving ...`, `direct fetch:` lines, final `stopped.`) is unchanged.

- [ ] **Step 3: Commit**

```bash
git add crates/np2ptp-node/src/main.rs
git commit -m "np2ptp-node: wire --json periodic status events into serve"
```

---

### Task 12: CLI integration test — `pack --json` and `get --json` emit valid NDJSON

**Files:**
- Modify: `crates/np2ptp-node/tests/integration.rs`

**Interfaces:**
- Consumes: the real built `np2ptp` binary (via `env!("CARGO_BIN_EXE_np2ptp")`, provided automatically by Cargo for integration tests in the same package that defines the `[[bin]]`).

- [ ] **Step 1: Write the failing test**

Add to `crates/np2ptp-node/tests/integration.rs`:

```rust
#[test]
fn pack_json_emits_valid_ndjson_and_a_final_result_event() {
    let dir = TmpDir::new();
    let input = dir.path().join("f.bin");
    std::fs::write(&input, sample(300_000, 50)).unwrap();
    let store_dir = dir.path().join("store");
    let out = dir.path().join("f.nptp");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("pack")
        .arg(&input)
        .arg("--store")
        .arg(&store_dir)
        .arg("--out")
        .arg(&out)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty(), "expected at least one NDJSON line");

    let mut saw_result = false;
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line not valid JSON: {line:?}: {e}"));
        assert_eq!(v["op"], "pack");
        if v["event"] == "result" {
            saw_result = true;
            assert!(v["root"].as_str().unwrap().starts_with("np2ptp:"));
            assert_eq!(v["bytes_total"], 300_000);
        }
    }
    assert!(saw_result, "expected a final result event, got: {stdout}");
}

#[test]
fn get_json_emits_valid_ndjson_and_a_final_result_event() {
    let dir = TmpDir::new();
    let input = dir.path().join("f.bin");
    std::fs::write(&input, sample(300_000, 51)).unwrap();
    let store_dir = dir.path().join("store");
    let out = dir.path().join("f.nptp");

    let pack_output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("pack")
        .arg(&input)
        .arg("--store")
        .arg(&store_dir)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert!(pack_output.status.success());

    let client_store = dir.path().join("client-store");
    let restored = dir.path().join("restored");
    let get_output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("get")
        .arg(&out)
        .arg("--source")
        .arg(&store_dir)
        .arg("--store")
        .arg(&client_store)
        .arg("--out")
        .arg(&restored)
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        get_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&get_output.stderr)
    );

    let stdout = String::from_utf8(get_output.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.is_empty()).collect();
    assert!(!lines.is_empty());
    let mut saw_result = false;
    for line in &lines {
        let v: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("line not valid JSON: {line:?}: {e}"));
        assert_eq!(v["op"], "get");
        if v["event"] == "result" {
            saw_result = true;
            assert_eq!(v["chunks_deduped"], 0);
        }
    }
    assert!(saw_result, "expected a final result event, got: {stdout}");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p np2ptp-node --test integration pack_json_emits_valid_ndjson_and_a_final_result_event`
Expected: FAIL — either a compile error (if `--json` isn't wired yet, this would actually already pass if Tasks 7–9 are done; run this test BEFORE those tasks in a real TDD flow, or, since this plan sequences it last, treat this run as the final confirmation and expect PASS here). If run out of order and it fails, the failure will show either non-JSON lines in stdout or a missing `result` event — fix per Step 3.

- [ ] **Step 3: Fix forward if needed**

If this fails after Tasks 7–11 are complete, the most likely cause is a stray non-JSON `println!` still active under `--json` in `cmd_pack`/`cmd_get` — search `crates/np2ptp-node/src/main.rs` for `println!` calls not guarded by `if json`/`if !json` in those two functions and fix.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p np2ptp-node --test integration pack_json_emits_valid_ndjson_and_a_final_result_event get_json_emits_valid_ndjson_and_a_final_result_event`
Expected: both PASS

Then run the entire workspace test suite and clippy to confirm nothing regressed:

Run: `cargo test --workspace`
Expected: all pass

Run: `cargo clippy --workspace --all-targets`
Expected: 0 warnings

- [ ] **Step 5: Commit**

```bash
git add crates/np2ptp-node/tests/integration.rs
git commit -m "np2ptp-node: add CLI integration tests for pack/get --json output"
```

---

## Final check

After Task 12, push `dev` to origin:

```bash
git push origin dev
```

Do **not** merge to `main` or tag a release as part of this plan — per the user's branching preference, that only happens when they confirm the feature is ready to ship.
