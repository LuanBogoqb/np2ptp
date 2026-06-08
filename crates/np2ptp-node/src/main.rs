//! `np2ptp` CLI — drive the linker and client from the command line.
//!
//! ```text
//! np2ptp pack <input> [--out <file.nptp>] [--store <dir>] [--name <name>]
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
use np2ptp_net::{peer_id_from_multiaddr, Multiaddr, Network};
use np2ptp_node::{download, read_dir_paths, StoreSource};
use np2ptp_store::Store;

const DEFAULT_STORE: &str = ".np2ptp-store";

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
    match args.first().map(String::as_str) {
        Some("pack") => cmd_pack(&args[1..]),
        Some("info") => cmd_info(&args[1..]),
        Some("get") => cmd_get(&args[1..]),
        Some("serve") => cmd_serve(&args[1..]),
        Some("fetch") => cmd_fetch(&args[1..]),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command: {other}\n");
            print_usage();
            Err("unknown command".into())
        }
    }
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

    let name = flags.get("name").cloned().or_else(|| {
        Path::new(input).file_name().map(|s| s.to_string_lossy().into_owned())
    });

    // A directory is packed as a tree of files; a single file as one blob. Both
    // stream from disk so packing huge content doesn't load it into memory.
    let manifest = if fs::metadata(input)?.is_dir() {
        let files = read_dir_paths(Path::new(input))?;
        if files.is_empty() {
            return Err(format!("pack: directory {input} contains no files").into());
        }
        store.ingest_tree_files(&files, name)?
    } else {
        let file_name = name
            .clone()
            .or_else(|| Path::new(input).file_name().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "data".to_string());
        store.ingest_tree_files(&[(file_name, Path::new(input).to_path_buf())], name)?
    };

    let out = flags
        .get("out")
        .cloned()
        .unwrap_or_else(|| format!("{input}.nptp"));
    fs::write(&out, manifest.to_nptp()?)?;

    println!("packed {input} ({} bytes) -> {out}", manifest.total_size);
    println!(
        "  files: {}   chunks: {}   store: {store_dir}",
        manifest.files.len(),
        manifest.chunks.len()
    );
    println!("  link:  {}", manifest.uri());
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

    let report = download(&manifest, &source, &local)?;
    let dest = write_output(&local, &manifest, flags.get("out").cloned())?;
    println!("downloaded {} ({} bytes) -> {dest}", manifest.uri(), manifest.total_size);
    println!(
        "  fetched {} chunks, {} already local (deduped)",
        report.fetched, report.deduped
    );
    Ok(())
}

/// Heuristic: treat as a directory tree if there are multiple files or the single
/// file carries a nested path (so we recreate folders rather than one flat file).
fn looks_like_tree(manifest: &Manifest) -> bool {
    manifest.files.len() > 1 || manifest.files.first().is_some_and(|f| f.path.contains('/'))
}

/// Write reconstructed content from `store` to disk, streaming (no whole-file
/// RAM). A tree goes under a directory; a single file to a file path. Returns a
/// human-readable destination description.
fn write_output(store: &Store, manifest: &Manifest, out_flag: Option<String>) -> Result<String, Box<dyn Error>> {
    if looks_like_tree(manifest) {
        let out_dir = out_flag.or_else(|| manifest.name.clone()).unwrap_or_else(|| "download".to_string());
        store.export_tree_to_dir(manifest, Path::new(&out_dir))?;
        Ok(format!("{out_dir}/ ({} files)", manifest.files.len()))
    } else {
        let out = out_flag.or_else(|| manifest.name.clone()).unwrap_or_else(|| "download.out".to_string());
        store.export_to(manifest, fs::File::create(&out)?)?;
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

/// Seed content on the network: load a `.nptp`, serve its chunks from the store,
/// and announce it on the DHT until interrupted.
fn cmd_serve(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, flags) = parse(args, &["--store", "--listen"]);
    let file = *pos.first().ok_or("serve: missing <file.nptp>")?;
    let manifest = Manifest::from_nptp(&fs::read(file)?)?;
    let store_dir = flags.get("store").map(String::as_str).unwrap_or(DEFAULT_STORE).to_string();
    let store = Store::open(&store_dir)?;
    let listen = flags
        .get("listen")
        .cloned()
        .unwrap_or_else(|| "/ip4/0.0.0.0/udp/0/quic-v1".to_string());

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let net = Network::spawn(store, None)?;
        net.listen(listen.parse()?).await?;
        net.provide(&manifest).await?;
        let peer = net.local_peer_id();

        println!(
            "serving {} ({} files, {} chunks)",
            manifest.uri(),
            manifest.files.len(),
            manifest.chunks.len()
        );
        let addrs = wait_for_listeners(&net).await;
        if addrs.is_empty() {
            println!("peer id: {peer} (no listen address yet)");
        } else {
            println!("fetch from another node with:");
            for a in addrs {
                println!("  np2ptp fetch {} --peer {a}/p2p/{peer}", manifest.uri());
            }
        }
        println!("\nProviding on the DHT. Press Ctrl-C to stop.");
        tokio::signal::ctrl_c().await?;
        println!("\nstopped.");
        Ok::<(), Box<dyn Error>>(())
    })
}

