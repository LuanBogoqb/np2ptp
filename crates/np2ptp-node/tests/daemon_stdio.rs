//! End-to-end daemon test: two real `np2ptp daemon` processes, real NDJSON over
//! real pipes, real content moved between them over QUIC on loopback.
//!
//! What it proves, in order: `provide` seeds a packed root; `status` reports it;
//! a second daemon `dial`s the first using the addresses the `ready` event
//! carried; `fetch` pulls the content back byte-for-byte; `unprovide` retires
//! one root and leaves the other; `shutdown` ends both processes with a success
//! exit code. Two fetches are fired concurrently with different ids to prove
//! events stay correlated to their own request; both go out in the same write
//! as the `dial`, so they reach the daemon while the handshake is still in
//! flight (see the comment at that step for what that does and does not prove).
//! A third daemon asks for a fabricated root nobody packed; the unservable
//! fetch still ends in exactly one terminal `error`, inside the deadline
//! (mDNS makes "nothing dialed" not mean "no peers" on loopback, hence the
//! fake root rather than a real one).
//!
//! No internet required, by construction:
//!  - `--relay /np2ptp-test/no-relay` is not a parsable multiaddr, so the daemon
//!    skips the relay dial entirely and emits a `warn` (the offline path Task 7
//!    designed for) instead of hanging on an unreachable host.
//!  - `--tracker http://127.0.0.1:1` refuses instantly; `fetch` falls back to
//!    the peer we dialed ourselves.
//!
//! Robustness rules this file follows, so it can never wedge CI:
//!  - one reader thread per child per stream (stdout *and* stderr), so neither
//!    pipe can fill and block the daemon mid-test;
//!  - a single 90s [`Deadline`] shared by every wait — each one fails loudly,
//!    dumping every event seen so far plus the child's stderr;
//!  - `Daemon` kills and reaps its child in `Drop`, so a panic anywhere (or an
//!    early `?`/assert) still leaves no orphaned process behind.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// Whole-test budget. Every blocking wait is bounded by it, so the worst case is
/// a loud failure at ~90s, never a hung test runner.
const TEST_BUDGET: Duration = Duration::from_secs(90);
const NO_RELAY: &str = "/np2ptp-test/no-relay";
/// Port 1 on loopback: nothing listens, so the connection is refused at once.
const DEAD_TRACKER: &str = "http://127.0.0.1:1";

// --------------------------------------------------------------- test fixtures

struct TmpDir(PathBuf);

impl TmpDir {
    fn new() -> TmpDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-daemon-{}-{}", std::process::id(), n));
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

fn sample(n: usize, seed: u64) -> Vec<u8> {
    let mut x = 0x9E3779B97F4A7C15u64 ^ seed.wrapping_mul(0xD1B54A32D192ED03);
    (0..n)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545F4914F6CDD1D) >> 33) as u8
        })
        .collect()
}

/// Shared wall-clock budget for the whole test.
struct Deadline(Instant);

impl Deadline {
    fn new(budget: Duration) -> Deadline {
        Deadline(Instant::now() + budget)
    }
    fn remaining(&self) -> Duration {
        self.0.saturating_duration_since(Instant::now())
    }
    fn expired(&self) -> bool {
        self.remaining().is_zero()
    }
}

// ------------------------------------------------------------- daemon harness

/// A live `np2ptp daemon` child plus everything read off it so far.
///
/// Every stdout line is parsed as JSON and appended to `log` by [`Daemon::pump`];
/// [`Daemon::wait_for`] matches against already-logged events before blocking,
/// so events that arrive out of order (two concurrent requests interleaving)
/// are never lost. `used` marks the ones a wait already consumed, so waiting
/// twice for "a progress event with id N" needs two distinct events.
struct Daemon {
    name: &'static str,
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<String>,
    stderr: Arc<Mutex<String>>,
    log: Vec<Value>,
    used: Vec<bool>,
    sent_ids: Vec<u64>,
}

