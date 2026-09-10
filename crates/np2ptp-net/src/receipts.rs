//! Receipts collected *about this node* — proof, signed by past clients,
//! that this node served them bytes. Presented to a new peer on request
//! (`GetReceipts`) so reputation travels even to peers with no direct
//! history, instead of resetting to zero on every new connection.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

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
    ///
    /// Like the ledger: a corrupt file must not brick the node — rename it
    /// aside and start empty. Receipts are re-earned from live clients; the
    /// old `receipts.bin` stays on disk for forensics.
    pub fn open(path: impl AsRef<Path>) -> Result<ReceiptBag, ReceiptBagError> {
        let path = path.as_ref().to_path_buf();
        let receipts = match fs::read(&path) {
            Ok(bytes) => match bincode::deserialize(&bytes) {
                Ok(receipts) => receipts,
                Err(_) => {
                    quarantine_corrupt(&path);
                    Vec::new()
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(ReceiptBag { receipts, path: Some(path) })
    }

    /// Insert `r` unless it's an exact duplicate of one already held, then
    /// keep only the `MAX_RECEIPTS` highest-`bytes` entries.
    pub fn insert(&mut self, r: Receipt) {
        if self.receipts.contains(&r) {
            return;
        }
        self.receipts.push(r);
        self.receipts.sort_by_key(|r| std::cmp::Reverse(r.bytes));
        self.receipts.truncate(MAX_RECEIPTS);
    }

    pub fn list(&self) -> &[Receipt] {
        &self.receipts
    }

    /// Persist to the bound path (no-op if created via `new`, without one).
    pub fn save(&self) -> Result<(), ReceiptBagError> {
        if let Some(path) = &self.path {
            let tmp = path.with_extension("tmp");
            let bytes = bincode::serialize(&self.receipts)?;
            {
                let mut f = fs::File::create(&tmp)?;
                f.write_all(&bytes)?;
                // Flush before the rename — rename() is name-atomic, not
                // durability; a power cut mid-save must not leave the renamed
                // file with no/garbage content.
                f.sync_data()?;
            }
            fs::rename(&tmp, path)?;
        }
        Ok(())
    }
}

/// Move a corrupt state file aside so the node can boot clean (mirror of the
/// ledger's quarantine; kept local — the two crates don't share this dep).
fn quarantine_corrupt(path: &Path) {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let aside = path.with_file_name(format!(
        "{}.corrupt-{ts}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state")
    ));
    let _ = fs::rename(path, aside);
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
    fn a_corrupt_file_boots_empty_and_is_quarantined() {
        let path = tmp_path();
        fs::write(&path, b"not bincode at all").unwrap();
        let bag = ReceiptBag::open(&path).unwrap();
        assert!(bag.list().is_empty(), "corrupt state must not brick the open");
        let quarantined = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().contains("receipts-"));
        assert!(quarantined, "corrupt file should be renamed aside");
        let _ = fs::remove_file(&path);
        for e in std::fs::read_dir(path.parent().unwrap()).unwrap().flatten() {
            if e.file_name().to_string_lossy().contains("np2ptp-net-receipts") {
                let _ = fs::remove_file(e.path());
            }
        }
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

    #[test]
    fn insert_ignores_an_exact_duplicate() {
        let mut bag = ReceiptBag::new();
        let client = Identity::from_seed([5u8; 32]);
        let server = Identity::from_seed([6u8; 32]).peer_id();
        let r = Receipt::issue(&client, server, 1000, 1);
        bag.insert(r.clone());
        bag.insert(r);
        assert_eq!(bag.list().len(), 1, "submitting the same receipt twice should not double-count it");
    }
}
