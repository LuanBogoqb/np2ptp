//! Exercises a real BitTorrent download through `librqbit` end to end: a
//! seeder session serves content it already has on disk, a separate
//! downloader session (mirroring what
//! [`np2ptp_bridge::resolve_or_convert_remote`] does internally) fetches it
//! over the real peer-wire protocol, and the result is fed through the same
//! `parse_torrent_file` + `convert_local` path the local-conversion tests use.
//!
//! Peer discovery is via a directly-injected `initial_peers` address, not
//! DHT/trackers — this keeps the test fully local, deterministic, and fast
//! (no dependency on live internet infrastructure or swarm health).
#![cfg(feature = "librqbit")]

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use librqbit::{AddTorrent, AddTorrentOptions, Session, SessionOptions};
use np2ptp_bridge::{convert_local, parse_torrent_file};
use np2ptp_store::Store;

struct TmpDir(std::path::PathBuf);
impl TmpDir {
    fn new() -> TmpDir {
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-librqbit-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_download_matches_local_convert() {
    // 1. Seed data on disk, and a .torrent describing it.
    let seed_data = TmpDir::new();
    let payload = b"np2ptp remote bridge test payload, downloaded over real BitTorrent wire protocol";
    std::fs::write(seed_data.path().join("hello.bin"), payload).unwrap();

    let created = librqbit::create_torrent(seed_data.path(), Default::default()).await.unwrap();
    let torrent_bytes = created.as_bytes().unwrap();

    // 2. Seeder: already has the data, listens for peers, no DHT/trackers.
    let seeder_session_dir = TmpDir::new();
    let seeder = Session::new_with_opts(
        seeder_session_dir.path().to_path_buf(),
        SessionOptions { disable_dht: true, listen_port_range: Some(19_500..19_600), ..Default::default() },
    )
    .await
    .unwrap();
    seeder
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes.clone()),
            Some(AddTorrentOptions {
                output_folder: Some(seed_data.path().display().to_string()),
                disable_trackers: true,
                // The data is already on disk at this exact path (that's the
                // whole point of seeding it) — without this, librqbit refuses
                // to write over the pre-existing file instead of verifying
                // and adopting it.
                overwrite: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
    let seed_port = seeder.tcp_listen_port().expect("seeder must be listening");
    let seed_peer: SocketAddr = format!("127.0.0.1:{seed_port}").parse().unwrap();

    // 3. Downloader: separate session, pointed directly at the seeder (no
    // DHT/tracker discovery needed), same shape as
    // `resolve_or_convert_remote`'s internals.
    let downloader_session_dir = TmpDir::new();
    let downloader = Session::new_with_opts(
        downloader_session_dir.path().to_path_buf(),
        SessionOptions { disable_dht: true, ..Default::default() },
    )
    .await
    .unwrap();
    let download_dir = TmpDir::new();
    let handle = downloader
        .add_torrent(
            AddTorrent::from_bytes(torrent_bytes),
            Some(AddTorrentOptions {
                output_folder: Some(download_dir.path().display().to_string()),
                disable_trackers: true,
                initial_peers: Some(vec![seed_peer]),
                ..Default::default()
            }),
        )
        .await
        .unwrap()
        .into_handle()
        .expect("add_torrent must return a handle for a non-list-only add");

    handle.wait_until_completed().await.unwrap();

    // 4. Same reconstruction `resolve_or_convert_remote` does: torrent bytes
    // from the resolved metadata, through the crate's own bencode parser.
    let torrent_bytes = handle.with_metadata(|m| m.torrent_bytes.clone()).unwrap();
    let meta = parse_torrent_file(&torrent_bytes).unwrap();

    assert_eq!(std::fs::read(download_dir.path().join("hello.bin")).unwrap(), payload, "downloaded bytes must match the seed exactly");

    // 5. Feed it through the same streaming convert path a local torrent uses.
    let store_dir = TmpDir::new();
    let store = Store::open(store_dir.path()).unwrap();
    let manifest = convert_local(&store, &meta, download_dir.path(), false).unwrap();

    let rebuilt = store.export_tree(&manifest).unwrap();
    assert_eq!(rebuilt, vec![("hello.bin".to_string(), payload.to_vec())]);
}
