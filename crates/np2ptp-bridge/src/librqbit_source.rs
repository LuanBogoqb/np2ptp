//! Downloads a torrent/magnet you don't already have, using a real
//! BitTorrent client (`librqbit`), then hands the result to the same
//! streaming path used for an already-downloaded torrent
//! ([`crate::resolve_or_convert_local`]) — so a real (tens-of-GB) torrent is
//! never held in memory, and the content is re-verified against the
//! torrent's own piece hashes before being trusted into NP2PTP, exactly like
//! the local-conversion path.
//!
//! `librqbit` resolves a magnet's metadata (BEP 9) before `add_torrent`
//! returns, and always exposes it as a full reconstructed `.torrent` byte
//! blob (`TorrentMetadata::torrent_bytes`) — even for a magnet input — so
//! this module never needs to hand-map `librqbit`'s own metainfo types: it
//! feeds those bytes straight into the crate's own tested
//! [`crate::parse_torrent_file`], keeping one single `TorrentMeta`
//! representation for every source.

use librqbit::{AddTorrent, AddTorrentOptions, Session};
use np2ptp_net::Network;
use np2ptp_store::Store;
use sha1::{Digest, Sha1};

use crate::{parse_torrent_file, resolve_or_convert_local, BridgeError, Outcome};

fn source_err(e: impl std::fmt::Display) -> BridgeError {
    BridgeError::Source(e.to_string())
}

/// Fetch `input` (a magnet link, a path to a `.torrent` file, or an
/// `http(s)://` URL to one — see `librqbit::AddTorrent::from_cli_argument`)
/// over BitTorrent, then convert/bridge it into NP2PTP.
///
/// The download lands under `store.root()/.np2ptp-bridge-downloads/<key>`,
/// keyed by a hash of `input` so retrying the same magnet/torrent resumes
/// instead of starting over. `librqbit`'s own DHT/session state lives
/// alongside it in `.np2ptp-bridge-librqbit-session`.
pub async fn resolve_or_convert_remote(
    net: &Network,
    store: &Store,
    input: &str,
    no_copy: bool,
) -> Result<Outcome, BridgeError> {
    let key = hex::encode(Sha1::digest(input.as_bytes()));
    let download_dir = store.root().join(".np2ptp-bridge-downloads").join(&key);
    let session_dir = store.root().join(".np2ptp-bridge-librqbit-session");
    std::fs::create_dir_all(&download_dir).map_err(|e| source_err(format!("creating download dir: {e}")))?;
    std::fs::create_dir_all(&session_dir).map_err(|e| source_err(format!("creating session dir: {e}")))?;

    let session = Session::new(session_dir).await.map_err(source_err)?;

    let add = AddTorrent::from_cli_argument(input).map_err(source_err)?;
    let opts = AddTorrentOptions {
        output_folder: Some(download_dir.display().to_string()),
        ..Default::default()
    };
    let handle = session
        .add_torrent(add, Some(opts))
        .await
        .map_err(source_err)?
        .into_handle()
        .ok_or_else(|| BridgeError::Source("torrent add returned no handle (list-only?)".into()))?;

    handle.wait_until_completed().await.map_err(source_err)?;

    let torrent_bytes = handle.with_metadata(|m| m.torrent_bytes.clone()).map_err(source_err)?;
    let meta = parse_torrent_file(&torrent_bytes)?;

    resolve_or_convert_local(net, store, &meta, &download_dir, no_copy).await
}
