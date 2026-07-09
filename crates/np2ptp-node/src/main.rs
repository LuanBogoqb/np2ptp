//! `np2ptp` CLI — drive the linker and client from the command line.
//!
//! ```text
//! np2ptp pack <input> [--out <file.nptp>] [--store <dir>] [--name <name>] [--no-copy]
//! np2ptp info <file.nptp>
//! np2ptp get  <file.nptp> --source <store-dir> [--store <dir>] [--out <output>]
//! ```
//!
//! `--source` is another node's store acting as a seed. Once networking exists,
//! it becomes a peer address instead of a directory.

use std::collections::HashMap;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use std::time::Duration;

use np2ptp_core::{Hash, Manifest};
use np2ptp_net::{peer_id_from_multiaddr, Multiaddr, Network, PeerId};
use np2ptp_node::{download_with_progress, read_dir_paths, StoreSource};
use np2ptp_store::Store;

mod portmap;
mod tracker;

const DEFAULT_STORE: &str = ".np2ptp-store";
/// The "principal" public relay + DHT bootstrap node — the always-works fallback
/// when a `serve`r turns out to have no other reachable address (CGNAT, no
/// UPnP/NAT-PMP). Same box as `tracker::DEFAULT_TRACKER`.
const DEFAULT_RELAY: &str = "/ip4/194.163.191.81/udp/4001/quic-v1/p2p/12D3KooWSzXtDVLLFf2avw9bpcMCRsE7JvbdQNEcd45MKuRsGmyR";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

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
        Some("torrent") => cmd_torrent(&args[1..]),
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

/// Split args into positionals and `--flag value` pairs. `value_flags` are the
/// flags that consume the following token; any other `--flag` is a boolean.
fn parse<'a>(args: &'a [String], value_flags: &[&str]) -> (Vec<&'a str>, HashMap<String, String>) {
    let mut positionals = Vec::new();
    let mut flags = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            if value_flags.contains(&a.as_str()) {
                if let Some(v) = args.get(i + 1) {
                    flags.insert(name.to_string(), v.clone());
                    i += 2;
                    continue;
                }
            }
            flags.insert(name.to_string(), String::new());
        } else {
            positionals.push(a.as_str());
        }
        i += 1;
    }
    (positionals, flags)
}

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

fn cmd_info(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, _) = parse(args, &[]);
    let file = *pos.first().ok_or("info: missing <file.nptp>")?;
    let manifest = Manifest::from_nptp(&fs::read(file)?)?;

    println!("name:       {}", manifest.name.as_deref().unwrap_or("(none)"));
    println!("size:       {} bytes", manifest.total_size);
    println!("files:      {}", manifest.files.len());
    println!("chunks:     {}", manifest.chunks.len());
    println!("root:       {}", manifest.root);
    println!("link:       {}", manifest.uri());
    println!("consistent: {}", manifest.root_is_consistent());
    if manifest.files.len() > 1 {
        println!("contents:");
        for entry in manifest.files.iter().take(20) {
            println!("  {:>12}  {}", entry.size, entry.path);
        }
        if manifest.files.len() > 20 {
            println!("  … and {} more", manifest.files.len() - 20);
        }
    }
    Ok(())
}

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
                    serde_json::json!({"event":"progress","op":"get","phase":"downloading","chunks_done":done,"chunks_total":total})
                );
            }
        }
    };

    let report = download_with_progress(&manifest, &source, &local, &mut on_progress)?;

    // Rebuilding the output file re-reads and re-verifies every chunk, which
    // can take a while on its own for large content — report it separately
    // from download progress so --json never goes silent long enough to look
    // hung.
    let mut write_last_emit = std::time::Instant::now();
    let mut on_write_progress = |done: usize, total: usize| {
        if json {
            let now = std::time::Instant::now();
            if done == total || now.duration_since(write_last_emit) >= Duration::from_millis(100) {
                write_last_emit = now;
                println!(
                    "{}",
                    serde_json::json!({"event":"progress","op":"get","phase":"writing","chunks_done":done,"chunks_total":total})
                );
            }
        }
    };
    let dest = write_output_with_progress(&local, &manifest, flags.get("out").cloned(), &mut on_write_progress)?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "event":"result","op":"get",
                "root": manifest.uri(),
                "path": dest,
                "bytes_total": manifest.total_size,
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