impl Daemon {
    fn spawn(name: &'static str, store: &Path) -> Daemon {
        let mut child = Command::new(env!("CARGO_BIN_EXE_np2ptp"))
            .arg("daemon")
            .arg("--store")
            .arg(store)
            .arg("--tracker")
            .arg(DEAD_TRACKER)
            .arg("--relay")
            .arg(NO_RELAY)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("daemon {name}: spawn failed: {e}"));

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Reader thread per stream: the daemon must never block writing into a
        // pipe nobody is draining.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let err_buf = Arc::new(Mutex::new(String::new()));
        let err_sink = Arc::clone(&err_buf);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Ok(mut b) = err_sink.lock() {
                    b.push_str(&line);
                    b.push('\n');
                }
            }
        });

        Daemon {
            name,
            child,
            stdin: Some(stdin),
            rx,
            stderr: err_buf,
            log: Vec::new(),
            used: Vec::new(),
            sent_ids: Vec::new(),
        }
    }

    /// Everything seen so far, for panic messages.
    fn dump(&self) -> String {
        let events: Vec<String> = self.log.iter().map(|e| format!("    {e}")).collect();
        let err = self.stderr.lock().map(|b| b.clone()).unwrap_or_default();
        format!(
            "daemon {} stdout events:\n{}\n  daemon {} stderr:\n{}",
            self.name,
            if events.is_empty() { "    <none>".to_string() } else { events.join("\n") },
            self.name,
            if err.is_empty() { "    <empty>" } else { err.as_str() }
        )
    }

    fn send(&mut self, req: Value) {
        let line = req.to_string();
        if let Some(id) = req.get("id").and_then(Value::as_u64) {
            self.sent_ids.push(id);
        }
        let name = self.name;
        let stdin = self.stdin.as_mut().unwrap_or_else(|| panic!("daemon {name}: stdin closed"));
        writeln!(stdin, "{line}").unwrap_or_else(|e| panic!("daemon {name}: write {line}: {e}"));
        stdin.flush().unwrap_or_else(|e| panic!("daemon {name}: flush {line}: {e}"));
    }

    /// Writes several requests in one `write` + `flush`, so the daemon's reader
    /// takes them from a single chunk and dispatches them back to back. Used
    /// where the gap between two ops must be as small as this harness can make
    /// it.
    fn send_batch(&mut self, reqs: &[Value]) {
        let mut buf = String::new();
        for req in reqs {
            if let Some(id) = req.get("id").and_then(Value::as_u64) {
                self.sent_ids.push(id);
            }
            buf.push_str(&req.to_string());
            buf.push('\n');
        }
        let name = self.name;
        let stdin = self.stdin.as_mut().unwrap_or_else(|| panic!("daemon {name}: stdin closed"));
        stdin
            .write_all(buf.as_bytes())
            .unwrap_or_else(|e| panic!("daemon {name}: write batch {buf:?}: {e}"));
        stdin.flush().unwrap_or_else(|e| panic!("daemon {name}: flush batch: {e}"));
    }

    /// Block for one more stdout line and log it. Panics loudly on timeout or
    /// on a line that isn't JSON (stdout must be pure NDJSON — that is the
    /// contract an embedder parses against).
    fn pump(&mut self, dl: &Deadline, what: &str) {
        match self.rx.recv_timeout(dl.remaining()) {
            Ok(line) => {
                let v: Value = serde_json::from_str(&line).unwrap_or_else(|e| {
                    panic!(
                        "daemon {}: non-JSON line on stdout ({e}): {line:?}\n{}",
                        self.name,
                        self.dump()
                    )
                });
                self.log.push(v);
                self.used.push(false);
            }
            Err(RecvTimeoutError::Timeout) => panic!(
                "TIMEOUT after {:?}: daemon {} never produced {what}\n{}",
                TEST_BUDGET,
                self.name,
                self.dump()
            ),
            Err(RecvTimeoutError::Disconnected) => panic!(
                "daemon {} closed stdout while waiting for {what} (exited early?)\n{}",
                self.name,
                self.dump()
            ),
        }
    }

    fn wait_for(&mut self, dl: &Deadline, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        loop {
            let found =
                self.log.iter().zip(self.used.iter()).position(|(e, used)| !*used && pred(e));
            if let Some(i) = found {
                self.used[i] = true;
                return self.log[i].clone();
            }
            self.pump(dl, what);
        }
    }

    /// Waits for the terminal event of request `id` and asserts it succeeded —
    /// an `error` event fails here with its own message instead of stalling
    /// until the deadline.
    fn wait_result(&mut self, dl: &Deadline, id: u64) -> Value {
        let what = format!("a result for id {id}");
        let v = self.wait_for(dl, &what, |e| {
            e["id"].as_u64() == Some(id)
                && matches!(e["event"].as_str(), Some("result") | Some("error"))
        });
        assert_eq!(v["event"], "result", "daemon {}: id {id} errored: {v}\n{}", self.name, self.dump());
        assert_eq!(v["ok"], true, "daemon {}: id {id} not ok: {v}", self.name);
        v
    }

    fn wait_ready(&mut self, dl: &Deadline) -> Value {
        self.wait_for(dl, "its ready event", |e| e["event"] == "ready")
    }

    /// Waits for the child to exit on its own (stdin deliberately left open, so
    /// exiting proves the `shutdown` op worked and not just an stdin EOF).
    fn expect_clean_exit(&mut self, dl: &Deadline) {
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(
                        status.success(),
                        "daemon {} exited with {status}\n{}",
                        self.name,
                        self.dump()
                    );
                    return;
                }
                Ok(None) => {
                    if dl.expired() {
                        panic!(
                            "TIMEOUT: daemon {} still running after shutdown\n{}",
                            self.name,
                            self.dump()
                        );
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => panic!("daemon {}: wait failed: {e}", self.name),
            }
        }
    }

    /// Every event that carries an `id` must carry one we actually sent.
    fn assert_ids_are_ours(&self) {
        for e in &self.log {
            if let Some(id) = e.get("id").and_then(Value::as_u64) {
                assert!(
                    self.sent_ids.contains(&id),
                    "daemon {}: event carries id {id}, which was never requested: {e}\n{}",
                    self.name,
                    self.dump()
                );
            }
        }
    }

    fn assert_no_error_events(&self) {
        for e in &self.log {
            assert_ne!(e["event"], "error", "daemon {}: unexpected error event: {e}", self.name);
        }
    }
}

