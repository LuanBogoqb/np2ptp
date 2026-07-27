# Daemon and Embedding — Design

**Date:** 2026-07-27
**Status:** Implemented, shipped in v0.1.9

## Goal

Make NP2PTP embeddable by a long-running application without that application
having to drive one CLI process per operation. Concretely:

1. One process can serve several contents at once.
2. A BitTorrent download can land in a caller-chosen folder and report
   progress while it runs.
3. The relay and tracker endpoints are not hardcoded to one deployment.
4. A persistent process owns the store, the identity and the network, and
   takes commands over a stream instead of being restarted for each one.
5. The binary can update itself, with the same signature pinning the GUI
   already uses.

The first embedder is a game launcher, but nothing in this design is specific
to one. That work lives in its own repository.

## Governing constraint: the 3-step rule

NP2PTP's core usability is the Quick Start: `pack` → `serve` → `fetch`.
**Every patch in this design is additive and opt-in.** No existing command
changes behavior, output, or required arguments. A CI golden test (see
Testing) enforces this byte-for-byte. New capabilities are extras that a CLI
user may never encounter.

## The patches, in order

### Patch 1: `serve` multi-manifest

- `np2ptp serve a.nptp b.nptp …` provides all listed manifests from one node.
- `np2ptp serve --all` provides everything the store knows.
- `np2ptp serve a.nptp` (one arg) behaves exactly as today.
- Implementation: the single `net.provide(&manifest)` call becomes a loop;
  chunk serving already answers from the store regardless — `provide` is only
  the DHT/tracker announcement.
- Ships as a standalone CLI release.

### Patch 2: bridge output dir + progress

- `resolve_or_convert_remote` gains an optional output folder parameter
  (CLI: `np2ptp torrent <input> --out <dir>`). Default: current behavior
  (download under `store.root()/.np2ptp-bridge-downloads/<key>`).
- Replace the silent `wait_until_completed()` with a poll loop over librqbit's
  `handle.stats()`, emitting the same NDJSON progress shape the other ops use
  (`bytes_done`, `bytes_total`, plus peers/speed fields when available).
- librqbit session state already persists; re-issuing the same input resumes.
  This is the pause/resume mechanism — no new code.

### Patch 3: configurable endpoints

- The hardcoded default relay (`main.rs:32`) and tracker (`tracker.rs:13`)
  become overridable via flags (already partially exist) **and** env vars
  (`NP2PTP_RELAY`, `NP2PTP_TRACKER`), with the current values as defaults.
- Required for any third-party deployment of the daemon; also closes the
  2026-07-26 audit finding.

### Patch 4: `np2ptp daemon`

New subcommand. A persistent process that is the **single owner** of the
store, the identity key, and the network stack. This resolves by construction
the concurrency hazard of multiple CLI processes sharing one store dir and
one `identity.key` (same peer id from multiple nodes).

**Transport:** NDJSON over stdin/stdout. No HTTP, no ports, no auth surface —
only the parent process can talk to it.

**Protocol:** requests carry a client-chosen `id`; every event for that
operation echoes the `id`. Operations run as independent tokio tasks, so
commands interleave freely.

| Command | Effect |
|---|---|
| `{"id":N,"cmd":"fetch","uri":…,"out":…}` | Download np2ptp content |
| `{"id":N,"cmd":"convert","torrent":…,"data":…}` | Bridge a downloaded torrent in place (no-copy, piece-hash verified) |
| `{"id":N,"cmd":"convert","path":…}` | Pack an arbitrary file/dir (no-copy, unverified origin) |
| `{"id":N,"cmd":"torrent","input":…,"out":…}` | Download a magnet/.torrent via librqbit, auto-bridged no-copy on completion |
| `{"id":N,"cmd":"provide","nptp":…}` | Start seeding (no restart) |
| `{"id":N,"cmd":"unprovide","root":…}` | Stop seeding |
| `{"id":N,"cmd":"status"}` | Peers, bytes served, provided roots |
| `{"id":N,"cmd":"shutdown"}` | Graceful exit |

Events reuse the existing `--json` shapes (`progress`, `result`, `error`)
plus a `ready` event at startup and `warn` events (e.g. missing no-copy
source file). Implementation is a refactor: extract the `cmd_*` bodies into
functions taking a progress callback (several already do), then the daemon
loop dispatches to them.

### Patch 5: self-update (ported from np2ptp-gui's BinaryManager)

Added 2026-07-27 by Luan's request: bring np2ptp-gui's update system
(integrity verification + automatic update at startup) to the np2ptp binary
itself.

**The ported recipe** (from `np2ptp-gui/src/Np2ptpGui/Services/BinaryManager.cs`):

- Latest GitHub release → download the platform asset
  (`np2ptp-windows-x86_64.exe` / linux equivalent).
- **Integrity:** on Windows, the downloaded exe's Authenticode signer
  thumbprint must equal the pinned release-pipeline cert
  (`36477BB5DCB10D2C0381A2D79533F0386C5CCACA`); mismatch → delete download,
  keep current binary, loud error. On Linux (no Authenticode), verify against
  the release's published `SHA256SUMS` asset instead.
- **Version detection:** binary's own embedded version (`CARGO_PKG_VERSION`)
  vs release tag (minus leading `v`). Unknown/unreadable → treat as "needs
  update".
- **Silent startup check:** bounded timeout; no internet / GitHub down /
  timeout → keep running the binary already on disk, silently.
- **Self-replace on Windows:** a running exe cannot be overwritten — rename
  current exe to `np2ptp.exe.old`, write the new one in place, delete the
  `.old` on next successful start.

**Reconciliation with the 3-step rule** (golden rule 5): auto-update at
startup of `pack`/`serve`/`fetch` would change what a Quick Start user sees
(network calls, latency, messages) — not acceptable. Therefore:

- New `np2ptp update` subcommand — manual, for everyone.
- Automatic silent check **only at `daemon` startup** (new surface, no
  existing UX touched), opt-out via `--no-auto-update`. Emits a
  `{"event":"updated","from":…,"to":…}` NDJSON event when it happens, so the
  embedder can log or surface it.
