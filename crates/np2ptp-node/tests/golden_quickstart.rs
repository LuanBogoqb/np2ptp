//! 3-step golden invariant: pack -> get -> serve, run through the real
//! `np2ptp` binary, with stdout compared against goldens captured from the
//! pre-patch binary. Only the content hash (unpredictable, content-addressed)
//! and the TmpDir's absolute path prefix are normalized (to `<HASH>` and
//! `<TMP>`); file names, byte counts and file counts are known from the
//! fixture and asserted literally. The chunk count is not independently
//! known (it depends on the chunker), so it is captured from the pack line
//! and then asserted to reappear identically in the serve and get lines —
//! catching a swapped/wrong count that a blanket `<N>` placeholder could not.
//!
//! UPDATING THESE GOLDENS REQUIRES LUAN'S SIGN-OFF (golden rule 5).
//!
//! (byte/chunk counts are asserted, not normalized — see above.)

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

/// Replace the TmpDir's own absolute path prefix with `<TMP>`, leaving the
/// rest of the path (the file name) literal — so a pack/get that swaps input
/// and output paths produces a different string, not a matching one.
fn normalize_tmp(s: &str, tmp_dir: &Path) -> String {
    s.replace(&tmp_dir.display().to_string(), "<TMP>")
}

/// `<TMP>`-normalize then hash-normalize a full path for building expected
/// golden strings from the fixture's own `Path`s.
fn tmp_path(p: &Path, tmp_dir: &Path) -> String {
    normalize_tmp(&p.display().to_string(), tmp_dir)
}

/// Normalize a captured stdout line: known temp dir prefix -> `<TMP>`, then
/// `np2ptp:<hash>` links -> `np2ptp:<HASH>`. Byte/file/chunk counts are left
/// as literal text — the test asserts them, it does not erase them.
fn normalize_line(line: &str, tmp_dir: &Path) -> String {
    let s = normalize_tmp(line, tmp_dir);
    normalize_hashes(&s)
}

fn normalized_lines(output: &str, tmp_dir: &Path) -> Vec<String> {
    output
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| normalize_line(l, tmp_dir))
        .collect()
}

/// Parse the decimal count that follows `label` in `line` (e.g. `"chunks:"`
/// in `"  files: 1   chunks: 28   store: ..."`).
fn parse_count(line: &str, label: &str) -> u64 {
    let idx = line
        .find(label)
        .unwrap_or_else(|| panic!("expected {label:?} in {line:?}"));
    let rest = &line[idx + label.len()..];
    let digits: String = rest
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("could not parse a count after {label:?} in {line:?}"))
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
    let pack_lines = normalized_lines(&pack_stdout, dir.path());
    let files_count = parse_count(&pack_lines[1], "files:");
    let chunks_count = parse_count(&pack_lines[1], "chunks:");
    assert_eq!(files_count, 1, "one file was packed; raw stdout was:\n{pack_stdout}");
    let pack_golden = vec![
        format!(
            "packed {} ({} bytes) -> {}",
            tmp_path(&input, dir.path()),
            data.len(),
            tmp_path(&nptp, dir.path())
        ),
        format!(
            "  files: {}   chunks: {}   store: {}",
            files_count,
            chunks_count,
            tmp_path(&seed_store, dir.path())
        ),
        "  link:  np2ptp:<HASH>".to_string(),
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
    let get_lines = normalized_lines(&get_stdout, dir.path());
    let get_golden = vec![
        format!(
            "downloaded np2ptp:<HASH> ({} bytes) -> {}",
            data.len(),
            tmp_path(&restored, dir.path())
        ),
        format!("  fetched {} chunks, 0 already local (deduped)", chunks_count),
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

    let serve_line = normalize_line(&first_line, dir.path());
    assert_eq!(
        serve_line,
        format!("serving np2ptp:<HASH> ({} files, {} chunks)", files_count, chunks_count),
        "raw line was: {first_line:?}"
    );
}