impl Drop for Daemon {
    /// The guard: on any exit path — clean end, assertion failure, panic in
    /// another helper — the child dies and is reaped here.
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ------------------------------------------------------------------- helpers

/// Pack `input` into `store` through the real CLI, writing `nptp`.
fn pack(input: &Path, store: &Path, nptp: &Path) {
    let out = Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("pack")
        .arg(input)
        .arg("--store")
        .arg(store)
        .arg("--out")
        .arg(nptp)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "pack {input:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A path as the daemon protocol carries it (JSON string). `json!` escapes the
/// backslashes in a Windows path for us; this just avoids `Cow` in the asserts.
fn s(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// The loopback address out of a `ready`/`status` `addrs` array — that is the
/// one a second daemon on this machine can always reach, with no firewall
/// prompt and no LAN in the picture.
fn loopback_addr(addrs: &Value, name: &str) -> String {
    let strs: Vec<String> = addrs
        .as_array()
        .unwrap_or_else(|| panic!("daemon {name}: addrs is not an array: {addrs}"))
        .iter()
        .filter_map(|a| a.as_str().map(str::to_string))
        .collect();
    strs.iter()
        .find(|a| a.starts_with("/ip4/127.0.0.1/"))
        .or_else(|| strs.iter().find(|a| a.starts_with("/ip4/") && !a.starts_with("/ip4/0.0.0.0/")))
        .cloned()
        .unwrap_or_else(|| panic!("daemon {name}: no dialable ip4 address in {strs:?}"))
}

fn provided_roots(status: &Value) -> Vec<String> {
    status["provided"]
        .as_array()
        .unwrap_or_else(|| panic!("status has no provided array: {status}"))
        .iter()
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect()
}

fn count_progress(daemon: &Daemon, id: u64) -> usize {
    daemon
        .log
        .iter()
        .filter(|e| e["event"] == "progress" && e["id"].as_u64() == Some(id))
        .count()
}

// ---------------------------------------------------------------- the test

#[test]
fn daemon_provides_and_a_second_daemon_fetches_it_over_stdio() {
    let dl = Deadline::new(TEST_BUDGET);
    let dir = TmpDir::new();
    let store_a = dir.path().join("store-a");
    let store_b = dir.path().join("store-b");

    // Two distinct fixtures: the concurrent-fetch step needs two roots whose
    // results are telling apart-able, so a crossed id is impossible to miss.
    let data1 = sample(2_000_000, 5);
    let data2 = sample(1_500_000, 11);
    let file1 = dir.path().join("one.bin");
    let file2 = dir.path().join("two.bin");
    std::fs::write(&file1, &data1).unwrap();
    std::fs::write(&file2, &data2).unwrap();
    let nptp1 = dir.path().join("one.nptp");
    let nptp2 = dir.path().join("two.nptp");
    pack(&file1, &store_a, &nptp1);
    pack(&file2, &store_a, &nptp2);

    let mut a = Daemon::spawn("A", &store_a);
    let ready_a = a.wait_ready(&dl);
    assert!(ready_a["version"].is_string(), "ready has no version: {ready_a}");
    assert!(ready_a["peer_id"].is_string(), "ready has no peer_id: {ready_a}");
    let dial_addr = loopback_addr(&ready_a["addrs"], "A");

    // --- provide both roots on A -------------------------------------------
    a.send(json!({"id": 1, "cmd": "provide", "nptp": s(&nptp1)}));
    a.send(json!({"id": 2, "cmd": "provide", "nptp": s(&nptp2)}));
    let root1 = a.wait_result(&dl, 1)["root"].as_str().expect("provide result has root").to_string();
    let root2 = a.wait_result(&dl, 2)["root"].as_str().expect("provide result has root").to_string();
    assert_ne!(root1, root2, "two different fixtures must have two different roots");

    a.send(json!({"id": 3, "cmd": "status"}));
    let status_a = a.wait_result(&dl, 3);
    let provided = provided_roots(&status_a);
    assert!(provided.contains(&root1), "status is missing {root1}: {status_a}");
    assert!(provided.contains(&root2), "status is missing {root2}: {status_a}");

    // --- second daemon, dialed straight at A's listen address ---------------
    let mut b = Daemon::spawn("B", &store_b);
    let ready_b = b.wait_ready(&dl);
    assert_ne!(
        ready_b["peer_id"], ready_a["peer_id"],
        "two stores must yield two identities: {ready_b}"
    );

    // --- dial + two concurrent fetches, one single write --------------------
    // The three requests go out in one `write`, deliberately: the daemon reads
    // them from a single chunk and dispatches the fetches immediately after the
    // dial, with the QUIC handshake to A still in flight. This mirrors the
    // dial-then-fetch flow the README documents, where a `dial` op answers
    // before a connection is fully established.
    //
    // What this test does verify:
    //  - the `dial` op succeeds and reports the address we reached;
    //  - a `fetch` right after a `dial` in the same batch succeeds;
    //  - two concurrent fetches with different request ids keep their responses
    //    separate (no id crossover);
    //  - bytes transferred match the original content exactly.
    //
    // What this test does NOT verify:
    //  - that the daemon's settle-and-retry logic in `run_fetch` actually waits
    //    for a peer connection when that peer is not yet connected. On loopback,
    //    np2ptp-net's mDNS runs unconditionally and discovers peer A within
    //    milliseconds, populating connected_peers before run_fetch samples it.
    //    This means the test passes regardless of whether the settle logic works:
    //    a build with no settle at all could still pass by winning the discovery
    //    race.
    //
    // Where settle-and-retry IS actually verified:
    //  - Code review of daemon/mod.rs (the settle wait and retry ladder).
    //  - Live environments: peers on different subnets, across WAN, or where
    //    mDNS is blocked. Those are the cases where settle-and-retry matters.
    let out1 = dir.path().join("fetched-one.bin");
    let out2 = dir.path().join("fetched-two.bin");
    let (out1_s, out2_s) = (s(&out1), s(&out2));
    b.send_batch(&[
        json!({"id": 100, "cmd": "dial", "addr": dial_addr.clone()}),
        json!({"id": 200, "cmd": "fetch", "uri": root1, "out": out1_s.clone()}),
        json!({"id": 201, "cmd": "fetch", "uri": root2, "out": out2_s.clone()}),
    ]);
    let dialed = b.wait_result(&dl, 100);
    assert_eq!(dialed["addr"], dial_addr.as_str());
    let fetched1 = b.wait_result(&dl, 200);
    let fetched2 = b.wait_result(&dl, 201);

    // Id correlation: each result must describe *its own* request's content.
    assert_eq!(fetched1["root"], root1.as_str(), "id 200 result carries the wrong root: {fetched1}");
    assert_eq!(fetched2["root"], root2.as_str(), "id 201 result carries the wrong root: {fetched2}");
    assert_eq!(fetched1["bytes_total"].as_u64(), Some(data1.len() as u64), "{fetched1}");
    assert_eq!(fetched2["bytes_total"].as_u64(), Some(data2.len() as u64), "{fetched2}");
    assert_eq!(fetched1["path"], out1_s.as_str(), "{fetched1}");
    assert_eq!(fetched2["path"], out2_s.as_str(), "{fetched2}");
    assert!(count_progress(&b, 200) >= 1, "no progress events for id 200\n{}", b.dump());
    assert!(count_progress(&b, 201) >= 1, "no progress events for id 201\n{}", b.dump());
    b.assert_ids_are_ours();

    // The whole point: the bytes came across intact.
    assert_eq!(std::fs::read(&out1).unwrap(), data1, "fetched content 1 differs from the original");
    assert_eq!(std::fs::read(&out2).unwrap(), data2, "fetched content 2 differs from the original");

    // --- negative control: a fetch nobody on earth can serve -----------------
    // FIX 1 moved `run_fetch`'s "no peers" bail *below* the settle wait, so the
    // op now always spends the settle window and the retry ladder before it can
    // fail. This proves that path still terminates: a third daemon asks for a
    // fabricated root — 64 valid hex chars that were never packed — so no
    // provider exists on any discovery path.
    //
    // Fabricated rather than "a real root with nothing dialed" because
    // np2ptp-net runs mDNS: on loopback a fresh daemon finds A by itself, so
    // "no tracker and no dial" is not "no peers" here. The earlier version of
    // this control fetched successfully and proved nothing.
    //
    // What it pins: exactly one terminal event, an `error`, inside the shared
    // deadline, with no output file — the settle cannot hang the op and the
    // bounded retry ladder cannot outlive it. Which failure shape it takes (the
    // "no peers" bail if mDNS has not landed yet, or a download failure against
    // a peer that does not have the content) depends on discovery timing, so
    // the message is not asserted, only that it is a non-empty error.
    let store_c = dir.path().join("store-c");
    let never = dir.path().join("never.bin");
    let nobodys_root = format!("np2ptp:{}", "de1e7ed0".repeat(8));
    let mut c = Daemon::spawn("C", &store_c);
    c.wait_ready(&dl);
    c.send(json!({"id": 400, "cmd": "fetch", "uri": nobodys_root, "out": s(&never)}));
    let failed = c.wait_for(&dl, "a terminal event for id 400", |e| {
        e["id"].as_u64() == Some(400)
            && matches!(e["event"].as_str(), Some("result") | Some("error"))
    });
    assert_eq!(
        failed["event"], "error",
        "a fetch for a root nobody has must fail: {failed}\n{}",
        c.dump()
    );
    assert_eq!(failed["ok"], false, "an error event must carry ok:false: {failed}");
    assert!(
        !failed["message"].as_str().unwrap_or_default().is_empty(),
        "an error event must carry a message: {failed}"
    );
    assert!(!never.exists(), "a failed fetch must not leave an output file: {never:?}");
    c.send(json!({"id": 401, "cmd": "shutdown"}));
    c.wait_result(&dl, 401);
    c.expect_clean_exit(&dl);
    c.assert_ids_are_ours();

    // --- unprovide one root on A, the other stays ---------------------------
    a.send(json!({"id": 4, "cmd": "unprovide", "root": root1}));
    let unprovided = a.wait_result(&dl, 4);
    assert_eq!(unprovided["root"], root1.as_str(), "{unprovided}");

    a.send(json!({"id": 5, "cmd": "status"}));
    let status_a = a.wait_result(&dl, 5);
    let provided = provided_roots(&status_a);
    assert!(!provided.contains(&root1), "{root1} is still provided after unprovide: {status_a}");
    assert!(provided.contains(&root2), "unprovide dropped the wrong root: {status_a}");

    // --- shutdown both, clean exit codes ------------------------------------
    a.send(json!({"id": 6, "cmd": "shutdown"}));
    b.send(json!({"id": 300, "cmd": "shutdown"}));
    a.wait_result(&dl, 6);
    b.wait_result(&dl, 300);
    a.expect_clean_exit(&dl);
    b.expect_clean_exit(&dl);

    // Nothing above should have produced an error event on either side (the
    // dead tracker and the disabled relay are `warn`s by design).
    a.assert_no_error_events();
    b.assert_no_error_events();
    a.assert_ids_are_ours();
}
