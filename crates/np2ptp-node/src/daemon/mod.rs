//! Daemon runtime: a persistent NDJSON stdio node (Task 7).
//!
//! One JSON request per stdin line, one (or more) JSON events per line back on
//! stdout — see [`proto`] for the wire shapes. Every request runs on its own
//! `tokio` task so responses can interleave by `id`; every task writes through
//! the single [`mpsc`] channel drained by the writer thread below, so stdout
//! lines are never partially interleaved.
//!
//! Extractable cores this reuses (**not** by calling into `main.rs` — that's a
//! separate `[[bin]]` crate target and its `cmd_*` fns are private, so this
//! module talks to the same lower-level APIs `main.rs` does instead):
//!  - `fetch`     -> tracker peer discovery (reimplemented below — see the
//!    `tracker_*` note), unioned with the peers this node is already connected
//!    to (so a `dial`ed peer is fetchable with no tracker at all, and a dead
//!    tracker is a `warn`, not a failed fetch) + `Network::download_multi_with_progress`, then
//!    `Store::export_to_with_progress` / `export_tree_to_dir_with_progress`
//!    (mirrors `cmd_fetch`/`cmd_get`'s `write_output_with_progress`).
//!  - `convert`   -> torrent+data form: `np2ptp_bridge::resolve_or_convert_local`.
//!    path form: `Store::ingest_tree_files_no_copy_with_progress` (mirrors
//!    `cmd_pack`'s `--no-copy` path, per the task brief).
//!  - `torrent`   -> `np2ptp_bridge::resolve_or_convert_remote`, `librqbit`-gated;
//!    a clear error event when the feature is off (mirrors `cmd_torrent`'s
//!    `fetch_remote_torrent` fallback).
//!  - `provide`   -> `Network::provide` + `register_manifest` + a per-root
//!    tracker announce loop (same 120s cadence as `cmd_serve`'s), tracked in a
//!    `HashMap<Hash, JoinHandle<()>>`. Takes a `.nptp` path **or** an
//!    `np2ptp:<hex>` root already in `<store>/manifests/` (what `convert`
//!    hands back), so convert -> provide is a closed loop inside the daemon.
//!  - `unprovide` -> abort that task + `Network::unprovide` + drop the
//!    `<store>/manifests/<root>.nptp` registry entry (else `serve --all`
//!    resurrects it).
//!  - `status`    -> `connected_peers` + the provided-roots map + `ledger_totals`
//!    + this node's `peer_id`/`addrs` (same pair the `ready` event carries).
//!  - `dial`      -> not a `proto::Op` variant yet (adding one means touching
//!    `proto.rs`, a third file outside this task's budget) — handled here via a
//!    raw `serde_json::Value` fallback keyed on `"cmd":"dial"`. Folding it into
//!    `Op` properly is follow-up work for whoever touches `proto.rs` next.
//!
//! One deliberate deviation from the brief, to stay inside the 2-file budget
//! (`daemon/mod.rs` + `main.rs` — no `Cargo.toml`):
//!  - stdin/stdout use `std::io` on a plain thread + `blocking_recv`, not
//!    `tokio::io::{stdin, stdout, BufReader}` — the latter needs the `io-util`/
//!    `io-std` tokio features, which nothing in this workspace enables yet.
//!    Behavior is the same: one blocking reader thread feeds lines into a
//!    `tokio::sync::mpsc` channel the async loop drains.
//!
//! The daemon's libp2p identity is persisted per `--store`, same as
//! `cmd_serve`/`cmd_torrent`'s `identity.key`: the caller (`main.rs`) loads or
//! creates the seed and hands it in via `DaemonConfig::identity_seed`, so the
//! reputation ledger and choke mechanism — both keyed off peer id — accumulate
//! across daemon restarts instead of resetting every run.
//!
//! Tracker HTTP calls (`tracker_announce`/`tracker_get_peers`) are a second,
//! smaller reimplementation for the same reason as the cores above: `mod
//! tracker;` in `main.rs` only compiles that file into the `np2ptp` *binary*
//! crate, not the `np2ptp_node` *library* crate this module lives in.

pub mod proto;

use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use np2ptp_core::{Hash, Manifest};
use np2ptp_net::{Multiaddr, Network, PeerId};
use np2ptp_store::Store;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, Mutex as AsyncMutex, Notify};
use tokio::task::JoinHandle;

use proto::{event_error, event_progress, event_ready, event_result, parse_request, Op, Request};