/// Run as a public relay + DHT bootstrap node — the always-reachable "main node"
/// that lets peers behind CGNAT connect to each other. Run it on a host with a
/// public IP and an open UDP port.
fn cmd_relay(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (_pos, flags) = parse(args, &["--listen", "--public", "--key", "--store"]);
    let listen = flags
        .get("listen")
        .cloned()
        .unwrap_or_else(|| "/ip4/0.0.0.0/udp/4001/quic-v1".to_string());
    let key_path = flags.get("key").cloned().unwrap_or_else(|| "relay.key".to_string());
    let store_dir = flags.get("store").cloned().unwrap_or_else(|| ".np2ptp-relay".to_string());
    let public = flags.get("public").cloned();

    // Stable identity so clients can hardcode the relay's address.
    let seed = load_or_create_seed(&key_path)?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let net = Network::spawn(Store::open(&store_dir)?, Some(seed))?;
        net.listen(listen.parse()?).await?;
        let peer = net.local_peer_id();

        if let Some(p) = &public {
            // Advertise the public address so the reservations this relay grants
            // carry a dialable address (else clients reject them).
            let ext: Multiaddr = if p.starts_with('/') {
                p.parse()?
            } else {
                let port = udp_port(&listen.parse::<Multiaddr>()?).unwrap_or(4001);
                format!("/ip4/{p}/udp/{port}/quic-v1").parse()?
            };
            net.add_external_address(ext.clone()).await?;
            println!("relay peer id: {peer}");
            println!("clients use:   --relay {ext}/p2p/{peer}");
        } else {
            println!("relay peer id: {peer}");
            for a in wait_for_listeners(&net).await {
                println!("  listening: {a}/p2p/{peer}");
            }
            eprintln!("note: pass --public <public-ip> so reservations carry a reachable address");
        }

        println!("\nrelay running. Press Ctrl-C to stop.");
        tokio::signal::ctrl_c().await?;
        println!("\nstopped.");
        Ok::<(), Box<dyn Error>>(())
    })
}

/// Load a 32-byte identity seed from `path`, creating (and saving) one if absent.
fn load_or_create_seed(path: &str) -> Result<[u8; 32], Box<dyn Error>> {
    if let Ok(bytes) = fs::read(path) {
        if bytes.len() == 32 {
            let mut seed = [0u8; 32];
            seed.copy_from_slice(&bytes);
            return Ok(seed);
        }
    }
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).map_err(|e| format!("rng error: {e}"))?;
    write_secret(path, &seed)?;
    Ok(seed)
}

/// Write a private key file with owner-only permissions (0600) on Unix. On
/// Windows the default ACL already restricts it to the owner.
#[cfg(unix)]
fn write_secret(path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(data)
}

#[cfg(not(unix))]
fn write_secret(path: &str, data: &[u8]) -> std::io::Result<()> {
    fs::write(path, data)
}

/// Heuristic: treat as a directory tree if there are multiple files or the single
/// file carries a nested path (so we recreate folders rather than one flat file).
fn looks_like_tree(manifest: &Manifest) -> bool {
    manifest.files.len() > 1 || manifest.files.first().is_some_and(|f| f.path.contains('/'))
}

/// Write reconstructed content from `store` to disk, streaming (no whole-file
/// RAM). A tree goes under a directory; a single file to a file path. Returns a
/// human-readable destination description.
/// Reconstructs `manifest`'s content from `store` at the requested output
/// path, calling `on_progress(chunks_done, chunks_total)` as it goes — this
/// reconstruction phase re-reads and re-verifies every chunk and can take a
/// while on its own for large content. Both CLI callers always pass a real
/// callback (a no-op one in non-`--json` mode), so there is no plain
/// no-progress variant to keep in sync.
fn write_output_with_progress(
    store: &Store,
    manifest: &Manifest,
    out_flag: Option<String>,
    on_progress: impl FnMut(usize, usize),
) -> Result<String, Box<dyn Error>> {
    if looks_like_tree(manifest) {
        let out_dir = out_flag.or_else(|| manifest.name.clone()).unwrap_or_else(|| "download".to_string());
        store.export_tree_to_dir_with_progress(manifest, Path::new(&out_dir), on_progress)?;
        Ok(format!("{out_dir}/ ({} files)", manifest.files.len()))
    } else {
        let out = out_flag.or_else(|| manifest.name.clone()).unwrap_or_else(|| "download.out".to_string());
        store.export_to_with_progress(manifest, fs::File::create(&out)?, on_progress)?;
        Ok(out)
    }
}

