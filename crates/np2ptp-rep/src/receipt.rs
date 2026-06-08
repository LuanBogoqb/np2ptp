//! Signed receipts: portable, verifiable proof that one peer served bytes to
//! another.
//!
//! When a server uploads `bytes` of data to a client, the **client** signs a
//! receipt crediting the **server**. The server keeps it. Because the receipt is
//! signed by the receiver, the server can later show it to anyone as proof of
//! contribution — the basis for reputation that travels beyond a single pairwise
//! relationship (unlike BitTorrent's tit-for-tat, which forgets everything the
//! moment a connection drops).

use serde::{Deserialize, Serialize};

use crate::identity::{Identity, PeerId};

/// Domain tag so a receipt signature can't be repurposed as some other message.
const RECEIPT_TAG: &[u8] = b"np2ptp-receipt-v1";

/// A receipt: "`client` acknowledges receiving `bytes` from `server` at `epoch`."
/// Signed by `client`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    /// The peer that served the bytes — the one credited.
    pub server: PeerId,
    /// The peer that received the bytes and signs this receipt.
    pub client: PeerId,
    pub bytes: u64,
    /// A caller-supplied monotonic counter / timestamp (keeps receipts distinct
    /// and orderable). NP2PTP does not mint time itself.
    pub epoch: u64,
    /// `client`'s signature over the canonical encoding of the fields above.
    pub sig: Vec<u8>,
}

impl Receipt {
    /// Canonical byte encoding that gets signed/verified.
    fn canonical(server: &PeerId, client: &PeerId, bytes: u64, epoch: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(RECEIPT_TAG.len() + 32 + 32 + 8 + 8);
        v.extend_from_slice(RECEIPT_TAG);
        v.extend_from_slice(server.as_bytes());
        v.extend_from_slice(client.as_bytes());
        v.extend_from_slice(&bytes.to_le_bytes());
        v.extend_from_slice(&epoch.to_le_bytes());
        v
    }

    /// Issue a receipt: called by the **client** (receiver) to credit `server`.
    pub fn issue(client: &Identity, server: PeerId, bytes: u64, epoch: u64) -> Receipt {
        let client_id = client.peer_id();
        let msg = Self::canonical(&server, &client_id, bytes, epoch);
        Receipt {
            server,
            client: client_id,
            bytes,
            epoch,
            sig: client.sign(&msg).to_vec(),
        }
    }

    /// Verify the receipt's signature matches its contents and signer.
    pub fn verify(&self) -> bool {
        let msg = Self::canonical(&self.server, &self.client, self.bytes, self.epoch);
        self.client.verify(&msg, &self.sig)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issued_receipt_verifies() {
        let client = Identity::from_seed([1u8; 32]);
        let server = Identity::from_seed([2u8; 32]).peer_id();
        let r = Receipt::issue(&client, server, 4096, 1);
        assert!(r.verify());
        assert_eq!(r.server, server);
        assert_eq!(r.client, client.peer_id());
    }

    #[test]
    fn tampering_with_bytes_breaks_verification() {
        let client = Identity::from_seed([1u8; 32]);
        let server = Identity::from_seed([2u8; 32]).peer_id();
        let mut r = Receipt::issue(&client, server, 4096, 1);
        r.bytes = 1_000_000; // inflate the credit
        assert!(!r.verify());
    }

    #[test]
    fn swapping_the_server_breaks_verification() {
        let client = Identity::from_seed([1u8; 32]);
        let server = Identity::from_seed([2u8; 32]).peer_id();
        let attacker = Identity::from_seed([3u8; 32]).peer_id();
        let mut r = Receipt::issue(&client, server, 4096, 1);
        r.server = attacker; // try to steal the credit
        assert!(!r.verify());
    }

    #[test]
    fn forged_signature_fails() {
        let client = Identity::from_seed([1u8; 32]).peer_id();
        let server = Identity::from_seed([2u8; 32]).peer_id();
        let r = Receipt {
            server,
            client,
            bytes: 10,
            epoch: 1,
            sig: vec![0u8; 64],
        };
        assert!(!r.verify());
    }

    #[test]
    fn receipt_serializes() {
        let client = Identity::from_seed([1u8; 32]);
        let server = Identity::from_seed([2u8; 32]).peer_id();
        let r = Receipt::issue(&client, server, 4096, 7);
        let bytes = bincode::serialize(&r).unwrap();
        let back: Receipt = bincode::deserialize(&bytes).unwrap();
        assert_eq!(back, r);
        assert!(back.verify());
    }
}