/// Ephemeral local listen address — same default `cmd_torrent` uses.
const DEFAULT_LISTEN: &str = "/ip4/0.0.0.0/udp/0/quic-v1";
/// Cadence for a `provide`d root's tracker announce loop — matches `cmd_serve`.
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(120);
/// Floor between two `warn`s from one root's announce loop: a tracker that is
/// down stays down for a while, and one line every two minutes per root is
/// noise the embedder has to parse.
const ANNOUNCE_WARN_INTERVAL: Duration = Duration::from_secs(600);
/// Whole-request budget for every tracker HTTP call. Without it a tracker that
/// accepts the connection and never answers hangs the op forever — and an op
/// that never finishes never emits its terminal event.
const TRACKER_TIMEOUT: Duration = Duration::from_secs(30);
/// Hard cap on a tracker response body, so a hostile/broken tracker can't make
/// us buffer unbounded.
const TRACKER_MAX_BODY: usize = 4 * 1024 * 1024;
/// Minimum gap between two `progress` events of one op. The out channel is
/// unbounded, so an un-throttled per-chunk callback lets a slow reader grow the
/// queue without bound. Terminal events (`result`/`error`) are never throttled.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
/// Longest request line accepted on stdin. A stream with no newline in it would
/// otherwise buffer until the process runs out of memory.
const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// How long shutdown waits for the writer to drain before exiting anyway —
/// in-flight ops hold their own `Ctx` clone, and a big `fetch` must not turn
/// `shutdown` into a several-minute wait.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);
/// How long [`listen_addrs`] waits for the swarm to report a bound address
/// before giving up (polls × interval — 2s total).
const LISTEN_ADDR_POLLS: usize = 40;
const LISTEN_ADDR_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Configuration for [`run_daemon`]. `relay`/`tracker` are already resolved by
/// the caller (env override, `--relay`/`--tracker` flags, or the built-in
/// defaults) — this module just uses them as given.
pub struct DaemonConfig {
    pub store_dir: String,
    pub relay: Option<String>,
    pub tracker: String,
    pub auto_update: bool,
    /// Persisted per `--store` by the caller (mirrors `cmd_serve`/`cmd_torrent`'s
    /// `identity.key`), so the daemon keeps the same peer id across restarts.
    pub identity_seed: [u8; 32],
}

/// Per-task handle to everything a request might need. Cheap to `Clone`
/// (`Network`/`Arc`/`String`/channel handles) — a fresh clone goes into every
/// spawned per-request task.
#[derive(Clone)]
struct Ctx {
    net: Network,
    store: Arc<Store>,
    store_dir: String,
    tracker: String,
    /// One shared HTTP client for every tracker call — built with
    /// [`TRACKER_TIMEOUT`] so no tracker request can hang an op forever.
    http: reqwest::Client,
    tx: mpsc::UnboundedSender<String>,
    announces: Arc<AsyncMutex<HashMap<Hash, JoinHandle<()>>>>,
    shutdown: Arc<Notify>,
}

/// Parses one NDJSON line and, on failure, sends an `error` event straight to
/// `tx`. Pure otherwise: no `Network`, no I/O beyond the channel send, so it's
/// unit-testable without any networking spun up.
fn handle_line(line: &str, tx: &mpsc::UnboundedSender<String>) -> Option<Request> {
    match parse_request(line) {
        Ok(req) => Some(req),
        Err(e) => {
            let _ = tx.send(event_error(raw_id(line), &e));
            None
        }
    }
}

/// The `id` of a line that failed request validation. A line can be valid JSON
/// carrying a perfectly good id and still be a bad *request* (`{"id":7,
/// "cmd":"fetch"}` with no `out`) — reporting id `0` there would leave the
/// embedder's pending id 7 waiting forever for a terminal event. `0` is the
/// fallback for a line that isn't JSON at all, where there is no id to trust.
fn raw_id(line: &str) -> u64 {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| v.get("id").and_then(|x| x.as_u64()))
        .unwrap_or(0)
}

fn event_warn(message: &str) -> String {
    json!({"event": "warn", "message": message}).to_string()
}

/// A `progress` sink that drops updates arriving less than
/// [`PROGRESS_INTERVAL`] apart. The completion update (`done >= total`) always
/// goes out, so a fast op still reports at least once.
fn progress_sender(
    tx: mpsc::UnboundedSender<String>,
    id: u64,
    op: &'static str,
) -> impl FnMut(u64, u64) {
    let mut last: Option<std::time::Instant> = None;
    move |done, total| {
        let now = std::time::Instant::now();
        let due = match last {
            None => true,
            Some(t) => now.duration_since(t) >= PROGRESS_INTERVAL,
        };
        if done >= total || due {
            last = Some(now);
            let _ = tx.send(event_progress(id, op, done, total));
        }
    }
}

/// `/p2p/<peer>`-suffixed string forms of `addrs` (idempotent: an address that
/// already carries the suffix is left alone). That suffix is what makes an
/// address directly dialable — it names who to expect at the other end.
fn addr_strs_with_peer(addrs: &[Multiaddr], peer: PeerId) -> Vec<String> {
    let suffix = format!("/p2p/{peer}");
    addrs
        .iter()
        .map(|a| {
            let s = a.to_string();
            if s.ends_with(&suffix) {
                s
            } else {
                format!("{s}{suffix}")
            }
        })
        .collect()
}

/// This node's dialable addresses, `/p2p/`-suffixed.
///
/// Polled rather than read once: `Network::listen` returns as soon as the
/// listener is *registered*, but the bound addresses only reach the swarm's
/// listener set a few ms later, when it processes `NewListenAddr` — reading
/// immediately would report an empty list on a node that is in fact listening.
/// Bounded by [`LISTEN_ADDR_POLLS`], so a box with no usable interface gets an
/// empty `addrs` array instead of a daemon that never says `ready`.
async fn listen_addrs(net: &Network) -> Vec<String> {
    for _ in 0..LISTEN_ADDR_POLLS {
        let addrs = net.listeners().await.unwrap_or_default();
        if !addrs.is_empty() {
            return addr_strs_with_peer(&addrs, net.local_peer_id());
        }
        tokio::time::sleep(LISTEN_ADDR_POLL_INTERVAL).await;
    }
    Vec::new()
}

