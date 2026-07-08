# Signed Receipt Exchange — Design

## Problem

`np2ptp-rep` already has everything needed for portable reputation — an
Ed25519 `Identity`/`PeerId`, a signed `Receipt` ("`client` acknowledges
receiving `bytes` from `server`"), and a `Ledger<K>` with `apply_receipt` and
a `credited_by_receipts` counter — but none of it is wired into `np2ptp-net`.
Today:

- `net::Network`'s `Ledger` is keyed by the libp2p `PeerId` (a hash of the
  public key), not `rep::PeerId` (the raw Ed25519 key) — the type
  `apply_receipt` requires. It can't be used as-is.
- The ledger lives only in memory and resets on every restart, even for
  `serve`, which already persists its identity (`{store}/identity.key`) for
  exactly this kind of continuity.
- No `Receipt` is ever issued, signed, or sent over the wire. A server's
  reputation is invisible to any peer it hasn't directly transacted with in
  the current process lifetime — the same amnesia BitTorrent's tit-for-tat
  has, which this crate exists to fix.
- `Ledger::reputation()` computes `served_to_us - we_served` and ignores
  `credited_by_receipts` entirely, so even if receipts were applied, they
  would not affect choke/unchoke decisions.

## Goals

1. After a client finishes downloading from a server, the client signs one
   `Receipt` (aggregating bytes pulled from that server this session) and
   sends it to the server.
2. A server persists received receipts about itself and can present them,
   on request, to a peer it has no direct history with — so a brand-new
   connection can be credited immediately instead of starting from zero.
3. Third-party-credited bytes (`credited_by_receipts`) count toward the real
   reputation score used for choke/unchoke, not just as a side counter.
4. `net::Network`'s ledger survives restarts, mirroring `serve`'s existing
   identity persistence.
5. The effect is measurable: a new `np2ptp-sim` scenario shows a cold peer
   with valid third-party receipts getting served where an equally cold peer
   without them gets choked.

## Non-goals

- No change to `get`/`fetch`'s use of an ephemeral, non-persisted identity.
  A receipt's validity does not depend on the *client*'s identity persisting
  across runs — only the *credited* peer's identity needs continuity, and
  that's `serve`, which already persists one.
- No gossip/relay of receipts beyond one hop (a peer presents only receipts
  *about itself*, never receipts it holds about a third party).
- No change to the existing direct-experience half of `reputation()`
  (`served_to_us - we_served`); it is extended, not replaced.

## Architecture

### Identity

No second keypair. `serve` already loads a 32-byte Ed25519 seed from
`{store}/identity.key` and uses it to build the libp2p `Keypair`
(`identity::Keypair::ed25519_from_bytes`). The same seed builds a
`np2ptp_rep::Identity` (`Identity::from_seed`) for signing outgoing receipts
and verifying/crediting incoming ones — one seed, two views of the same key.

`get`/`fetch` currently pass `None` to `Network::spawn`, which lets libp2p
generate a random keypair internally without exposing the seed. Since the
client now needs a `rep::Identity` to *sign* its outgoing receipt,
`Network::spawn`'s internal handling changes so that when no seed is given,
*it* generates the random 32 bytes (instead of delegating to libp2p's hidden
RNG) and keeps them for the paired `rep::Identity`. Nothing is persisted to
disk for `get`/`fetch` — behaviorally this is unchanged, still a fresh
identity every run.

### Mapping libp2p `PeerId` ↔ `rep::PeerId`

A receipt names a `rep::PeerId` (the raw 32-byte Ed25519 key); the swarm
talks in terms of libp2p `PeerId` (a multihash of that same key). Both
already flow through the existing `identify` behaviour: `identify::Event
::Received { info, .. }` carries `info.public_key`, a libp2p
`identity::PublicKey`. Since every np2ptp node's key is Ed25519,
`public_key.try_into_ed25519()` yields the raw verifying key
(`ed25519::PublicKey::to_bytes() -> [u8; 32]`), which is byte-for-byte a
`rep::PeerId`. `EventLoop` gains a `HashMap<libp2p::PeerId, np2ptp_rep::PeerId>`
populated in the existing `on_event` handler for `identify::Event::Received`
— no new handshake, just reading a field already exchanged.

### Wire protocol

Two variants added to the existing `Request`/`Response` enums (still one
`request_response::cbor` behaviour, no new protocol/stream):

```rust
enum Request {
    // ...existing Manifest / Chunk / Symbol / Symbols...
    SubmitReceipt(np2ptp_rep::Receipt),
    GetReceipts,
}
enum Response {
    // ...existing Manifest / Chunk / Symbol / Symbols...
    ReceiptAck,
    Receipts(Vec<np2ptp_rep::Receipt>),
}
```

- **`SubmitReceipt`**: sent once, by the client, immediately after a
  successful `download`/`download_fec` call — one receipt per (download
  call, provider) pair, `bytes` = total bytes actually pulled from that
  provider this session (chunks already-local don't count; nothing to
  credit for data not transferred). Delivery is best-effort: if it fails,
  the download has already succeeded and is not rolled back or retried.
