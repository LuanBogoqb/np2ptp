//! 3-step golden invariant: pack -> get -> serve, run through the real
//! `np2ptp` binary, with stdout compared against goldens captured from the
//! pre-patch binary. Hex hashes and numeric counts drift with the sample
//! fixture's exact bytes/chunking, so both are normalized to `<HASH>` /
//! `<N>` before comparison — the LINE STRUCTURE and literal words are the
//! invariant, not the specific numbers.
//!
//! UPDATING THESE GOLDENS REQUIRES LUAN'S SIGN-OFF (golden rule 5).

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new() -> TmpDir {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("np2ptp-golden-{}-{}", std::process::id(), n));
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

/// Replace every `np2ptp:<hex...>` link with `np2ptp:<HASH>`.
fn normalize_hashes(s: &str) -> String {
    const PREFIX: &str = "np2ptp:";
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        if s[i..].starts_with(PREFIX) {
            out.push_str(PREFIX);
            let rest = &s[i + PREFIX.len()..];
            let hexlen = rest.chars().take_while(|c| c.is_ascii_hexdigit()).count();
            out.push_str("<HASH>");
            i += PREFIX.len() + hexlen;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Collapse every run of ASCII digits (byte/chunk/file counts) to `<N>`,
/// but only when it stands alone — not when it's embedded in an identifier
/// (e.g. the literal "2" in "np2ptp" must survive untouched).
fn collapse_digits(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let prev_is_word = start > 0 && (chars[start - 1].is_alphanumeric() || chars[start - 1] == '_');
            let next_is_word = i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_');
            if prev_is_word || next_is_word {
                out.extend(&chars[start..i]);
            } else {
                out.push_str("<N>");
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// Normalize a captured stdout line: known temp paths -> `<PATH>`, then
/// `np2ptp:<hash>` links -> `np2ptp:<HASH>`, then standalone digit runs -> `<N>`.
fn normalize_line(line: &str, paths: &[&Path]) -> String {
    let mut s = line.to_string();
    for p in paths {
        let disp = p.display().to_string();
        s = s.replace(&disp, "<PATH>");
    }
    s = normalize_hashes(&s);
    s = collapse_digits(&s);
    s
}

fn normalized_lines(output: &str, paths: &[&Path]) -> Vec<String> {
    output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| normalize_line(l, paths))
        .collect()
}

#[test]
fn pack_get_serve_output_matches_the_frozen_golden_shape() {
    let dir = TmpDir::new();
    let input = dir.path().join("f.bin");
    let data = sample(2_000_000, 5);
    std::fs::write(&input, &data).unwrap();
    let seed_store = dir.path().join("seed-store");
    let nptp = dir.path().join("f.nptp");

    // Step 1: pack.
    let pack_output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("pack")
        .arg(&input)
        .arg("--store")
        .arg(&seed_store)
        .arg("--out")
        .arg(&nptp)
        .output()
        .unwrap();
    assert!(
        pack_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&pack_output.stderr)
    );
    let pack_stdout = String::from_utf8(pack_output.stdout).unwrap();
    let pack_lines = normalized_lines(&pack_stdout, &[&input, &seed_store, &nptp]);
    let pack_golden = [
        "packed <PATH> (<N> bytes) -> <PATH>",
        "  files: <N>   chunks: <N>   store: <PATH>",
        "  link:  np2ptp:<HASH>",
    ];
    assert_eq!(pack_lines, pack_golden, "raw stdout was:\n{pack_stdout}");

    // Step 2: get, from the offline seed store (--source), byte-compare the
    // reconstructed file against the original input.
    let client_store = dir.path().join("client-store");
    let restored = dir.path().join("f2.bin");
    let get_output = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("get")
        .arg(&nptp)
        .arg("--source")
        .arg(&seed_store)
        .arg("--store")
        .arg(&client_store)
        .arg("--out")
        .arg(&restored)
        .output()
        .unwrap();
    assert!(
        get_output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&get_output.stderr)
    );
    let get_stdout = String::from_utf8(get_output.stdout).unwrap();
    let get_lines = normalized_lines(&get_stdout, &[&nptp, &restored]);
    let get_golden = [
        "downloaded np2ptp:<HASH> (<N> bytes) -> <PATH>",
        "  fetched <N> chunks, <N> already local (deduped)",
    ];
    assert_eq!(get_lines, get_golden, "raw stdout was:\n{get_stdout}");
    assert_eq!(std::fs::read(&restored).unwrap(), data, "restored bytes must match the original input");

    // Step 3: serve, offline (--no-tracker --no-relay), read its first stdout
    // line then SIGKILL — the process would otherwise run forever.
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_np2ptp"))
        .arg("serve")
        .arg(&nptp)
        .arg("--store")
        .arg(&seed_store)
        .arg("--no-tracker")
        .arg("--no-relay")
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::{BufRead, BufReader};
        if let Some(Ok(line)) = BufReader::new(stdout).lines().next() {
            let _ = tx.send(line);
        }
    });
    let first_line = rx.recv_timeout(std::time::Duration::from_secs(60));
    let _ = child.kill();
    let _ = child.wait();
    let first_line = first_line.expect("serve did not print a line within 60s");

    let serve_line = normalize_line(&first_line, &[&nptp, &seed_store]);
    assert_eq!(serve_line, "serving np2ptp:<HASH> (<N> files, <N> chunks)", "raw line was: {first_line:?}");
}