/// [`proto::event_ready`] plus this node's `peer_id` and listen `addrs`.
///
/// An embedder has to be able to tell a second node where this one is
/// without a tracker in the loop, and `ready` is the only event it is
/// guaranteed to see. Built by merging into the `proto` line rather than
/// re-spelling it here, so the base shape stays owned by `proto`; falls back to
/// the bare line if that (self-produced) JSON somehow doesn't re-parse.
fn ready_line(version: &str, peer: PeerId, addrs: &[String]) -> String {
    let base = event_ready(version);
    match serde_json::from_str::<serde_json::Value>(&base) {
        Ok(serde_json::Value::Object(mut obj)) => {
            obj.insert("peer_id".to_string(), json!(peer.to_string()));
            obj.insert("addrs".to_string(), json!(addrs));
            serde_json::Value::Object(obj).to_string()
        }
        _ => base,
    }
}

/// Runs the daemon until `shutdown` is requested (the `shutdown` op) or stdin
/// closes. Never returns early on a bad request line or a relay dial failure —
/// only a setup failure (bad listen address, `Network::spawn` failure, etc.)
/// or a clean shutdown ends the loop.
pub async fn run_daemon(cfg: DaemonConfig) -> Result<(), Box<dyn Error>> {
    // Single stdout writer: every event line — `ready`, per-request results,
    // progress, errors, warnings — funnels through this channel so concurrent
    // per-request tasks never interleave a partial line. `println!` happens
    // *only* here.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
    // Requested by the writer when stdout dies, and by the `shutdown` op.
    let shutdown = Arc::new(Notify::new());
    let writer_shutdown = shutdown.clone();
    let writer = tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut out_rx = out_rx;
        let stdout = std::io::stdout();
        let mut sink = stdout.lock();
        while let Some(line) = out_rx.blocking_recv() {
            // `println!` *panics* on a closed pipe: the writer task would die,
            // its error would be swallowed by the `let _ = writer.await` below,
            // and the daemon would keep doing work nobody can read. Write
            // explicitly instead and ask for shutdown when the reader is gone.
            let wrote = sink
                .write_all(line.as_bytes())
                .and_then(|()| sink.write_all(b"\n"))
                .and_then(|()| sink.flush());
            if wrote.is_err() {
                writer_shutdown.notify_one();
                out_rx.close();
                break;
            }
        }
    });

    // Blocking stdin read on a plain thread, forwarded into the async loop —
    // see the module doc's "deliberate deviations" note for why this isn't
    // `tokio::io::stdin()`.
    //
    // The overflow reporter is a *weak* sender on purpose: a strong clone held
    // by this thread (which lives until stdin EOF) would keep the writer's
    // channel open past shutdown.
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<String>();
    let overflow_tx = out_tx.downgrade();
    std::thread::spawn(move || {
        use std::io::{BufRead, Read};
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            // Bounded read: `lines()` would buffer a newline-less stream until
            // the process runs out of memory.
            let read = (&mut reader).take(MAX_LINE_BYTES as u64 + 1).read_until(b'\n', &mut buf);
            match read {
                Ok(0) => break, // EOF
                Ok(_) => {}
                Err(_) => break,
            }
            if buf.len() > MAX_LINE_BYTES && !buf.ends_with(b"\n") {
                if let Some(tx) = overflow_tx.upgrade() {
                    let _ = tx.send(event_error(
                        0,
                        &format!("request line exceeds {MAX_LINE_BYTES} bytes — dropped"),
                    ));
                }
                // Discard the rest of the oversized line so the next parse
                // starts on a real request boundary.
                let mut skip: Vec<u8> = Vec::new();
                let ended = loop {
                    skip.clear();
                    match (&mut reader).take(MAX_LINE_BYTES as u64).read_until(b'\n', &mut skip) {
                        Ok(0) | Err(_) => break false,
                        Ok(_) if skip.ends_with(b"\n") => break true,
                        Ok(_) => continue,
                    }
                };
                if !ended {
                    break;
                }
                continue;
            }
            let line = String::from_utf8_lossy(&buf).into_owned();
            if in_tx.send(line).is_err() {
                break;
            }
        }
    });

    // Same reopen-after-`Network::spawn`-consumes-one pattern `cmd_serve`/
    // `cmd_torrent` use: `Network::spawn` takes ownership of one `Store`
    // instance for its own chunk serving; op handlers share a second one.
    let net = Network::spawn(Store::open(&cfg.store_dir)?, Some(cfg.identity_seed))?;
    let store = Arc::new(Store::open(&cfg.store_dir)?);
    let http = reqwest::Client::builder().timeout(TRACKER_TIMEOUT).build()?;

    net.listen(DEFAULT_LISTEN.parse()?).await?;

    if let Some(relay) = &cfg.relay {
        match relay.parse::<Multiaddr>() {
            Ok(addr) => {
                if let Err(e) = net.dial(addr).await {
                    let _ = out_tx.send(event_warn(&format!("relay dial failed: {e}")));
                }
            }
            Err(e) => {
                let _ = out_tx.send(event_warn(&format!("invalid relay address {relay:?}: {e}")));
            }
        }
    }

    // Reap a leftover `.old`/`.new` from a previous run, same as `cmd_update`:
    // the design is that the `.old` copy dies on the next successful start, and
    // a daemon-only embedder never runs `cmd_update` to do it.
    crate::update::cleanup_old_binary();

    let addrs = listen_addrs(&net).await;
    let _ = out_tx.send(ready_line(env!("CARGO_PKG_VERSION"), net.local_peer_id(), &addrs));

    if cfg.auto_update {
        // Auto-update stays on by default, but the swap is never invisible: a
        // binary that changed under the embedder gets announced on the protocol
        // (`updated`, with from/to) so it can react — restart, re-handshake,
        // log it — instead of only finding out from stderr.
        let tx = out_tx.clone();
        tokio::task::spawn_blocking(move || match crate::update::check_and_update(Duration::from_secs(30)) {
            Ok(report) if report.updated => {
                let _ = tx.send(
                    json!({"event": "updated", "from": report.from, "to": report.to}).to_string(),
                );
            }
            Ok(_) => {}
            Err(e) => eprintln!("auto-update check failed: {e}"),
        });
    }

    let ctx = Ctx {
        net,
        store,
        store_dir: cfg.store_dir,
        tracker: cfg.tracker,
        http,
        tx: out_tx.clone(),
        announces: Arc::new(AsyncMutex::new(HashMap::new())),
        shutdown,
    };

    loop {
        tokio::select! {
            _ = ctx.shutdown.notified() => break,
            maybe_line = in_rx.recv() => {
                let Some(line) = maybe_line else { break }; // stdin closed
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some(v) = dial_fallback_value(line) {
                    let ctx = ctx.clone();
                    let dial_id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
                    tokio::spawn(async move {
                        let tx = ctx.tx.clone();
                        // Same panic guard `execute` uses — see there.
                        if tokio::spawn(async move { dispatch_dial(v, ctx).await }).await.is_err() {
                            let _ = tx.send(event_error(dial_id, "internal error"));
                        }
                    });
                    continue;
                }
                if let Some(req) = handle_line(line, &ctx.tx) {
                    let ctx = ctx.clone();
                    tokio::spawn(async move { execute(req.id, req.op, ctx).await });
                }
            }
        }
    }

    // Every in-flight task holds its own clone of `out_tx`; dropping this one
    // lets the writer's channel close once they finish, instead of cutting
    // them off — but only up to [`SHUTDOWN_GRACE`]. Waiting unconditionally
    // would make `shutdown` during a multi-GB `fetch` block until that fetch
    // ended, which is not a shutdown.
    drop(ctx);
    drop(out_tx);
    tokio::select! {
        _ = writer => {}
        _ = tokio::time::sleep(SHUTDOWN_GRACE) => {}
    }
    Ok(())
}