/// Download content over the network from a peer, by content id or `.nptp` file.
fn cmd_fetch(args: &[String]) -> Result<(), Box<dyn Error>> {
    let (pos, flags) = parse(args, &["--peer", "--store", "--out"]);
    let target = *pos.first().ok_or("fetch: missing <np2ptp:ROOT | file.nptp>")?;
    let root = match target.strip_prefix("np2ptp:") {
        Some(hex) => Hash::from_hex(hex)?,
        None => Manifest::from_nptp(&fs::read(target)?)?.root,
    };
    let peer_str = flags
        .get("peer")
        .ok_or("fetch: --peer <multiaddr/p2p/PEERID> is required")?;
    let addr: Multiaddr = peer_str.parse()?;
    let peer = peer_id_from_multiaddr(&addr)
        .ok_or("fetch: --peer must include the peer id, e.g. /ip4/.../quic-v1/p2p/<peer-id>")?;
    let store_dir = flags.get("store").map(String::as_str).unwrap_or(DEFAULT_STORE).to_string();
    let out_flag = flags.get("out").cloned();
    let use_fec = flags.contains_key("fec");

    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        let net = Network::spawn(Store::open(&store_dir)?, None)?;
        let into = Store::open(&store_dir)?;
        net.add_peer(peer, addr.clone()).await?;
        net.dial(addr).await?;

        // Retry while the QUIC connection establishes; already-stored chunks are
        // skipped on retry, so this resumes rather than restarts.
        let mut manifest = None;
        let mut last_err = None;
        for _ in 0..100 {
            let attempt = if use_fec {
                net.download_fec(root, peer, &into).await
            } else {
                net.download(root, peer, &into).await
            };
            match attempt {
                Ok(m) => {
                    manifest = Some(m);
                    break;
                }
                Err(e) => {
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
        let manifest = manifest.ok_or_else(|| {
            format!(
                "download failed: {}",
                last_err.map(|e| e.to_string()).unwrap_or_default()
            )
        })?;

        let dest = write_output(&into, &manifest, out_flag)?;
        println!("fetched {} ({} bytes) -> {dest}", manifest.uri(), manifest.total_size);
        Ok::<(), Box<dyn Error>>(())
    })
}

fn print_usage() {
    eprintln!(
        "np2ptp — New Peer-To-Peer Transfer Protocol (prototype)\n\n\
         USAGE:\n\
         \x20 np2ptp pack  <input> [--out <file.nptp>] [--store <dir>] [--name <name>]\n\
         \x20 np2ptp info  <file.nptp>\n\
         \x20 np2ptp get   <file.nptp> --source <store-dir> [--store <dir>] [--out <output>]\n\
         \x20 np2ptp serve <file.nptp> [--store <dir>] [--listen <multiaddr>]\n\
         \x20 np2ptp fetch <np2ptp:ROOT | file.nptp> --peer <multiaddr/p2p/ID> [--store <dir>] [--out <output>] [--fec]\n\n\
         NOTES:\n\
         \x20 'pack' is the linker: chunks a file/folder into a store and writes a .nptp file.\n\
         \x20 'get' rebuilds content from a local --source store (offline stand-in for a peer).\n\
         \x20 'serve' seeds content over the network (QUIC + DHT); 'fetch' downloads it from a\n\
         \x20 peer over the network, verifying every chunk. Default store dir: {DEFAULT_STORE}"
    );
}