async fn wait_for_listeners(net: &Network) -> Vec<Multiaddr> {
    for _ in 0..40 {
        if let Ok(addrs) = net.listeners().await {
            if !addrs.is_empty() {
                return addrs;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Vec::new()
}

/// Extract the UDP port from a `/…/udp/<port>/…` multiaddr.
fn udp_port(addr: &Multiaddr) -> Option<u16> {
    addr.to_string().split("/udp/").nth(1)?.split('/').next()?.parse().ok()
}

/// Seed content on the network: load a `.nptp`, serve its chunks from the store,
/// and announce it on the DHT until interrupted.
fn cmd_serve(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, flags) = parse(args, &["--store", "--listen", "--tracker", "--public", "--relay"]);
    let file = *pos.first().ok_or("serve: missing <file.nptp>")?;
    let manifest = Manifest::from_nptp(&fs::read(file)?)?;
    let store_dir = flags.get("store").map(String::as_str).unwrap_or(DEFAULT_STORE).to_string();
    let store = Store::open(&store_dir)?;
    // Persist identity per store dir: restarting `serve` on the same --store
    // keeps the same peer id, so providers already found (DHT, tracker, a
    // peer's cache) don't lose track of us. Mirrors `relay`'s --key, just
    // automatic — there's no reason to hand out a fresh identity every time.
    let identity_seed = load_or_create_seed(&format!("{store_dir}/identity.key"))?;
    let listen = flags
        .get("listen")
        .cloned()
        .unwrap_or_else(|| "/ip4/0.0.0.0/udp/0/quic-v1".to_string());
    // A reachable public address to advertise (e.g. a router dst-nat / port
    // forward), for when this node is reachable via a public IP it isn't bound to.
    let public = flags.get("public").cloned();
    // Relay fallback is automatic: if nothing below (manual --public, UPnP,
    // NAT-PMP/PCP) produces a reachable external address, we dial the public
    // relay ourselves. `--relay <multiaddr>` forces a specific relay instead of
    // the default; `--no-relay` disables the fallback entirely.
    let relay_override = flags.get("relay").cloned();
    let no_relay = flags.contains_key("no-relay");
    let tracker_url = flags
        .get("tracker")
        .cloned()
        .unwrap_or_else(|| tracker::DEFAULT_TRACKER.to_string());
    let no_tracker = flags.contains_key("no-tracker");
    let json = flags.contains_key("json");

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let net = Network::spawn(store, Some(identity_seed))?;
        net.listen(listen.parse()?).await?;
        net.provide(&manifest).await?;
        let peer = net.local_peer_id();

        if !json {
            println!(
                "serving {} ({} files, {} chunks)",
                manifest.uri(),
                manifest.files.len(),
                manifest.chunks.len()
            );
        }
        let addrs = wait_for_listeners(&net).await;
        if !json {
            if addrs.is_empty() {
                println!("peer id: {peer} (no listen address yet)");
            } else {
                println!("direct fetch:");
                for a in &addrs {
                    println!("  np2ptp fetch {} --peer {a}/p2p/{peer}", manifest.uri());
                }
                if !no_tracker {
                    println!("or, once announced, just: np2ptp fetch {}   (peers found via the tracker)", manifest.uri());
                }
            }
        }

        // Manually-advertised public address (e.g. a MikroTik/router port-forward
        // to a VPN/WireGuard IP) — bypasses CGNAT when the router cooperates.
        if let Some(p) = &public {
            let port = addrs.iter().find_map(udp_port).unwrap_or(4001);
            let ext: Multiaddr = if p.starts_with('/') {
                p.parse()?
            } else {
                format!("/ip4/{p}/udp/{port}/quic-v1").parse()?
            };
            net.add_external_address(ext.clone()).await?;
            if !json {
                println!("public address: {ext}/p2p/{peer}");
            }
        }

        // Try NAT-PMP / PCP for a public address (complements net's UPnP/IGD).
        // Kept alive for the session so the router mapping isn't torn down.
        let portmap_result = match addrs.iter().find_map(udp_port) {
            Some(port) => match portmap::try_map_udp(port).await {
                Ok(mapped) => {
                    if let Some(ip) = portmap::public_ip().await {
                        if let Ok(ext) =
                            format!("/ip4/{ip}/udp/{}/quic-v1", mapped.external_port).parse::<Multiaddr>()
                        {
                            let _ = net.add_external_address(ext.clone()).await;
                            if !json {
                                println!("NAT-PMP/PCP: public address {ext}/p2p/{peer}");
                            }
                        }
                    }
                    Some(mapped)
                }
                Err(e) => {
                    eprintln!("NAT-PMP/PCP: not available ({e})");
                    None
                }
            },
            None => None,
        };

        // Decide, automatically, whether we need the relay fallback: give UPnP a
        // little longer to report in (it's async and may not have fired yet),
        // then check whether *anything* so far (--public, UPnP, NAT-PMP/PCP)
        // produced a real external address.
        let mut has_external = public.is_some() || portmap_result.is_some();
        if !has_external {
            for _ in 0..30 {
                if !net.external_addresses().await.unwrap_or_default().is_empty() {
                    has_external = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }

        let relay_addr_str = if no_relay {
            None
        } else if let Some(r) = relay_override {
            Some(r)
        } else if !has_external {
            if !json {
                println!("no direct/UPnP/NAT-PMP public address — falling back to public relay");
            }
            Some(DEFAULT_RELAY.to_string())
        } else {
            None
        };

        // Reserve a circuit on a public relay — the fallback for CGNAT / no port
        // forward, where UPnP and NAT-PMP/PCP both have nothing to work with.
        if let Some(r) = &relay_addr_str {
            let relay_addr: Multiaddr = r.parse()?;
            if !json {
                println!("relay: dialing {relay_addr} ...");
            }
            net.dial(relay_addr.clone()).await?;
            // The reservation needs an established connection to the relay first.
            tokio::time::sleep(Duration::from_millis(800)).await;
            net.listen(format!("{relay_addr}/p2p-circuit").parse()?).await?;
            let mut got_circuit = false;
            for _ in 0..100 {
                if net
                    .listeners()
                    .await?
                    .iter()
                    .any(|a| a.to_string().contains("p2p-circuit"))
                {
                    got_circuit = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if got_circuit {
                if !json {
                    println!("relay: reservation ok -> {relay_addr}/p2p-circuit/p2p/{peer}");
                }
                if !no_tracker {
                    let addrs = net.listeners().await.unwrap_or_default();
                    if let Err(e) = tracker::announce(&tracker_url, manifest.root, peer, &addrs).await {
                        eprintln!("  (tracker announce failed: {e})");
                    }
                }
            } else {
                eprintln!("relay: no reservation after 10s, continuing without it");
            }
        }

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

/// Download content over the network. With `--peer` it dials that peer directly;
/// without it, it discovers providers via the tracker (`--tracker`, default
/// `https://nptp.bogotec.uk`) and tries each.
fn cmd_fetch(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, flags) = parse(args, &["--peer", "--store", "--out", "--tracker"]);
    let target = *pos.first().ok_or("fetch: missing <np2ptp:ROOT | file.nptp>")?;
    let root = match target.strip_prefix("np2ptp:") {
        Some(hex) => Hash::from_hex(hex)?,
        None => Manifest::from_nptp(&fs::read(target)?)?.root,
    };
    let store_dir = flags.get("store").map(String::as_str).unwrap_or(DEFAULT_STORE).to_string();
    let out_flag = flags.get("out").cloned();
    let use_fec = flags.contains_key("fec");
    let json = flags.contains_key("json");
    let tracker_url = flags
        .get("tracker")
        .cloned()
        .unwrap_or_else(|| tracker::DEFAULT_TRACKER.to_string());

    // Explicit peer, if given; otherwise we discover providers via the tracker.
    let explicit: Option<(PeerId, Multiaddr)> = match flags.get("peer") {
        Some(s) => {
            let addr: Multiaddr = s.parse()?;
            let peer = peer_id_from_multiaddr(&addr)
                .ok_or("fetch: --peer must include the peer id (.../p2p/<peer-id>)")?;
            Some((peer, addr))
        }
        None => None,
    };

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let net = Network::spawn(Store::open(&store_dir)?, None)?;
        let into = Store::open(&store_dir)?;

        let candidates: Vec<(PeerId, Vec<Multiaddr>)> = match explicit {
            Some((peer, addr)) => vec![(peer, vec![addr])],
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
        };

        let mut last_emit = std::time::Instant::now();
        let mut first_done: Option<usize> = None;
        let mut last_total: usize = 0;
        let mut on_progress = |done: usize, total: usize| {
            if first_done.is_none() {
                first_done = Some(done);
            }
            last_total = total;
            if json {
                let now = std::time::Instant::now();
                if done == total || now.duration_since(last_emit) >= Duration::from_millis(100) {
                    last_emit = now;
                    println!(
                        "{}",
                        serde_json::json!({"event":"progress","op":"fetch","phase":"downloading","chunks_done":done,"chunks_total":total})
                    );
                }
            }
        };

        // Try each candidate provider until one serves the content.
        let mut manifest = None;
        let mut last_err: Option<String> = None;
        'outer: for (peer, addrs) in &candidates {
            for addr in addrs {
                let _ = net.dial(addr.clone()).await;
            }
            for _ in 0..60 {
                let attempt = if use_fec {
                    net.download_fec_with_progress(root, *peer, &into, &mut on_progress).await
                } else {
                    net.download_with_progress(root, *peer, &into, &mut on_progress).await
                };
                match attempt {
                    Ok(m) => {
                        manifest = Some(m);
                        break 'outer;
                    }
                    Err(e) => {
                        last_err = Some(e.to_string());
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        }
        let manifest =
            manifest.ok_or_else(|| format!("download failed: {}", last_err.unwrap_or_default()))?;

        // Rebuilding the output file re-reads and re-verifies every chunk,
        // which can take a while on its own for large content — report it
        // separately from download progress so --json never goes silent long
        // enough to look hung.
        let mut write_last_emit = std::time::Instant::now();
        let mut on_write_progress = |done: usize, total: usize| {
            if json {
                let now = std::time::Instant::now();
                if done == total || now.duration_since(write_last_emit) >= Duration::from_millis(100) {
                    write_last_emit = now;
                    println!(
                        "{}",
                        serde_json::json!({"event":"progress","op":"fetch","phase":"writing","chunks_done":done,"chunks_total":total})
                    );
                }
            }
        };
        let dest = write_output_with_progress(&into, &manifest, out_flag, &mut on_write_progress)?;
        if json {
            let deduped = first_done.unwrap_or(0);
            let fetched = last_total.saturating_sub(deduped);
            println!(
                "{}",
                serde_json::json!({
                    "event":"result","op":"fetch",
                    "root": manifest.uri(),
                    "path": dest,
                    "bytes_total": manifest.total_size,
                    "chunks_fetched": fetched,
                    "chunks_deduped": deduped,
                })
            );
        } else {
            println!("fetched {} ({} bytes) -> {dest}", manifest.uri(), manifest.total_size);
        }
        Ok::<(), Box<dyn Error>>(())
    })
}

/// Bridge a torrent into NP2PTP: either an already-downloaded one
/// (`--data <dir>`, which must contain the torrent's file tree directly,
/// e.g. what a BitTorrent client's save-path already looks like for that
/// torrent) or, with the `librqbit` feature, a magnet link / `.torrent` /
/// `http(s)://` URL you don't have yet — downloaded via a real BitTorrent
/// swarm first.
fn cmd_torrent(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, flags) = parse(args, &["--data", "--store", "--relay"]);
    let input = *pos.first().ok_or("torrent: missing <file.torrent|magnet:...>")?;
    let data_dir = flags.get("data").cloned();
    let store_dir = flags.get("store").map(String::as_str).unwrap_or(DEFAULT_STORE).to_string();
    let no_copy = flags.contains_key("no-copy");
    let no_relay = flags.contains_key("no-relay");
    let relay_override = flags.get("relay").cloned();
    let json = flags.contains_key("json");

    // `--data` means "already downloaded" — parse the .torrent file upfront
    // so a bad file fails fast, before any networking spins up.
    let local_meta = match &data_dir {
        Some(_) => Some(np2ptp_bridge::parse_torrent_file(&fs::read(input)?)?),
        None => None,
    };

    // Store::open creates store_dir if it doesn't exist yet — must happen
    // before load_or_create_seed writes identity.key under it.
    let store = Store::open(&store_dir)?;
    let identity_seed = load_or_create_seed(&format!("{store_dir}/identity.key"))?;

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let net = Network::spawn(store, Some(identity_seed))?;
        net.listen("/ip4/0.0.0.0/udp/0/quic-v1".parse()?).await?;

        if !no_relay {
            let relay_addr: Multiaddr = relay_override.unwrap_or_else(|| DEFAULT_RELAY.to_string()).parse()?;
            if !json {
                println!("relay: dialing {relay_addr} ...");
            }
            net.dial(relay_addr).await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let store = Store::open(&store_dir)?;
        let outcome = match (local_meta, &data_dir) {
            (Some(meta), Some(data_dir)) => {
                np2ptp_bridge::resolve_or_convert_local(&net, &store, &meta, Path::new(data_dir), no_copy).await?
            }
            _ => fetch_remote_torrent(&net, &store, input, no_copy).await?,
        };

        if json {
            println!(
                "{}",
                serde_json::json!({
                    "event":"result","op":"torrent",
                    "root": outcome.manifest.uri(),
                    "converted": outcome.converted,
                    "files_total": outcome.manifest.files.len(),
                    "chunks_total": outcome.manifest.chunks.len(),
                    "bytes_total": outcome.manifest.total_size,
                })
            );
        } else {
            println!(
                "{} ({} files, {} chunks) - {}",
                outcome.manifest.uri(),
                outcome.manifest.files.len(),
                outcome.manifest.chunks.len(),
                if outcome.converted { "converted from BitTorrent" } else { "already bridged, served from NP2PTP" }
            );
        }
        Ok::<(), Box<dyn Error>>(())
    })
}

#[cfg(feature = "librqbit")]
async fn fetch_remote_torrent(
    net: &Network,
    store: &Store,
    input: &str,
    no_copy: bool,
) -> Result<np2ptp_bridge::Outcome, Box<dyn Error>> {
    Ok(np2ptp_bridge::resolve_or_convert_remote(net, store, input, no_copy).await?)
}

#[cfg(not(feature = "librqbit"))]
async fn fetch_remote_torrent(
    _net: &Network,
    _store: &Store,
    _input: &str,
    _no_copy: bool,
) -> Result<np2ptp_bridge::Outcome, Box<dyn Error>> {
    Err("torrent: fetching a magnet/torrent you don't already have needs the 'librqbit' \
         feature (rebuild with `cargo build --features librqbit`), or pass --data <dir> \
         for content you've already downloaded"
        .into())
}

fn print_usage() {
    eprintln!(
        "np2ptp — New Peer-To-Peer Transfer Protocol (prototype)\n\n\
         USAGE:\n\
         \x20 np2ptp pack  <input> [--out <file.nptp>] [--store <dir>] [--name <name>] [--no-copy]\n\
         \x20 np2ptp info  <file.nptp>\n\
         \x20 np2ptp get   <file.nptp> --source <store-dir> [--store <dir>] [--out <output>]\n\
         \x20 np2ptp serve <file.nptp> [--store <dir>] [--listen <multiaddr>] [--public <public-ip>] [--tracker <url>] [--relay <multiaddr> | --no-relay]\n\
         \x20 np2ptp fetch <np2ptp:ROOT | file.nptp> [--peer <multiaddr>] [--tracker <url>] [--store <dir>] [--out <output>] [--fec]\n\
         \x20 np2ptp relay [--listen <multiaddr>] [--public <public-ip>] [--key <file>]   (run on a public host)\n\
         \x20 np2ptp torrent <file.torrent|magnet:...> [--data <dir>] [--store <dir>] [--no-copy] [--relay <multiaddr> | --no-relay] [--json]\n\n\
         NOTES:\n\
         \x20 'pack' is the linker: chunks a file/folder into a store and writes a .nptp file.\n\
         \x20 --no-copy references the input in place instead of copying its chunks into the\n\
         \x20 store (no doubled disk usage) — keep the input at that path, unchanged, while seeding.\n\
         \x20 'get' rebuilds content from a local --source store (offline stand-in for a peer).\n\
         \x20 'serve' seeds over the network and announces to a tracker; 'fetch' without --peer\n\
         \x20 discovers providers via the tracker and downloads from them, verifying every chunk.\n\
         \x20 'serve' falls back to the public relay automatically when --public/UPnP/NAT-PMP\n\
         \x20 all fail to find a reachable address (e.g. CGNAT) — override with --relay, or\n\
         \x20 disable with --no-relay.\n\
         \x20 Default store dir: {DEFAULT_STORE}"
    );
}