/// `Some(raw JSON value)` if `line` is a `{"cmd":"dial", ...}` request — this
/// op isn't a [`proto::Op`] variant (see the module doc), so it's recognized
/// ahead of [`handle_line`]/[`parse_request`] instead of through them.
fn dial_fallback_value(line: &str) -> Option<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("cmd").and_then(|c| c.as_str()) == Some("dial") {
        Some(v)
    } else {
        None
    }
}

async fn dispatch_dial(v: serde_json::Value, ctx: Ctx) {
    let id = v.get("id").and_then(|x| x.as_u64()).unwrap_or(0);
    let Some(addr) = v.get("addr").and_then(|a| a.as_str()) else {
        let _ = ctx.tx.send(event_error(id, "dial requires a string \"addr\" (multiaddr)"));
        return;
    };
    let result: Result<(), String> = async {
        let ma: Multiaddr = addr.parse().map_err(|e| format!("{e}"))?;
        ctx.net.dial(ma).await.map_err(|e| e.to_string())
    }
    .await;
    match result {
        Ok(()) => {
            let _ = ctx.tx.send(event_result(id, json!({"addr": addr})));
        }
        Err(e) => {
            let _ = ctx.tx.send(event_error(id, &e));
        }
    }
}

/// Runs one request's op and reports its outcome (`result` or `error`) to
/// `ctx.tx`. Isolated in its own `tokio::spawn`'d task by the caller so a slow
/// or stuck op never blocks other in-flight requests.
///
/// The op itself runs in a *nested* task so the contract — exactly one terminal
/// event per id, never zero — survives a panic inside it (an `unwrap` deep in
/// store/bridge code, a client that fails to build). Without the guard the
/// daemon lives on while that one id gets no event at all and its caller waits
/// forever.
async fn execute(id: u64, op: Op, ctx: Ctx) {
    let tx = ctx.tx.clone();
    let line = match tokio::spawn(async move { run_op(id, op, &ctx).await }).await {
        Ok(Ok(fields)) => event_result(id, fields),
        Ok(Err(e)) => event_error(id, &e),
        Err(_) => event_error(id, "internal error"),
    };
    let _ = tx.send(line);
}

async fn run_op(id: u64, op: Op, ctx: &Ctx) -> Result<serde_json::Value, String> {
    match op {
        Op::Fetch { uri, out } => run_fetch(id, &uri, &out, ctx).await,
        Op::Convert { torrent, data, path } => run_convert(id, torrent, data, path, ctx).await,
        Op::Torrent { input, out } => run_torrent(id, &input, out.as_deref(), ctx).await,
        Op::Provide { nptp } => run_provide(&nptp, ctx).await,
        Op::Unprovide { root } => run_unprovide(&root, ctx).await,
        Op::Status {} => run_status(ctx).await,
        Op::Shutdown {} => {
            ctx.shutdown.notify_one();
            Ok(json!({}))
        }
    }
}

