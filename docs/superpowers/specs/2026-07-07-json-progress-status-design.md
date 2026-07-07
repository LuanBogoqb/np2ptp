# JSON progress/status output for external launchers

## Problem

`np2ptp` is a CLI/library today; its only output is human-readable text
(`println!`). A launcher written in another language (the motivating case: a
C# game launcher wrapping `np2ptp` as a child process) has no way to show a
progress bar for `pack`/`fetch`/`get`, or to know — while `serve` is running —
how many peers are connected, which tracker is in use, or how many bytes have
been served/received. There is no Rust FFI/binding involved; the launcher only
ever talks to the `np2ptp` executable as a subprocess.

Two things don't exist yet and block this:

1. No machine-readable output mode. All output is `println!` text meant for a
   human terminal.
2. No progress hook at all inside the download/chunking code paths.
   `Network::download`, `np2ptp_node::download` (the offline/local path used
   by `get`), and `Store::ingest_file_streaming_impl` (used by `pack`) all run
   to completion and only return a final result — there is nowhere today to
   observe "36491 of 36491 chunks done" mid-flight.

## Scope

In scope: `pack`, `fetch`, `get`, `serve` all gain an opt-in `--json` flag
that switches their output to newline-delimited JSON (NDJSON) events on
stdout. Existing text output is unchanged and remains the default.

Out of scope (explicitly deferred, per user decision during brainstorming):

- A local HTTP status endpoint for `serve`. The user confirmed the launcher
  always starts and holds the `np2ptp` process itself, so there is no need to
  query status from a process that didn't spawn it — NDJSON on the held
  process's stdout is sufficient and avoids adding a local network listener.
- The BitTorrent-style tracker-measured "ratio" feature (signed receipts
  wired into the network path + tracker-side aggregation). Discussed
  separately; the user chose to do progress/status first. That work is not
  part of this spec.
- A real BitTorrent bridge (`np2ptp-bridge` + librqbit + CLI) — unrelated,
  not part of this spec.

## Design

### Flag and output mode

`--json` is accepted by `pack`, `get`, `fetch`, and `serve`. When present:

- Every event is one line of JSON on **stdout**, always including an `event`
  field so a consumer can dispatch on it, and an `op` field naming the
  subcommand.
- Errors that would otherwise be a `Err(...)` printed to stderr are instead
  emitted as a `{"event":"error", ...}` line **on stdout** (not stderr), so a
  launcher only has to read one stream to get everything.
- Without `--json`, behavior is byte-for-byte what it is today. No existing
  script, doc example, or test relying on the text output changes.

### Event shapes

```jsonc
// pack — progress is BYTES processed, not chunks: streaming content-defined
// chunking (FastCDC) doesn't know the total chunk count in advance, but the
// input file's total size is known upfront.
{"event":"progress","op":"pack","bytes_done":1048576,"bytes_total":5242880}
{"event":"result","op":"pack","root":"np2ptp:e0cf...","chunks_total":70,"chunks_new":2,"bytes_total":5242880}

// fetch / get — total chunk count IS known upfront here (from the manifest,
// fetched/read before any chunk transfer starts).
{"event":"progress","op":"fetch","chunks_done":1230,"chunks_total":36491,"bytes_done":78643200,"bytes_total":2400000000}
{"event":"result","op":"fetch","root":"np2ptp:...","chunks_fetched":36491,"chunks_deduped":0,"bytes_total":2400000000}
{"event":"progress","op":"get","chunks_done":40,"chunks_total":70}
{"event":"result","op":"get","chunks_fetched":68,"chunks_deduped":2}

// serve — periodic status (~2s tick), not progress of something that finishes.
// `tracker` is `null` if `serve` was run with `--no-tracker`.
{"event":"status","op":"serve","peers":3,"tracker":"https://nptp.bogotec.uk","bytes_served":10485760,"bytes_received":0}

// error, any command, --json mode, printed to stdout instead of stderr
{"event":"error","op":"fetch","message":"download failed: request to peer failed"}
```

Progress lines are throttled (emitted at most every ~100ms), not one line per
chunk — a 36,491-chunk transfer must not produce 36,491 lines of stdout.

### Where this plugs into the code

No existing public function signature changes — every new capability is an
additive `..._with_progress` sibling, so nothing written today (including
today's tests) breaks.

- **`np2ptp-store`**: `ingest_file_streaming_impl`/`ingest_tree_files_impl`
  (the private, already-shared implementations behind both the copying and
  `--no-copy` public functions) gain an optional
  `on_progress: Option<&mut dyn FnMut(u64, u64, bool)>` parameter —
  `(bytes_done, bytes_total, chunk_was_new)`. New public wrappers
  `ingest_tree_files_with_progress` / `ingest_tree_files_no_copy_with_progress`
  pass `Some(...)`; the existing public functions keep passing `None`.
- **`np2ptp-node` (lib.rs)**: `download()` (the offline `ChunkSource`-based
  path used by `get`) gains a `download_with_progress` sibling taking
  `on_progress: impl FnMut(usize, usize)` — `(chunks_done, chunks_total)`.
- **`np2ptp-net`**: `Network::download`/`download_fec` gain the same kind of
  `_with_progress` sibling. Two new read-only accessors:
  - `Network::connected_peers() -> Vec<PeerId>` — free, `Swarm::connected_peers()`
    already tracks this.
  - `Ledger::totals(&self) -> Counters` (in `np2ptp-rep`) — sums `served_to_us`/
    `we_served`/`credited_by_receipts` across every peer entry, so `serve` can
    report one aggregate served/received figure instead of per-peer.
- **`np2ptp-node/main.rs`**: parses `--json` (already has a `parse()` helper
  for flags). A small internal enum/helper serializes events via
  `serde_json` (already a dependency — no new crate). `cmd_serve` gets a
  second `tokio::time::interval` (~2s) alongside the existing 120s
  tracker-reannounce interval, ticking `connected_peers().len()` +
  `Ledger::totals()` + the tracker URL already in scope.

### Error handling

- In `--json` mode, the top-level `Result::Err` in each `cmd_*` function is
  caught at the call site (not propagated to the generic "print to stderr and
  exit 1" path `main()` uses today) and printed as one `{"event":"error",...}`
  line to stdout before exiting non-zero. Exit code behavior (non-zero on
  failure) is unchanged — only *where* the message goes changes.
- A malformed/partial line is not a concern this spec needs to handle:
  each `println!` call already writes one flushed line at a time; NDJSON
  consumers (per the standard convention) always process complete lines.

### Testing

- `np2ptp-store`: unit test asserting the progress callback reports
  `chunk_was_new = false` for chunks that already exist in the store (the
  re-pack-after-editing-one-file scenario) and `true` for genuinely new ones;
  assert final `bytes_done == bytes_total`.
- `np2ptp-net`: unit test asserting `download_with_progress`'s callback fires
  with a monotonically increasing `chunks_done` that reaches `chunks_total`;
  a small test asserting `connected_peers()` includes a peer after a real
  connection is established (reuses the existing relay/network test harness
  style).
- `np2ptp-rep`: unit test for `Ledger::totals()` summing multiple peers'
  counters correctly.
- `np2ptp-node`: a CLI-level integration test that runs the built `np2ptp`
  binary (`pack --json` and `get --json`) as a real subprocess against a
  temp file, and asserts every stdout line parses as JSON and the final line
  is a `result` event with the expected `root`/`chunks_total`.
