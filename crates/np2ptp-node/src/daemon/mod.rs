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
//!    `tracker_*` note) + `Network::download_multi_with_progress`, then
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
//!    `HashMap<Hash, JoinHandle<()>>`.
//!  - `unprovide` -> abort that task + `Network::unprovide`.
//!  - `status`    -> `connected_peers` + the provided-roots map + `ledger_totals`.
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
    tx: mpsc::UnboundedSender<String>,
    announces: Arc<AsyncMutex<HashMap<Hash, JoinHandle<()>>>>,
    shutdown: Arc<Notify>,
}

/// Parses one NDJSON line and, on failure, sends an `error` event straight to
/// `tx` (id `0` — a line that didn't parse into a `Request` has no id we can
/// trust). Pure otherwise: no `Network`, no I/O beyond the channel send, so
/// it's unit-testable without any networking spun up.
fn handle_line(line: &str, tx: &mpsc::UnboundedSender<String>) -> Option<Request> {
    match parse_request(line) {
        Ok(req) => Some(req),
        Err(e) => {
            let _ = tx.send(event_error(0, &e));
            None
        }
    }
}

fn event_warn(message: &str) -> String {
    json!({"event": "warn", "message": message}).to_string()
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
    let writer = tokio::task::spawn_blocking(move || {
        let mut out_rx = out_rx;
        while let Some(line) = out_rx.blocking_recv() {
            println!("{line}");
        }
    });

    // Blocking stdin read on a plain thread, forwarded into the async loop —
    // see the module doc's "deliberate deviations" note for why this isn't
    // `tokio::io::stdin()`.
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(l) => {
                    if in_tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // Same reopen-after-`Network::spawn`-consumes-one pattern `cmd_serve`/
    // `cmd_torrent` use: `Network::spawn` takes ownership of one `Store`
    // instance for its own chunk serving; op handlers share a second one.
    let net = Network::spawn(Store::open(&cfg.store_dir)?, Some(cfg.identity_seed))?;
    let store = Arc::new(Store::open(&cfg.store_dir)?);

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

    let _ = out_tx.send(event_ready(env!("CARGO_PKG_VERSION")));

    if cfg.auto_update {
        // Best-effort, silent on success/failure beyond stderr — the daemon's
        // stdout protocol has no event for this in the Task 7/8 contract, and
        // inventing one here would mean guessing at what Task 8 expects.
        tokio::task::spawn_blocking(|| {
            if let Err(e) = crate::update::check_and_update(Duration::from_secs(30)) {
                eprintln!("auto-update check failed: {e}");
            }
        });
    }

    let ctx = Ctx {
        net,
        store,
        store_dir: cfg.store_dir,
        tracker: cfg.tracker,
        tx: out_tx.clone(),
        announces: Arc::new(AsyncMutex::new(HashMap::new())),
        shutdown: Arc::new(Notify::new()),
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
                    tokio::spawn(async move { dispatch_dial(v, ctx).await });
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
    // just lets the writer's channel close once they finish, instead of
    // cutting them off.
    drop(ctx);
    drop(out_tx);
    let _ = writer.await;
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
async fn execute(id: u64, op: Op, ctx: Ctx) {
    match run_op(id, op, &ctx).await {
        Ok(fields) => {
            let _ = ctx.tx.send(event_result(id, fields));
        }
        Err(e) => {
            let _ = ctx.tx.send(event_error(id, &e));
        }
    }
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

    let candidates = tracker_get_peers(&ctx.tracker, root).await?;
    if candidates.is_empty() {
        return Err("no peers found on the tracker for this content".to_string());
    }
    for (_, addrs) in &candidates {
        for addr in addrs {
            let _ = ctx.net.dial(addr.clone()).await;
        }
    }
    let providers: Vec<PeerId> = candidates.iter().map(|(p, _)| *p).collect();

    let tx = ctx.tx.clone();
    let manifest = ctx
        .net
        .download_multi_with_progress(root, &providers, &ctx.store, |done, total| {
            let _ = tx.send(event_progress(id, "fetch", done as u64, total as u64));
        })
        .await
        .map_err(|e| e.to_string())?;

    let tx = ctx.tx.clone();
    let dest = write_output_to(&ctx.store, &manifest, out, |done, total| {
        let _ = tx.send(event_progress(id, "fetch", done as u64, total as u64));
    })?;

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
            let mut chunks_new = 0u64;
            let tx = ctx.tx.clone();
            let mut on_progress = |done: u64, total: u64, is_new: bool| {
                if is_new {
                    chunks_new += 1;
                }
                let _ = tx.send(event_progress(id, "convert", done, total));
            };
            let manifest = if is_dir {
                let files = crate::read_dir_paths(Path::new(&path)).map_err(|e| e.to_string())?;
                ctx.store
                    .ingest_tree_files_no_copy_with_progress(&files, name, &mut on_progress)
                    .map_err(|e| e.to_string())?
            } else {
                let file_name = name.clone().unwrap_or_else(|| "data".to_string());
                let entry = [(file_name, PathBuf::from(&path))];
                ctx.store
                    .ingest_tree_files_no_copy_with_progress(&entry, name, &mut on_progress)
                    .map_err(|e| e.to_string())?
            };
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
    let tx = ctx.tx.clone();
    let mut on_progress = |done: u64, total: u64| {
        let _ = tx.send(event_progress(id, "torrent", done, total));
    };
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

async fn run_provide(nptp: &str, ctx: &Ctx) -> Result<serde_json::Value, String> {
    let bytes = fs::read(nptp).map_err(|e| e.to_string())?;
    let manifest = Manifest::from_nptp(&bytes).map_err(|e| e.to_string())?;
    ctx.net.provide(&manifest).await.map_err(|e| e.to_string())?;
    crate::register_manifest(Path::new(&ctx.store_dir), &manifest).map_err(|e| e.to_string())?;

    let root = manifest.root;
    let handle = {
        let tracker = ctx.tracker.clone();
        let net = ctx.net.clone();
        tokio::spawn(async move { announce_loop(tracker, net, root).await })
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
    let root_hash = Hash::from_hex(root).map_err(|e| e.to_string())?;
    if let Some(handle) = ctx.announces.lock().await.remove(&root_hash) {
        handle.abort();
    }
    ctx.net.unprovide(root_hash).await.map_err(|e| e.to_string())?;
    Ok(json!({ "root": format!("np2ptp:{}", root_hash.to_hex()) }))
}

async fn run_status(ctx: &Ctx) -> Result<serde_json::Value, String> {
    let peers = ctx.net.connected_peers().await.map_err(|e| e.to_string())?;
    let provided: Vec<String> =
        ctx.announces.lock().await.keys().map(|h| format!("np2ptp:{}", h.to_hex())).collect();
    let ledger = ctx.net.ledger_totals().await.map_err(|e| e.to_string())?;
    Ok(json!({
        "peers": peers.len(),
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
async fn announce_loop(tracker: String, net: Network, root: Hash) {
    let mut interval = tokio::time::interval(ANNOUNCE_INTERVAL);
    loop {
        interval.tick().await;
        let mut addrs = net.listeners().await.unwrap_or_default();
        for ext in net.external_addresses().await.unwrap_or_default() {
            if !addrs.contains(&ext) {
                addrs.push(ext);
            }
        }
        let peer = net.local_peer_id();
        let _ = tracker_announce(&tracker, root, peer, &addrs).await;
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

async fn tracker_announce(tracker: &str, cid: Hash, peer: PeerId, addrs: &[Multiaddr]) -> Result<(), String> {
    let suffix = format!("/p2p/{peer}");
    let addr_strs: Vec<String> = addrs
        .iter()
        .map(|a| {
            let s = a.to_string();
            if s.ends_with(&suffix) {
                s
            } else {
                format!("{s}{suffix}")
            }
        })
        .collect();
    let body = json!({
        "cid": cid.to_hex(),
        "peer": peer.to_string(),
        "addrs": addr_strs,
    });
    reqwest::Client::new()
        .post(format!("{tracker}/announce"))
        .json(&body)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    Ok(())
}

async fn tracker_get_peers(tracker: &str, cid: Hash) -> Result<Vec<(PeerId, Vec<Multiaddr>)>, String> {
    let url = format!("{tracker}/peers?cid={}", cid.to_hex());
    let resp: PeersResp = reqwest::Client::new()
        .get(url)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

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
        handle_line("not json", &tx);
        let line = rx.recv().await.unwrap();
        assert!(line.contains(r#""event":"error""#));
    }

    #[tokio::test]
    async fn well_formed_line_is_not_reported_as_an_error() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let req = handle_line(r#"{"id":1,"cmd":"status"}"#, &tx);
        assert!(req.is_some());
        assert!(rx.try_recv().is_err()); // nothing sent — caller executes the op
    }

    #[test]
    fn dial_fallback_recognizes_only_dial_cmd() {
        assert!(dial_fallback_value(r#"{"id":1,"cmd":"dial","addr":"/ip4/1.2.3.4/udp/1/quic-v1"}"#).is_some());
        assert!(dial_fallback_value(r#"{"id":1,"cmd":"status"}"#).is_none());
        assert!(dial_fallback_value("not json").is_none());
    }
}