/// A tree's content goes under a directory on export; a single file to a file
/// path. Mirrors `main.rs`'s `looks_like_tree` (private there, in the `bin`
/// crate — see the module doc).
fn looks_like_tree(manifest: &Manifest) -> bool {
    manifest.files.len() > 1 || manifest.files.first().is_some_and(|f| f.path.contains('/'))
}

fn write_output_to(
    store: &Store,
    manifest: &Manifest,
    out: &str,
    on_progress: impl FnMut(usize, usize),
) -> Result<String, String> {
    if looks_like_tree(manifest) {
        store
            .export_tree_to_dir_with_progress(manifest, Path::new(out), on_progress)
            .map_err(|e| e.to_string())?;
        Ok(format!("{out}/ ({} files)", manifest.files.len()))
    } else {
        let file = fs::File::create(out).map_err(|e| e.to_string())?;
        store.export_to_with_progress(manifest, file, on_progress).map_err(|e| e.to_string())?;
        Ok(out.to_string())
    }
}

async fn run_fetch(id: u64, uri: &str, out: &str, ctx: &Ctx) -> Result<serde_json::Value, String> {
    let root = match uri.strip_prefix("np2ptp:") {
        Some(hex) => Hash::from_hex(hex).map_err(|e| e.to_string())?,
        None => Manifest::from_nptp(&fs::read(uri).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
            .root,
    };

    // Tracker first, but never fatally: an embedder that dialed its peer itself
    // (the `dial` op), or runs with no reachable tracker at all, must still be
    // able to fetch. A tracker failure is a `warn`; peers we already hold a
    // connection to are folded in as candidates, and
    // `download_multi_with_progress` skips whichever of them turns out not to
    // have the content.
    let mut discovered: Vec<PeerId> = Vec::new();
    match tracker_get_peers(&ctx.http, &ctx.tracker, root).await {
        Ok(candidates) => {
            for (peer, addrs) in &candidates {
                for addr in addrs {
                    // `add_peer` before `dial`: request-response needs a known
                    // address for the peer or it fails the pending request with
                    // `NoAddresses`, and `dial` returns as soon as the dial is
                    // *initiated* — well before the QUIC handshake lands.
                    let _ = ctx.net.add_peer(*peer, addr.clone()).await;
                    let _ = ctx.net.dial(addr.clone()).await;
                }
                discovered.push(*peer);
            }
        }
        Err(e) => {
            let _ = ctx.tx.send(event_warn(&format!("tracker lookup failed: {e}")));
        }
    }

    // Settle *before* deciding there is nobody to fetch from. This runs whether
    // or not the tracker returned anything, because the documented `dial` then
    // `fetch` flow has an empty tracker set and a handshake still in flight:
    // the `dial` op answers as soon as the swarm queued the attempt, and
    // `connected_peers` only reports established connections, so a `providers`
    // check taken right now sees zero peers and would fail the op terminally.
    // Same 60 x 50ms shape `cmd_fetch` uses; emits no events either way.
    let mut connected = ctx.net.connected_peers().await.unwrap_or_default();
    for _ in 0..60 {
        let settled = if discovered.is_empty() {
            !connected.is_empty()
        } else {
            discovered.iter().any(|p| connected.contains(p))
        };
        if settled {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        connected = ctx.net.connected_peers().await.unwrap_or_default();
    }

    // Providers are the tracker's peers plus whoever we ended up connected to.
    // `download_multi_with_progress` skips whichever of them lacks the content.
    let mut providers: Vec<PeerId> = discovered.clone();
    for peer in connected {
        if !providers.contains(&peer) {
            providers.push(peer);
        }
    }
    if providers.is_empty() {
        return Err("no peers to fetch from: none on the tracker for this content, and \
                    none connected (dial one first)"
            .to_string());
    }

    // Retry with backoff on top of the settle wait: a transient `NoAddresses`
    // or a connection that dropped mid-handshake must not turn into the
    // operation's terminal error on the first try. No extra events — the
    // retries are invisible, the op still ends in exactly one terminal event.
    //
    // Bounded twice: 5 attempts *and* an overall deadline, so a stale tracker
    // entry pointing at a dead peer reports its single error in seconds rather
    // than stalling the embedder through every attempt.
    let mut prog = progress_sender(ctx.tx.clone(), id, "fetch");
    let mut sent_complete = false;
    let mut manifest = None;
    let mut last_err = String::new();
    let retry_deadline = std::time::Instant::now() + Duration::from_secs(20);
    for attempt in 0..5u32 {
        if attempt > 0 {
            if std::time::Instant::now() >= retry_deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50 << (attempt - 1))).await;
        }
        match ctx
            .net
            .download_multi_with_progress(root, &providers, &ctx.store, |done, total| {
                // A retry that got all the way to done == total and then failed
                // late would otherwise emit that completion progress event once
                // per attempt; an embedder keying "finished" off progress must
                // see it at most once per op.
                if done == total && total > 0 {
                    if sent_complete {
                        return;
                    }
                    sent_complete = true;
                }
                prog(done as u64, total as u64);
            })
            .await
        {
            Ok(m) => {
                manifest = Some(m);
                break;
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    let manifest = manifest.ok_or(last_err)?;

    // Export re-reads and re-verifies every chunk — fully synchronous,
    // GB-scale work. Run on an async worker it would stall the libp2p swarm
    // sharing that pool (including the chunk serving other peers are waiting
    // on), so it goes to the blocking pool instead.
    let store = ctx.store.clone();
    let out_path = out.to_string();
    let export_manifest = manifest.clone();
    let tx = ctx.tx.clone();
    let dest = tokio::task::spawn_blocking(move || {
        let mut prog = progress_sender(tx, id, "fetch");
        write_output_to(&store, &export_manifest, &out_path, |done, total| {
            prog(done as u64, total as u64);
        })
    })
    .await
    .map_err(|_| "internal error: export task failed".to_string())??;

    Ok(json!({
        "root": manifest.uri(),
        "path": dest,
        "bytes_total": manifest.total_size,
    }))
}

async fn run_convert(
    id: u64,
    torrent: Option<String>,
    data: Option<String>,
    path: Option<String>,
    ctx: &Ctx,
) -> Result<serde_json::Value, String> {
    match (torrent, data, path) {
        (Some(torrent_path), Some(data_dir), None) => {
            let bytes = fs::read(&torrent_path).map_err(|e| e.to_string())?;
            let meta = np2ptp_bridge::parse_torrent_file(&bytes).map_err(|e| e.to_string())?;
            let outcome =
                np2ptp_bridge::resolve_or_convert_local(&ctx.net, &ctx.store, &meta, Path::new(&data_dir), false)
                    .await
                    .map_err(|e| e.to_string())?;
            // Register so the root this result hands back can be `provide`d
            // (and survives into `serve --all`). The bridge's own `net.provide`
            // leaves no registry entry and no announce loop, so without this
            // the converted content is invisible to both `status` and the
            // tracker — a dead end for the convert -> seed flow.
            crate::register_manifest(Path::new(&ctx.store_dir), &outcome.manifest)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "root": outcome.manifest.uri(),
                "converted": outcome.converted,
                "infohash": hex_encode(&outcome.infohash),
                "files_total": outcome.manifest.files.len(),
                "chunks_total": outcome.manifest.chunks.len(),
                "bytes_total": outcome.manifest.total_size,
            }))
        }
        (None, None, Some(path)) => {
            let name = Path::new(&path).file_name().map(|s| s.to_string_lossy().into_owned());
            let is_dir = fs::metadata(&path).map_err(|e| e.to_string())?.is_dir();
            let files: Vec<(String, PathBuf)> = if is_dir {
                let files = crate::read_dir_paths(Path::new(&path)).map_err(|e| e.to_string())?;
                // Same guard `cmd_pack` has: without it an empty directory
                // yields `ok:true` and a degenerate zero-file manifest.
                if files.is_empty() {
                    return Err(format!("convert: directory {path} contains no files"));
                }
                files
            } else {
                let file_name = name.clone().unwrap_or_else(|| "data".to_string());
                vec![(file_name, PathBuf::from(&path))]
            };

            // Whole-tree hashing is synchronous and unbounded — same reasoning
            // as the export in `run_fetch`: off the async workers.
            let store = ctx.store.clone();
            let tx = ctx.tx.clone();
            let (manifest, chunks_new) = tokio::task::spawn_blocking(move || {
                let mut chunks_new = 0u64;
                let mut prog = progress_sender(tx, id, "convert");
                let mut on_progress = |done: u64, total: u64, is_new: bool| {
                    if is_new {
                        chunks_new += 1;
                    }
                    prog(done, total);
                };
                let result =
                    store.ingest_tree_files_no_copy_with_progress(&files, name, &mut on_progress);
                result.map(|m| (m, chunks_new)).map_err(|e| e.to_string())
            })
            .await
            .map_err(|_| "internal error: convert task failed".to_string())??;

            // Convert -> seed, closed: `provide` accepts this result's root
            // because the manifest is now in the store's registry.
            crate::register_manifest(Path::new(&ctx.store_dir), &manifest)
                .map_err(|e| e.to_string())?;
            Ok(json!({
                "root": manifest.uri(),
                "chunks_total": manifest.chunks.len(),
                "chunks_new": chunks_new,
                "bytes_total": manifest.total_size,
            }))
        }
        // `parse_request` already rejects every other shape; this is a fail-safe,
        // not a reachable path — no `unreachable!()` in a daemon that must never
        // panic out from under a live connection.
        _ => Err("convert requires exactly one of: torrent+data together, or path alone".to_string()),
    }
}

