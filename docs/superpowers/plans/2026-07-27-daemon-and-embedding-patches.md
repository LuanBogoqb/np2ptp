# Daemon and Embedding — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land the five patches from the approved spec (`docs/superpowers/specs/2026-07-27-daemon-and-embedding-design.md`): configurable endpoints, multi-manifest serve, bridge output dir + progress, the NDJSON stdio daemon, and self-update — all additive/opt-in.

**Architecture:** All work in this repo on `dev`. CLI logic lives in `crates/np2ptp-node` (binary `main.rs` + library `lib.rs`); the daemon becomes a new module there. Bridge changes live in `crates/np2ptp-bridge`. One small addition to `crates/np2ptp-net` (unprovide). Testable logic goes in library functions; `main.rs` stays thin wiring.

**Tech Stack:** Rust (MSVC toolchain on this machine), tokio, libp2p 0.55 (pinned), librqbit 8.1.1 (optional feature), serde_json.


## Global Constraints

- **Golden rule 5 (3-step UX is sacred):** `pack`/`serve <one file>`/`fetch` keep behavior, output, and required args byte-for-byte. Everything here is additive and opt-in.
- `cargo test --workspace` green and `cargo clippy --workspace --all-targets` at 0 warnings before every commit.
- Fresh shells may lack cargo: prefix with `$env:Path = "$env:USERPROFILE\.cargo\bin;$env:Path"` (PowerShell).
- Tests use the repo's self-cleaning `TmpDir` pattern (no `tempfile` dep). Errors via `thiserror` per crate.
- Commits on `dev`, message body via `git commit -F <file>` (PowerShell mangles `-m` quotes), ending with the `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` trailer.
- Streaming rule: never load whole content into memory (golden rule 3).
- Suggested cavecrew dispatch tier is noted per task (Luan's rule: quick patch → Haiku, simple/well-specified → Sonnet, heavy/security/full-test → Opus). Builder agents have no Bash — the orchestrator runs `cargo test`/`clippy` itself after each diff.

## File Structure

- `crates/np2ptp-node/src/main.rs` — modify: env-default helpers, serve multi wiring, `--out` on torrent, `daemon` + `update` subcommand registration, help text.
- `crates/np2ptp-node/src/tracker.rs` — modify: `default_tracker()`.
- `crates/np2ptp-node/src/lib.rs` — modify: export `collect_serve_manifests`.
- `crates/np2ptp-node/src/serve_set.rs` — create: manifest collection + store-registry helpers (unit-testable core of multi-serve).
- `crates/np2ptp-node/src/daemon/mod.rs` — create: daemon runtime loop.
- `crates/np2ptp-node/src/daemon/proto.rs` — create: request/event types (serde).
- `crates/np2ptp-node/src/update.rs` — create: self-update (release check, verify, swap).
- `crates/np2ptp-net/src/lib.rs` — modify: `Network::unprovide`.
- `crates/np2ptp-bridge/src/librqbit_source.rs` — modify: output dir param + stats polling.
- `crates/np2ptp-node/tests/daemon_stdio.rs` — create: spawn-the-binary NDJSON integration test.
- `crates/np2ptp-node/tests/golden_quickstart.rs` — create: 3-step invariant test.
- `crates/np2ptp-sim` — modify: multi-manifest scenario.
- `README.md` — modify (folded into Tasks 2, 7, 10): `serve` multi note, daemon section, update section.

---

### Task 1: Env-overridable endpoints (tier: Haiku)

**Files:**
- Modify: `crates/np2ptp-node/src/main.rs` (const `DEFAULT_RELAY` at :32, usage sites — grep `DEFAULT_RELAY`)
- Modify: `crates/np2ptp-node/src/tracker.rs` (const `DEFAULT_TRACKER` at :13, usage sites — grep `DEFAULT_TRACKER`)

**Interfaces:**
- Produces: `fn default_relay() -> String` (main.rs, private), `pub fn default_tracker() -> String` (tracker.rs). All current `DEFAULT_RELAY.to_string()` / `DEFAULT_TRACKER.to_string()` call sites switch to these.

- [ ] **Step 1: Write the failing test** (in `tracker.rs` `#[cfg(test)] mod tests`; single test covering both cases sequentially — env vars are process-global, don't split into parallel tests)

```rust
#[test]
fn default_tracker_env_override() {
    // Sequential in one test: cargo runs tests in threads sharing the env.
    std::env::remove_var("NP2PTP_TRACKER");
    assert_eq!(default_tracker(), DEFAULT_TRACKER);
    std::env::set_var("NP2PTP_TRACKER", "https://example.test");
    assert_eq!(default_tracker(), "https://example.test");
    std::env::remove_var("NP2PTP_TRACKER");
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test -p np2ptp-node default_tracker_env_override` → FAIL: `default_tracker` not found.
- [ ] **Step 3: Implement**

```rust
// tracker.rs
/// `NP2PTP_TRACKER` overrides the built-in default — additive, for embedders
/// (e.g. an embedded daemon); absent, behavior is identical to before.
pub fn default_tracker() -> String {
    std::env::var("NP2PTP_TRACKER").unwrap_or_else(|_| DEFAULT_TRACKER.to_string())
}
```

Same shape in `main.rs` for `fn default_relay()` reading `NP2PTP_RELAY`. Replace every `DEFAULT_RELAY.to_string()` / `tracker::DEFAULT_TRACKER.to_string()` default-fallback site with the helpers (grep to find all; the consts stay, now used only inside the helpers and docs).

- [ ] **Step 4: Run** `cargo test -p np2ptp-node` + `cargo clippy --workspace --all-targets` → PASS / 0 warnings.
- [ ] **Step 5: Commit** — `feat: NP2PTP_RELAY / NP2PTP_TRACKER env overrides for default endpoints`

---

### Task 2: `serve` multi-manifest + `--all` (tier: Sonnet)

**Files:**
- Create: `crates/np2ptp-node/src/serve_set.rs`
- Modify: `crates/np2ptp-node/src/lib.rs` (add `pub mod serve_set;` and re-export), `crates/np2ptp-node/src/main.rs` (`cmd_serve`), `README.md` (one line under serve usage)

**Interfaces:**
- Produces: `pub fn collect_serve_manifests(paths: &[&str], all: bool, store_dir: &Path) -> Result<Vec<Manifest>, NodeError>` and `pub fn register_manifest(store_dir: &Path, manifest: &Manifest) -> Result<(), NodeError>` (writes `<store>/manifests/<root-hex>.nptp`; idempotent). Task 7's daemon uses both.
- Registry convention: `<store>/manifests/*.nptp` is the set `--all` serves. `serve <file>` also registers its manifest there (opaque store-internal file; no UX change).

- [ ] **Step 1: Failing tests** (`serve_set.rs` unit tests, `TmpDir` pattern copied from `crates/np2ptp-node/tests/integration.rs:11-30`)

```rust
#[test]
fn collect_explicit_paths_loads_all() {
    let dir = TmpDir::new();
    let store = Store::open(dir.path()).unwrap();
    let m1 = np2ptp_node::pack(b"one".repeat(5000).as_slice(), Some("a".into()), &store).unwrap();
    let m2 = np2ptp_node::pack(b"two".repeat(5000).as_slice(), Some("b".into()), &store).unwrap();
    let p1 = dir.path().join("a.nptp"); std::fs::write(&p1, m1.to_nptp().unwrap()).unwrap();
    let p2 = dir.path().join("b.nptp"); std::fs::write(&p2, m2.to_nptp().unwrap()).unwrap();
    let got = collect_serve_manifests(&[p1.to_str().unwrap(), p2.to_str().unwrap()], false, dir.path()).unwrap();
    assert_eq!(got.len(), 2);
}

#[test]
fn register_then_collect_all() {
    let dir = TmpDir::new();
    let store = Store::open(dir.path()).unwrap();
    let m = np2ptp_node::pack(b"x".repeat(5000).as_slice(), Some("a".into()), &store).unwrap();
    register_manifest(dir.path(), &m).unwrap();
    register_manifest(dir.path(), &m).unwrap(); // idempotent
    let got = collect_serve_manifests(&[], true, dir.path()).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].root(), m.root());
}

#[test]
fn all_with_empty_registry_errors() {
    let dir = TmpDir::new();
    assert!(collect_serve_manifests(&[], true, dir.path()).is_err());
}
```

(Adjust `pack` call signature to the real `np2ptp_node::pack` — see `lib.rs`; if `to_nptp`/`root` names differ, match `np2ptp_core::Manifest`.)

- [ ] **Step 2: Run to fail** — `cargo test -p np2ptp-node serve_set` → FAIL (module missing).
- [ ] **Step 3: Implement `serve_set.rs`** — `collect_serve_manifests`: explicit paths → read+parse each; `all` → glob `<store>/manifests/*.nptp` (via `fs::read_dir`, filter extension), error if both empty or if `all` finds nothing ("serve --all: no registered manifests in <store>/manifests"). `register_manifest`: create dir, write `<root-hex>.nptp` if absent.
- [ ] **Step 4: Run tests** → PASS. Clippy 0.
- [ ] **Step 5: Wire `cmd_serve`** (`main.rs`): replace the single `pos.first()` manifest load with `collect_serve_manifests(&pos, flags.contains_key("all"), Path::new(&store_dir))`, loop `net.provide(&m)` + `register_manifest` per manifest, print the existing `serving {uri} ({files} files, {chunks} chunks)` line **per manifest** (single-arg output stays byte-identical). Tracker announce loop: announce every collected root (follow the existing per-root announce code in `cmd_serve`).
- [ ] **Step 6: Manual smoke** — `cargo run -p np2ptp-node -- pack <small file>` then `serve x.nptp` (output unchanged) — compare against a pre-change capture.
- [ ] **Step 7: README** — add one line to the serve usage block: `np2ptp serve a.nptp b.nptp …` / `np2ptp serve --all`.
- [ ] **Step 8: Commit** — `feat: serve accepts multiple manifests and --all (store manifest registry)`

---

### Task 3: Bridge output dir — `torrent --out` (tier: Haiku)

**Files:**
- Modify: `crates/np2ptp-bridge/src/librqbit_source.rs` (`resolve_or_convert_remote`, :36), `crates/np2ptp-node/src/main.rs` (`cmd_torrent` + `fetch_remote_torrent`, ~:820-900)

**Interfaces:**
- Produces: `resolve_or_convert_remote(net, store, input, no_copy, out_dir: Option<&Path>)` — `None` keeps today's `store.root()/.np2ptp-bridge-downloads/<key>`; `Some(dir)` downloads directly into `dir`.

- [ ] **Step 1: Failing test** — the dir choice is the testable unit; extract it:

```rust
// librqbit_source.rs
pub(crate) fn bridge_download_dir(store_root: &Path, input: &str, out_dir: Option<&Path>) -> PathBuf {
    match out_dir {
        Some(d) => d.to_path_buf(),
        None => store_root.join(".np2ptp-bridge-downloads").join(hex::encode(Sha1::digest(input.as_bytes()))),
    }
}

#[test]
fn out_dir_overrides_default_location() {
    let def = bridge_download_dir(Path::new("/s"), "magnet:x", None);
    assert!(def.starts_with("/s"));
    let custom = bridge_download_dir(Path::new("/s"), "magnet:x", Some(Path::new("/games/x")));
    assert_eq!(custom, Path::new("/games/x"));
}
```

- [ ] **Step 2: Fail** — `cargo test -p np2ptp-bridge --features librqbit out_dir` (first run does `cargo fetch` for librqbit; expect FAIL on missing fn).
- [ ] **Step 3: Implement** — add the param to `resolve_or_convert_remote`, use `bridge_download_dir`. In `main.rs`, add `--out` to `cmd_torrent`'s `parse(args, &[…, "--out"])` and thread `flags.get("out")` through `fetch_remote_torrent`.
- [ ] **Step 4: Pass** — same test command + `cargo clippy --workspace --all-targets --features librqbit`.
- [ ] **Step 5: Commit** — `feat: torrent --out downloads into a caller-chosen directory`

---

### Task 4: Bridge progress events (tier: Sonnet)

**Files:**
- Modify: `crates/np2ptp-bridge/src/librqbit_source.rs` (replace `wait_until_completed`, :63), `crates/np2ptp-node/src/main.rs` (`cmd_torrent` NDJSON emission)

**Interfaces:**
- Produces: `resolve_or_convert_remote(net, store, input, no_copy, out_dir, on_progress: &mut (dyn FnMut(u64, u64) + Send))` — called with `(bytes_done, bytes_total)` at most every ~250 ms during the BitTorrent download phase. `cmd_torrent --json` emits `{"event":"progress","op":"torrent","bytes_done":…,"bytes_total":…}` (same shape family as pack/get at `main.rs:131/240`).

- [ ] **Step 1: Confirm the stats API** — librqbit source is not yet extracted locally. Run `cargo fetch` in the workspace, then read `~/.cargo/registry/src/index.crates.io-*/librqbit-8.1.1/src/torrent_state/stats.rs` (or grep `pub struct TorrentStats`). Expected fields: `progress_bytes: u64`, `total_bytes: u64`, `finished: bool` on `handle.stats()`. If names differ, use the real ones everywhere below.
- [ ] **Step 2: Implement the poll loop** (replacing `handle.wait_until_completed().await`):

```rust
loop {
    let s = handle.stats();
    on_progress(s.progress_bytes, s.total_bytes);
    if s.finished { break; }
    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
}
```

Keep a final `handle.wait_until_completed().await.map_err(source_err)?` after the loop iff the API requires it for error surfacing (check its docs in the fetched source; if `stats().error` exists, surface that instead).

- [ ] **Step 3: Test** — unit-testing needs a live torrent session, so the automated gate is: `cargo test -p np2ptp-bridge --features librqbit` (existing tests still green, new signature compiles everywhere) + `cargo clippy --features librqbit` 0 warnings. Behavior verification lands in Task 8's daemon integration test (progress events observed on a real local transfer) — note that forward dependency in the commit body.
- [ ] **Step 4: Wire `cmd_torrent`** — build `on_progress` closure emitting the NDJSON line (throttle: only if ≥100 ms since last emit or done==total, same pattern as `cmd_pack` `main.rs:118-131`); non-json mode prints nothing new (rule 5).
- [ ] **Step 5: Commit** — `feat: bridge download progress callback + torrent --json progress events`

---

### Task 5: `Network::unprovide` (tier: Sonnet)

**Files:**
- Modify: `crates/np2ptp-net/src/lib.rs` (Network impl, command enum, swarm loop)

**Interfaces:**
- Produces: `pub async fn unprovide(&self, root: Hash) -> Result<(), NetError>` — stops the kad provider announcement for `root` (`kad::Behaviour::stop_providing(&key)` with the same key derivation `provide` uses at `lib.rs:344`). Local chunk serving is NOT gated by this (a peer who already knows us can still fetch; that's fine — the daemon also cancels its tracker announce loop, so discovery stops).

- [ ] **Step 1: Read `provide`** (`np2ptp-net/src/lib.rs:344`) — copy its command-channel pattern (each `pub async fn` sends a command into the swarm task; find the `enum` of commands and its handler match).
- [ ] **Step 2: Failing test** — in np2ptp-net's existing test module style (two in-process nodes):

```rust
#[tokio::test]
async fn unprovide_stops_discovery() {
    // node A: provide manifest, node B: find_providers -> contains A.
    // A: unprovide(root). B: find_providers again -> empty (allow kad
    // propagation: retry up to ~5s before asserting).
}
```

Follow the connect/bootstrap boilerplate of the nearest existing two-node test in that crate (grep `find_providers` in its tests).

- [ ] **Step 3: Implement** — new command variant `Unprovide(Hash)`; handler: `swarm.behaviour_mut().kad.stop_providing(&key)`.
- [ ] **Step 4: Pass + clippy.** `cargo test -p np2ptp-net`.
- [ ] **Step 5: Commit** — `feat(net): Network::unprovide stops kad provider announcements`

---

### Task 6: Daemon protocol types (tier: Sonnet)

**Files:**
- Create: `crates/np2ptp-node/src/daemon/proto.rs`, `crates/np2ptp-node/src/daemon/mod.rs` (skeleton: `pub mod proto;`)
- Modify: `crates/np2ptp-node/src/lib.rs` (`pub mod daemon;`)

**Interfaces:**
- Produces (Task 7 consumes):

```rust
#[derive(Debug, serde::Deserialize)]
pub struct Request { pub id: u64, #[serde(flatten)] pub op: Op }

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Op {
    Fetch { uri: String, out: String },
    // torrent+data => verified bridge; path => pack. Exactly one form required.
    Convert { torrent: Option<String>, data: Option<String>, path: Option<String> },
    Torrent { input: String, out: Option<String> },
    Provide { nptp: String },
    Unprovide { root: String },
    Status {},
    Shutdown {},
}

pub fn parse_request(line: &str) -> Result<Request, String>; // String = error message for the error event
pub fn event_progress(id: u64, op: &str, done: u64, total: u64) -> String; // one NDJSON line, no trailing \n
pub fn event_result(id: u64, fields: serde_json::Value) -> String;         // {"id":…,"event":"result","ok":true,…fields}
pub fn event_error(id: u64, message: &str) -> String;
pub fn event_ready(version: &str) -> String;                                // {"event":"ready","version":…}
```

`parse_request` also enforces Convert's exactly-one-form rule (`torrent`+`data` together, or `path` alone) so the runtime never sees an ambiguous op.

- [ ] **Step 1: Failing round-trip tests** in `proto.rs`:

```rust
#[test]
fn parses_fetch() {
    let r = parse_request(r#"{"id":7,"cmd":"fetch","uri":"np2ptp:ab","out":"D:/x"}"#).unwrap();
    assert_eq!(r.id, 7);
    assert!(matches!(r.op, Op::Fetch { .. }));
}
#[test]
fn convert_requires_exactly_one_form() {
    assert!(parse_request(r#"{"id":1,"cmd":"convert","path":"a","torrent":"b","data":"c"}"#).is_err());
    assert!(parse_request(r#"{"id":1,"cmd":"convert"}"#).is_err());
    assert!(parse_request(r#"{"id":1,"cmd":"convert","torrent":"t","data":"d"}"#).is_ok());
    assert!(parse_request(r#"{"id":1,"cmd":"convert","path":"p"}"#).is_ok());
}
#[test]
fn events_carry_id_and_shape() {
    let line = event_progress(3, "fetch", 10, 100);
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], 3); assert_eq!(v["event"], "progress"); assert_eq!(v["op"], "fetch");
}
```

- [ ] **Step 2: Fail** — `cargo test -p np2ptp-node proto` → module missing.
- [ ] **Step 3: Implement** exactly the interface above (serde derive + `serde_json::json!` for events).
- [ ] **Step 4: Pass + clippy.**
- [ ] **Step 5: Commit** — `feat(daemon): NDJSON request/event protocol types`

---

### Task 7: Daemon runtime loop (tier: Sonnet build, Opus review)

**Files:**
- Modify: `crates/np2ptp-node/src/daemon/mod.rs` (the loop), `crates/np2ptp-node/src/main.rs` (register `Some("daemon") => cmd_daemon(&args[1..])` + flags `--store/--relay/--tracker/--no-auto-update` + help text), `README.md` (short "Embedding: the daemon" section)

**Interfaces:**
- Consumes: Task 6 proto, Task 5 `Network::unprovide`, Task 2 `register_manifest`/`collect_serve_manifests`, Task 3/4 bridge signature.
- Produces: `pub async fn run_daemon(cfg: DaemonConfig) -> Result<(), Box<dyn Error>>` where `DaemonConfig { store_dir: String, relay: Option<String>, tracker: String, auto_update: bool }`. Behavior contract (Task 8 tests it): emits `ready` after listen+relay dial; one tokio task per request; single stdout writer via `tokio::sync::mpsc::unbounded_channel::<String>()`; `provide` = parse `.nptp` → `net.provide` → `register_manifest` → spawn per-root tracker announce loop (same cadence as `cmd_serve`'s) tracked in `HashMap<Hash, JoinHandle<()>>`; `unprovide` = abort announce task + `net.unprovide(root)`; `status` = result with `{peers: connected_peers().len(), provided: [root hex…], ledger: {…ledger_totals()}}`; `shutdown` = result then clean exit; unknown/bad line = `event_error(id-or-0, …)` and keep running (a malformed line must never kill the daemon).
- Op implementations reuse the existing bodies: fetch → the download path `cmd_fetch`/`cmd_get` use (`download_with_progress` / `net.download_multi_with_progress`, see `main.rs` `cmd_get` ~:230-270); convert-verified → `np2ptp_bridge::resolve_or_convert_local`; convert-pack → the `Store::ingest_tree_files_no_copy_with_progress` path from `cmd_pack` (`main.rs:105-175`); torrent → `resolve_or_convert_remote` with `out` + progress callback. Where `cmd_*` bodies are copy-locked to CLI printing, extract the shared core into private fns in `main.rs`'s modules or `lib.rs` — do NOT change CLI-visible output (rule 5).

- [ ] **Step 1:** Read `cmd_get`, `cmd_fetch`, `cmd_serve` fully; list the extractable cores (function per op) as a short comment block in `daemon/mod.rs` before coding.
- [ ] **Step 2: Failing unit test** for the dispatcher edge that doesn't need a network: malformed line → error event, loop alive:

```rust
#[tokio::test]
async fn bad_line_emits_error_and_daemon_survives() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    handle_line("not json", &tx); // the pure dispatch helper run_daemon uses per stdin line
    let line = rx.recv().await.unwrap();
    assert!(line.contains(r#""event":"error""#));
}
```

(Design `handle_line`/dispatch so parse-and-report is pure and testable; only op execution needs the Network.)

- [ ] **Step 3:** Implement `run_daemon` per the contract above. stdin: `tokio::io::BufReader(stdin()).lines()`; writer task drains the mpsc to stdout, one line each, flush per line.
- [ ] **Step 4:** `cargo test -p np2ptp-node` + clippy 0. Manual smoke: `echo {"id":1,"cmd":"status"} | cargo run -p np2ptp-node -- daemon --no-auto-update` prints `ready` then a result (network-less ops must work offline-ish; relay dial failure = warn event, not exit).
- [ ] **Step 5:** README daemon section (10 lines max: what it is, one request/response example, the flags).
- [ ] **Step 6: Commit** — `feat: np2ptp daemon — persistent NDJSON stdio node (fetch/convert/torrent/provide/status)`

---

### Task 8: Daemon integration test (tier: Opus)

**Files:**
- Create: `crates/np2ptp-node/tests/daemon_stdio.rs`

**Interfaces:**
- Consumes: the built binary (`env!("CARGO_BIN_EXE_np2ptp")` — cargo builds/points to it in integration tests), Task 6 event shapes.

- [ ] **Step 1: Write the test** — full local loop, no external network (pass `--no-relay`-equivalent config: relay dial soft-fails offline, that's tolerated by Task 7 design; tracker unreachable likewise):

```rust
// Spawn daemon A (store TmpDir A). pack a file via the CLI binary into A's
// store, get x.nptp. Send: provide x.nptp -> expect ok result.
// Send: status -> expect provided contains the root.
// Spawn daemon B (store TmpDir B) — connect B to A directly (daemon A's
// ready event includes its listen multiaddrs; B gets {"cmd":"fetch","uri":…}
// after an add-peer step — if no add-peer op exists, extend proto with
// {"cmd":"dial","addr":…} in this task, mirroring cmd_get's --source peer logic).
// Expect on B: >=1 progress event then ok result; fetched bytes == original.
// Send unprovide on A -> status shows empty provided. Send shutdown to both ->
// clean exit codes.
```

Write it with `std::process::Command` + piped stdio, a reader thread per child, and a 60 s hard timeout (fail loudly, never hang CI). Interleave two concurrent fetch ids on B to assert id-correlated events don't cross.

- [ ] **Step 2: Run to fail** (before any missing pieces are patched): `cargo test -p np2ptp-node --test daemon_stdio -- --nocapture`.
- [ ] **Step 3:** Fix what it exposes (this task owns small daemon adjustments like the `dial` op; anything bigger goes back as a bug against Task 7).
- [ ] **Step 4:** PASS + clippy. Full `cargo test --workspace`.
- [ ] **Step 5: Commit** — `test: end-to-end daemon stdio integration (provide/fetch/unprovide across two nodes)`

---

### Task 9: Self-update module (tier: Opus — security-sensitive)

**Files:**
- Create: `crates/np2ptp-node/src/update.rs`
- Modify: `crates/np2ptp-node/Cargo.toml` (deps: `sha2`; Windows-only `windows-sys` features for WinVerifyTrust/CryptQueryObject — check what np2ptp-gui's `AuthenticodeVerifier` calls and mirror it; reuse the HTTP client crate `tracker.rs` already depends on — check its `Cargo.toml`, do not add a second HTTP client)
- Modify: `crates/np2ptp-node/src/lib.rs` (`pub mod update;`)

**Interfaces:**
- Produces (Task 10 consumes):

```rust
pub struct UpdateReport { pub updated: bool, pub from: String, pub to: String }
pub const EXPECTED_SIGNER_THUMBPRINT: &str = "36477BB5DCB10D2C0381A2D79533F0386C5CCACA";

/// Check GitHub latest release for LuanBogoqb/np2ptp; if its tag differs from
/// CARGO_PKG_VERSION, download the platform asset, verify (Windows: Authenticode
/// signer thumbprint == pinned; Linux: SHA-256 vs the release's SHA256SUMS
/// asset), then swap: rename running exe -> .old, write new exe. Never touches
/// the running process image. `timeout` bounds the whole operation.
pub fn check_and_update(timeout: std::time::Duration) -> Result<UpdateReport, UpdateError>;

/// Deletes a leftover np2ptp.exe.old next to the current exe, ignoring errors.
pub fn cleanup_old_binary();
pub fn needs_update(current: &str, latest_tag: &str) -> bool; // tag minus leading v/V; unknown current -> true
pub fn pick_asset<'a>(names: &'a [String]) -> Option<&'a str>; // np2ptp-windows-x86_64.exe / np2ptp-linux-x86_64
```

- [ ] **Step 1: Failing tests for the pure parts:**

```rust
#[test]
fn needs_update_semantics() {
    assert!(!needs_update("0.1.8", "v0.1.8"));
    assert!(needs_update("0.1.8", "v0.1.9"));
    assert!(needs_update("", "v0.1.9")); // unreadable current version
}
#[test]
fn picks_platform_asset() {
    let names = vec!["SHA256SUMS".into(), "np2ptp-windows-x86_64.exe".into(), "np2ptp-linux-x86_64".into()];
    #[cfg(windows)] assert_eq!(pick_asset(&names), Some("np2ptp-windows-x86_64.exe"));
    #[cfg(not(windows))] assert_eq!(pick_asset(&names), Some("np2ptp-linux-x86_64"));
}
#[test]
fn sha256sums_verification() {
    // fixture: bytes + a SHA256SUMS body containing "<hex>  np2ptp-linux-x86_64";
    // verify_sha256(bytes, sums_body, asset_name) -> Ok / tampered bytes -> Err
}
```

- [ ] **Step 2: Fail, then implement the pure parts.** Asset/tag/SHA logic first, all unit-tested.
- [ ] **Step 3: Implement the effectful parts.** GitHub API (`/repos/LuanBogoqb/np2ptp/releases/latest`, `User-Agent` header required). Thumbprint fn behind `#[cfg(windows)]`: port `AuthenticodeVerifier` from np2ptp-gui (read it first; it extracts the signer cert's SHA-1 thumbprint WITHOUT requiring chain trust, which is what sidesteps the known WinVerifyTrust self-signed issue. Mirror that exact call sequence). Verification failure ⇒ delete download, `UpdateError::BadSignature`, current binary untouched. Swap via `std::fs::rename` (same volume, atomic-enough) then write.
- [ ] **Step 4:** `cargo test -p np2ptp-node update` + clippy (run clippy for both `--target` families if cross-checking isn't possible: at minimum Windows locally).
- [ ] **Step 5: Commit** — `feat: self-update with pinned-signer verification (ported from np2ptp-gui BinaryManager)`

---

### Task 10: `update` subcommand + daemon auto-update (tier: Sonnet)

**Files:**
- Modify: `crates/np2ptp-node/src/main.rs` (register `Some("update") => cmd_update(&args[1..])`, help text), `crates/np2ptp-node/src/daemon/mod.rs` (startup hook), `README.md` (2-line update section)

**Interfaces:**
- Consumes: Task 9's `check_and_update`, `cleanup_old_binary`; Task 6's `event_result` (a `{"event":"updated","from":…,"to":…}` line uses the same emitters — add `pub fn event_updated(from: &str, to: &str) -> String` to proto in this task).

- [ ] **Step 1:** `cmd_update`: call `check_and_update(Duration::from_secs(120))`, print `updated 0.1.8 -> 0.1.9, restart to use it` or `already up to date (0.1.8)`. `--json` variant emits one result line. On BadSignature print the full refusal reason (security copy: write it normal, explicit).
- [ ] **Step 2:** Daemon startup (before `ready`): `cleanup_old_binary()`; if `cfg.auto_update`, `check_and_update(Duration::from_secs(30))` — silent on any failure (np2ptp-gui's `TryCheckForUpdateSilentlyAsync` semantics), emit `event_updated` iff updated. Note in README: the daemon updates the binary on disk; the *new* code runs on next daemon start (the embedder owns restarts — document, don't auto-restart).
- [ ] **Step 3:** Tests: golden-ish unit for the two CLI output strings (function returning the message given an `UpdateReport`); daemon path covered by making auto_update=false the Task 8 test default (assert no `updated` event) — the positive path is manual (next release) since it needs a real newer release.
- [ ] **Step 4:** `cargo test --workspace` + clippy 0.
- [ ] **Step 5: Commit** — `feat: np2ptp update subcommand + opt-out auto-update at daemon startup`

---

### Task 11: 3-step golden invariant test (tier: Sonnet)

**Files:**
- Create: `crates/np2ptp-node/tests/golden_quickstart.rs`

**Interfaces:**
- Consumes: the binary via `env!("CARGO_BIN_EXE_np2ptp")`.

- [ ] **Step 1: Write the test** — pure-local 3-step flow, asserting the *shape* of every stdout line against expectations captured from current behavior:

```rust
// 1. pack: fixed sample file (deterministic bytes, the sample() generator) ->
//    assert stdout matches the current pack output exactly, modulo the root
//    hash and paths (normalize: replace hex hashes with <HASH>, temp paths
//    with <PATH>, then compare to a checked-in golden string).
// 2. get with --source (the offline path from the CLI header docs) ->
//    normalized-compare; output file byte-equal to input.
// 3. serve <one>.nptp with --no-tracker --no-relay + immediate SIGKILL after
//    first output line -> first line normalized-compares to golden.
// Golden strings live inline in the test file with a comment: UPDATING THESE
// REQUIRES LUAN'S SIGN-OFF (golden rule 5).
```

- [ ] **Step 2:** Capture the goldens by running the CURRENT `dev` binary (before this plan's CLI wiring lands is ideal — if running after Tasks 1-10, verify against `git stash` / a `main` build that lines are unchanged).
- [ ] **Step 3:** Test passes on the patched tree — this is the proof rule 5 held.
- [ ] **Step 4:** `cargo test --workspace` + clippy.
- [ ] **Step 5: Commit** — `test: golden quick-start invariant (pack/get/serve output frozen)`

---

### Task 12: Sim scenario — multi-manifest serve (tier: Sonnet)

**Files:**
- Modify: `crates/np2ptp-sim` (new scenario alongside existing ones — read `src/lib.rs`/`main.rs` to match the scenario/assertion pattern, e.g. `dedup()` at `lib.rs:102`)

**Interfaces:**
- Consumes: Task 2's `collect_serve_manifests`/`register_manifest` (or drives the net API directly like other scenarios).

- [ ] **Step 1:** Read two existing scenarios end-to-end; write `multi_serve()` in the same style: one node provides two packed contents; a second node fetches both; assert both reconstruct and the provider count for each root ≥1.
- [ ] **Step 2:** Wire into the sim's CI assertion runner (however `dedup` and friends are registered — same mechanism).
- [ ] **Step 3:** `cargo run --release -p np2ptp-sim` (release — golden rule about FEC timing) + `cargo test --workspace`.
- [ ] **Step 4: Commit** — `test(sim): multi-manifest serve scenario`

---

## Self-Review (done at write time)

- **Spec coverage:** Patch 1→Task 1; Patch 1(serve)→Tasks 2,12; Patch 2→Tasks 3,4; Patch 4 daemon→Tasks 5,6,7,8; Patch 5 update→Tasks 9,10; 3-step invariant→Task 11. Embedder-side work lives in its own repository, out of scope here. ✔
- **Placeholders:** none — every step has code or an exact command/file target. Two deliberate "confirm against real source" steps (librqbit stats fields, AuthenticodeVerifier port) are verification steps with expected shapes stated, not TBDs. ✔
- **Type consistency:** `collect_serve_manifests`/`register_manifest` (T2→T7,T12); `resolve_or_convert_remote(net, store, input, no_copy, out_dir, on_progress)` final shape (T3 adds out_dir, T4 adds on_progress — T4's signature is the final one); proto emitters (T6→T7,T8,T10); `check_and_update`/`cleanup_old_binary` (T9→T10). ✔