- **`GetReceipts`**: sent once per newly-identified peer that has no entry
  yet in the local ledger — checked at the same point the libp2p↔rep
  `PeerId` mapping is learned (`identify::Event::Received`). If the ledger
  already has an entry for that peer (any prior direct exchange), skip the
  request — that peer's reputation isn't starting cold, so there's nothing
  to bootstrap.
- Receiving `GetReceipts`: reply with up to 50 receipts from
  `{store}/receipts.bin` (see below).
- Receiving `SubmitReceipt(r)`: verify `r.verify()`; if valid, insert it
  into `{store}/receipts.bin`'s list, then keep only the 50 highest-`bytes`
  entries (so a new receipt lower-valued than all 50 already held is
  effectively discarded), and reply `ReceiptAck`. Invalid receipts are
  dropped silently (still reply `ReceiptAck` — no reason to leak
  verification failure to a peer that may be malicious).
- Receiving `Receipts(list)` (the `GetReceipts` response): verify each
  independently via `Receipt::verify()`; valid ones are folded into the
  local `Ledger` via `apply_receipt`, crediting `receipt.server` (which must
  match the peer being talked to — a receipt about someone else is ignored,
  since presenting third-party receipts about *other* peers is out of
  scope).

### Persistence

Two new files alongside the existing `{store}/identity.key`, both only for
`serve` (the long-running, persistently-identified role):

- `{store}/ledger.bin` — `net::Network`'s ledger, re-typed from
  `Ledger<libp2p::PeerId>` to `Ledger<np2ptp_rep::PeerId>`, opened via the
  existing `Ledger::open`/`save` (already implemented, unused until now).
  Saved after every mutation that matters for continuity (receipt applied,
  or on a natural checkpoint — exact triggers are a plan-level detail, not
  a design constraint) so a restart doesn't lose accumulated reputation.
- `{store}/receipts.bin` — a `Vec<Receipt>` of receipts collected *about
  this node*, capped at 50, used to answer `GetReceipts`.

`get`/`fetch` do not open either file (no persisted store dir identity to
key them by, and no long-lived reputation to protect).

### Reputation scoring

```rust
pub fn reputation(&self, peer: &K) -> i64 {
    let c = self.counters(peer);
    c.served_to_us as i64 + c.credited_by_receipts as i64 - c.we_served as i64
}
```

This is the whole point of the feature: a peer presenting valid third-party
receipts is credited exactly as if it had served us those bytes directly,
so it can clear a choke threshold on first contact.

### Measurement

A new `np2ptp-sim` scenario: a server with a choke threshold tight enough to
refuse an unknown peer. Two cold peers connect — one carrying a bag of
valid receipts signed by third parties (simulating prior contribution
elsewhere in the network), one with nothing. Assert the first is served and
the second is choked, demonstrating that circulated receipts — not just
direct history — change a real outcome.

## Trust model / limitations

A receipt proves only that some Ed25519 key signed "I received `bytes` from
`server`" — it does not prove the signer is a distinct, real peer. An
operator can run one target node plus disposable downloader nodes (even
using the stock binary, no protocol violation), have the throwaways
download real content from the target to generate genuinely valid
large-`bytes` receipts, then present that bag to bootstrap trust with any
new peer. This is a known ceiling of signed-receipt reputation without
Sybil resistance, stake, or proof-of-unique-identity — acceptable for a
research prototype measuring whether *portable* reputation changes
outcomes at all, but not a claim that this reputation is Sybil-resistant.
`SubmitReceipt`'s handler rejects the degenerate case of a peer vouching
for itself (`receipt.client == receipt.server`), which blocks the laziest
form of self-dealing but not multi-identity Sybil attacks.

## Testing

- `np2ptp-rep::Ledger`: update the `reputation` formula's existing tests
  and add one asserting `credited_by_receipts` moves the score.
- `np2ptp-net`: unit-level coverage for the libp2p↔rep `PeerId` mapping, the
  `SubmitReceipt`/`GetReceipts` request/response round trip (including a
  tampered receipt being rejected), the receipt cap/eviction at 50, and the
  choke-then-unchoke-via-receipt behavior — following the existing
  two-node integration test pattern in `crates/np2ptp-net/tests/two_nodes.rs`.
- `np2ptp-sim`: the new scenario described above, writing into the existing
  `reports/` output alongside the other scenarios.

## Global constraints for the plan

- No new libp2p protocol/stream — reuse the existing
  `request_response::cbor::Behaviour<Request, Response>`.
- No second keypair — `rep::Identity` is always derived from the same seed
  already used for the libp2p `Keypair`.
- `get`/`fetch` keep using a fresh, non-persisted identity every run.
- Receipt cap is 50, keeping the highest-`bytes` entries.
- `GetReceipts` fires at most once per peer per process lifetime (gated on
  "no existing ledger entry for this peer" at the moment the libp2p↔rep
  `PeerId` mapping is learned via `identify`).
- `reputation()` becomes `served_to_us + credited_by_receipts - we_served`
  for every `Ledger<K>`, not just the net-facing one.