#[cfg(feature = "librqbit")]
async fn run_torrent(id: u64, input: &str, out: Option<&str>, ctx: &Ctx) -> Result<serde_json::Value, String> {
    let mut on_progress = progress_sender(ctx.tx.clone(), id, "torrent");
    let outcome = np2ptp_bridge::resolve_or_convert_remote(
        &ctx.net,
        &ctx.store,
        input,
        false,
        out.map(Path::new),
        &mut on_progress,
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok(json!({
        "root": outcome.manifest.uri(),
        "converted": outcome.converted,
        "files_total": outcome.manifest.files.len(),
        "chunks_total": outcome.manifest.chunks.len(),
        "bytes_total": outcome.manifest.total_size,
    }))
}

#[cfg(not(feature = "librqbit"))]
async fn run_torrent(_id: u64, input: &str, out: Option<&str>, ctx: &Ctx) -> Result<serde_json::Value, String> {
    let _ = (input, out, ctx);
    Err("torrent: this daemon build lacks the 'librqbit' feature (rebuild with \
         `cargo build --features librqbit`); use 'convert' with torrent+data for \
         content you've already downloaded"
        .to_string())
}

/// Path of a root's entry in the store's manifest registry. Mirrors
/// `serve_set::manifests_dir` (private there — see the module doc on why this
/// module can't reach into the binary crate's helpers either).
fn registry_path(store_dir: &str, root: Hash) -> PathBuf {
    Path::new(store_dir).join("manifests").join(format!("{}.nptp", root.to_hex()))
}

/// `provide`'s argument: a `.nptp` file path, **or** an `np2ptp:<hex>` root
/// resolved from the store's registry.
///
/// The root form is what makes convert -> provide work: the daemon's `convert`
/// hands back a root, not a file, so a path-only `provide` would leave content
/// converted through the daemon impossible to seed through the daemon.
fn load_provide_manifest(target: &str, store_dir: &str) -> Result<Manifest, String> {
    let Some(hex) = target.strip_prefix("np2ptp:") else {
        let bytes = fs::read(target).map_err(|e| e.to_string())?;
        return Manifest::from_nptp(&bytes).map_err(|e| e.to_string());
    };
    let root = Hash::from_hex(hex).map_err(|e| e.to_string())?;
    let path = registry_path(store_dir, root);
    let bytes = fs::read(&path).map_err(|e| {
        format!(
            "provide {target}: no manifest registered at {} ({e}) — convert it, or provide it \
             once by .nptp path, first",
            path.display()
        )
    })?;
    Manifest::from_nptp(&bytes).map_err(|e| e.to_string())
}

async fn run_provide(nptp: &str, ctx: &Ctx) -> Result<serde_json::Value, String> {
    let manifest = load_provide_manifest(nptp, &ctx.store_dir)?;
    ctx.net.provide(&manifest).await.map_err(|e| e.to_string())?;
    crate::register_manifest(Path::new(&ctx.store_dir), &manifest).map_err(|e| e.to_string())?;

    let root = manifest.root;
    let handle = {
        let tracker = ctx.tracker.clone();
        let http = ctx.http.clone();
        let net = ctx.net.clone();
        let tx = ctx.tx.clone();
        tokio::spawn(async move { announce_loop(tracker, http, net, root, tx).await })
    };
    if let Some(old) = ctx.announces.lock().await.insert(root, handle) {
        old.abort(); // re-`provide`ing the same root replaces its announce task
    }

    Ok(json!({
        "root": manifest.uri(),
        "files_total": manifest.files.len(),
        "chunks_total": manifest.chunks.len(),
    }))
}

async fn run_unprovide(root: &str, ctx: &Ctx) -> Result<serde_json::Value, String> {
    // Both forms accepted: bare hex, or the `np2ptp:<hex>` URI that every
    // `provide`/`status`/`fetch` result hands back — feeding our own output
    // straight back in has to work.
    let hex = root.strip_prefix("np2ptp:").unwrap_or(root);
    let root_hash = Hash::from_hex(hex).map_err(|e| e.to_string())?;
    if let Some(handle) = ctx.announces.lock().await.remove(&root_hash) {
        handle.abort();
    }
    ctx.net.unprovide(root_hash).await.map_err(|e| e.to_string())?;
    // Drop the registry entry too, or a later `serve --all` (which serves
    // everything ever registered) resurrects content the user explicitly
    // retired. Already-gone is success; anything else is worth a `warn`.
    let path = registry_path(&ctx.store_dir, root_hash);
    if let Err(e) = fs::remove_file(&path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            let _ = ctx
                .tx
                .send(event_warn(&format!("could not remove {}: {e}", path.display())));
        }
    }
    Ok(json!({ "root": format!("np2ptp:{}", root_hash.to_hex()) }))
}

