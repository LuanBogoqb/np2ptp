# Hydra Launcher × NP2PTP Integration — Design

**Date:** 2026-07-27
**Status:** Approved by Luan (this session), pending spec review
**Repos involved:** `np2ptp` (this repo, patches land on `dev`) and a to-be-created fork of `hydralauncher/hydra` (all work on `dev`)

## Goal

A Hydra Launcher fork that can:

1. Download content published as `np2ptp:` links (new downloader).
2. Convert already-downloaded games (torrent or direct download) into NP2PTP,
   in place, without duplicating data on disk.
3. Seed converted games to the NP2PTP network via a per-game toggle.
4. Optionally use NP2PTP's librqbit-based engine for *BitTorrent* downloads,
   replacing Hydra's libtorrent 2.x sidecar (which leaks memory under I/O
   pressure via its mmap write path on slow disks).

## Governing constraint: the 3-step rule

NP2PTP's core usability is the Quick Start: `pack` → `serve` → `fetch`.
**Every patch in this design is additive and opt-in.** No existing command
changes behavior, output, or required arguments. A CI golden test (see
Testing) enforces this byte-for-byte. New capabilities are extras that a CLI
user may never encounter.

## Part 1 — NP2PTP patches (this repo, in order)

### Patch 1: `serve` multi-manifest

- `np2ptp serve a.nptp b.nptp …` provides all listed manifests from one node.
- `np2ptp serve --all` provides everything the store knows.
- `np2ptp serve a.nptp` (one arg) behaves exactly as today.
- Implementation: the single `net.provide(&manifest)` call becomes a loop;
  chunk serving already answers from the store regardless — `provide` is only
  the DHT/tracker announcement.
- Ships as a standalone CLI release; useful without Hydra.

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
  `{"event":"updated","from":…,"to":…}` NDJSON event when it happens, so
  Hydra can log/show it.

## Part 2 — Hydra fork

### Sidecar: `Np2ptpDaemonManager` (Electron main process)

Mirrors Hydra's existing Python/libtorrent sidecar pattern:

- Spawns the bundled `np2ptp.exe daemon` at app boot.
- Routes NDJSON events via an `id → callback` map.
- On crash: exponential backoff, 3 attempts, then a visible warning.
- On restart: re-issues `provide` for every game whose seed toggle is ON.
  **Desired state lives in Hydra's local DB; the daemon only executes.**
- `np2ptp.exe` ships in the Electron build resources.

### Downloader

- `Np2ptp` added to the shared `Downloader` enum.
- URI classification recognizes the `np2ptp:` scheme and routes to the daemon
  (`cmd fetch`). Community sources need no structural change — it's one more
  URI type in their JSON lists.
- Progress events map onto Hydra's download model; speed derived in TS from
  event deltas.

### Torrent engine toggle

- Global setting **"Use NP2PTP as torrent engine"** (default OFF).
- ON: magnets/.torrents route to the daemon (`cmd torrent`) instead of the
  Python libtorrent sidecar. Files land in the normal game folder (`--out`).
- Side effect: such downloads arrive already bridged — the game is instantly
  convertible-free and seed-togglable.
- Documented trade-off: after completion, seeding happens on the NP2PTP side
  only; there is no ongoing BitTorrent seeding for these downloads (librqbit
  seeds only while its session runs the download).
- Bonus inherited from the bridge: before hitting the BitTorrent swarm,
  `resolve_or_convert` checks whether the content is already available on the
  NP2PTP network and prefers it.

### Conversion UX

- Button "Convert to NP2PTP" on a downloaded game's page:
  - Torrent-sourced → `cmd convert` with `.torrent` + data dir (verified, no-copy).
  - Direct download → `cmd convert` pack path (no-copy; UI states once that
    origin is unverified — "you are publishing what is on disk, as is").
- Result (`np2ptp:` URI + `.nptp` path) persisted on the download record;
  copy-link button in the UI.
- Global setting "Convert automatically after download" (default OFF) —
  enqueues the same `cmd convert` on download completion.

### Seeding UX

- Per-game toggle in the same UI region as Hydra's torrent seeding.
  ON → `cmd provide`; OFF → `cmd unprovide`; state persisted in Hydra's DB.
- Simple status view ("seeding N games, X GB served") fed by `cmd status`.

## Data flow & error handling

**Layout.** NP2PTP store (fetched chunks, `refs.tsv`, `identity.key`) lives in
Hydra's appData. Game data stays where it is; no-copy stores absolute-path
references, cross-volume is fine. Nothing is duplicated.

**No-copy invariant** (files must stay in place, unchanged):

1. Uninstall via Hydra → fork `unprovide`s and clears refs *before* deleting.
2. Moved/deleted outside Hydra → daemon fails the chunk read, answers "chunk
   unavailable" to the peer (never crashes), emits `warn`; Hydra marks the
   game "conversion broken — reconvert".
3. Game update replaces files → same handling, but Hydra knows it caused it
   and marks the conversion stale at update time, without waiting for a failure.

**Conversion failures.** Piece-hash mismatch → clear UI error, no partial
`.nptp` left behind.

**Daemon death.** Backoff restart ×3; in-flight fetch/convert operations fail
with a toast + retry button — no automatic retry of heavy operations.

**Fetch with no providers.** Configurable timeout, honest message ("nobody is
seeding this content right now") — same UX slot as Hydra's stalled-torrent state.

## Testing

**NP2PTP (Rust, TDD):**

- **3-step golden invariant:** CI test running the README's `pack`/`serve`/
  `fetch` flow, comparing output byte-for-byte against golden files. Guards
  the governing constraint.
- Multi-manifest: new `np2ptp-sim` scenario (one serve, two manifests, both
  fetchable by another node) added to the CI scenario assertions.
- Daemon: integration test spawning the real binary, speaking NDJSON over
  stdio — fetch fixture, convert fixture, provide/unprovide, interleaved ids.
  Golden tests for event shapes.
- Bridge: unit tests for the `--out` parameter and progress events, using the
  crate's existing fake-torrent infra.

**Hydra fork (TypeScript):**

- Unit: NDJSON parser/router (id correlation under interleaving), daemon
  manager restart logic with a fake child process (dies → backoff →
  re-provide from DB state).
- E2E: manual smoke — small np2ptp download, convert an existing torrent,
  enable seed, fetch from a second peer (VPS or Pi) to close the loop.

## Process

- All work on `dev` in both repos; `main` only on release.
- TDD throughout; subagent-driven development with the cavecrew agents
  (model tiering per Luan's dispatch rule).
- Implementation order: Patch 1 → 2 → 3 → 4 → Hydra fork (clone + map first).