async fn run_status(ctx: &Ctx) -> Result<serde_json::Value, String> {
    let peers = ctx.net.connected_peers().await.map_err(|e| e.to_string())?;
    let provided: Vec<String> =
        ctx.announces.lock().await.keys().map(|h| format!("np2ptp:{}", h.to_hex())).collect();
    let ledger = ctx.net.ledger_totals().await.map_err(|e| e.to_string())?;
    let listeners = ctx.net.listeners().await.unwrap_or_default();
    Ok(json!({
        "peers": peers.len(),
        "peer_id": ctx.net.local_peer_id().to_string(),
        "addrs": addr_strs_with_peer(&listeners, ctx.net.local_peer_id()),
        "provided": provided,
        "ledger": {
            "served_to_us": ledger.served_to_us,
            "we_served": ledger.we_served,
            "credited_by_receipts": ledger.credited_by_receipts,
        },
    }))
}

/// Same cadence as `cmd_serve`'s announce loop (`tokio::time::interval` fires
/// once immediately, then every [`ANNOUNCE_INTERVAL`]). Aborted from the
/// `HashMap` by `run_unprovide` or a replacing `run_provide`.
///
/// A failing announce is reported, not swallowed: silently, the content stops
/// being discoverable while `status.provided` keeps listing it, and the
/// embedder has no way to know. Rate-limited to [`ANNOUNCE_WARN_INTERVAL`] so a
/// tracker that's down for a day isn't a warn every two minutes per root.
async fn announce_loop(
    tracker: String,
    http: reqwest::Client,
    net: Network,
    root: Hash,
    tx: mpsc::UnboundedSender<String>,
) {
    let mut interval = tokio::time::interval(ANNOUNCE_INTERVAL);
    let mut last_warn: Option<std::time::Instant> = None;
    loop {
        interval.tick().await;
        let mut addrs = net.listeners().await.unwrap_or_default();
        for ext in net.external_addresses().await.unwrap_or_default() {
            if !addrs.contains(&ext) {
                addrs.push(ext);
            }
        }
        let peer = net.local_peer_id();
        if let Err(e) = tracker_announce(&http, &tracker, root, peer, &addrs).await {
            let now = std::time::Instant::now();
            let due = match last_warn {
                None => true,
                Some(t) => now.duration_since(t) >= ANNOUNCE_WARN_INTERVAL,
            };
            if due {
                last_warn = Some(now);
                let _ = tx.send(event_warn(&format!(
                    "tracker announce for np2ptp:{} failed: {e} (still provided locally, but \
                     peers may not find it)",
                    root.to_hex()
                )));
            }
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------- tracker HTTP client
//
// Reimplemented (not `use`d from `main.rs`'s `tracker` module) because that
// module is only `mod tracker;`-included into the `np2ptp` *binary* crate —
// this daemon code lives in the `np2ptp_node` *library* crate, a separate
// compilation unit that never sees it. Same wire format as `tracker.rs`'s
// `announce`/`get_peers`.

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

async fn tracker_announce(
    http: &reqwest::Client,
    tracker: &str,
    cid: Hash,
    peer: PeerId,
    addrs: &[Multiaddr],
) -> Result<(), String> {
    let addr_strs = addr_strs_with_peer(addrs, peer);
    let body = json!({
        "cid": cid.to_hex(),
        "peer": peer.to_string(),
        "addrs": addr_strs,
    });
    http.post(format!("{tracker}/announce"))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn tracker_get_peers(
    http: &reqwest::Client,
    tracker: &str,
    cid: Hash,
) -> Result<Vec<(PeerId, Vec<Multiaddr>)>, String> {
    let url = format!("{tracker}/peers?cid={}", cid.to_hex());
    let mut response =
        http.get(url).send().await.map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?;

    // Read the body in chunks against [`TRACKER_MAX_BODY`] rather than
    // `.json()`, which would buffer whatever the far end decides to send.
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
        if body.len() + chunk.len() > TRACKER_MAX_BODY {
            return Err(format!("tracker response exceeds {TRACKER_MAX_BODY} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    let resp: PeersResp = serde_json::from_slice(&body).map_err(|e| e.to_string())?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bad_line_emits_error_and_daemon_survives() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let _ = handle_line("not json", &tx);
        let line = rx.recv().await.unwrap();
        assert!(line.contains(r#""event":"error""#));
    }

    #[tokio::test]
    async fn valid_json_bad_request_error_keeps_the_callers_id() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let _ = handle_line(r#"{"id":7,"cmd":"fetch"}"#, &tx); // no `out`
        let v: serde_json::Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(v["event"], "error");
        assert_eq!(v["id"], 7, "the pending id must get its terminal event");
    }

    #[test]
    fn progress_is_throttled_but_completion_always_lands() {
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut prog = progress_sender(tx, 5, "fetch");
        for done in 0..100 {
            prog(done, 100);
        }
        prog(100, 100);
        let mut lines = Vec::new();
        while let Ok(l) = rx.try_recv() {
            lines.push(l);
        }
        assert!(lines.len() <= 3, "throttle let {} events through", lines.len());
        let last: serde_json::Value = serde_json::from_str(lines.last().unwrap()).unwrap();
        assert_eq!(last["done"], 100);
    }

    #[tokio::test]
    async fn well_formed_line_is_not_reported_as_an_error() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let req = handle_line(r#"{"id":1,"cmd":"status"}"#, &tx);
        assert!(req.is_some());
        assert!(rx.try_recv().is_err()); // nothing sent — caller executes the op
    }

    #[test]
    fn peer_suffix_is_idempotent() {
        let peer: PeerId = "12D3KooWSzXtDVLLFf2avw9bpcMCRsE7JvbdQNEcd45MKuRsGmyR".parse().unwrap();
        let bare: Multiaddr = "/ip4/127.0.0.1/udp/4001/quic-v1".parse().unwrap();
        let suffixed: Multiaddr = format!("/ip4/127.0.0.1/udp/4001/quic-v1/p2p/{peer}").parse().unwrap();
        let out = addr_strs_with_peer(&[bare, suffixed], peer);
        assert_eq!(out[0], out[1]);
        assert!(out[0].ends_with(&format!("/p2p/{peer}")));
    }

    #[test]
    fn ready_carries_peer_id_and_addrs() {
        let peer: PeerId = "12D3KooWSzXtDVLLFf2avw9bpcMCRsE7JvbdQNEcd45MKuRsGmyR".parse().unwrap();
        let line = ready_line("9.9.9", peer, &["/ip4/127.0.0.1/udp/1/quic-v1".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["event"], "ready");
        assert_eq!(v["version"], "9.9.9");
        assert_eq!(v["peer_id"], peer.to_string());
        assert_eq!(v["addrs"][0], "/ip4/127.0.0.1/udp/1/quic-v1");
    }

    #[test]
    fn dial_fallback_recognizes_only_dial_cmd() {
        assert!(dial_fallback_value(r#"{"id":1,"cmd":"dial","addr":"/ip4/1.2.3.4/udp/1/quic-v1"}"#).is_some());
        assert!(dial_fallback_value(r#"{"id":1,"cmd":"status"}"#).is_none());
        assert!(dial_fallback_value("not json").is_none());
    }
}
