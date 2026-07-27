//! Manifest set resolution for `serve`: explicit files, or the store's manifest
//! registry via `--all`.
//!
//! `serve` opaquely registers every manifest it's given into
//! `<store>/manifests/<root-hex>.nptp` so a later `serve --all` (or, in Task 7,
//! the daemon) can find it again without the caller re-passing the path.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use np2ptp_core::Manifest;

use crate::NodeError;

/// Disambiguates concurrent `register_manifest` tmp-file names within one
/// process (see the tmp+rename comment there).
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Directory under the store where registered manifests live.
fn manifests_dir(store_dir: &Path) -> std::path::PathBuf {
    store_dir.join("manifests")
}

/// Resolve the set of manifests `serve` should provide: either the explicit
/// `paths` given on the command line, or — when `all` is set — every manifest
/// previously registered into the store (see [`register_manifest`]).
///
/// Errors if `paths` is empty and `all` is false (nothing to serve), or if
/// `all` is set but the registry has no manifests.
pub fn collect_serve_manifests(
    paths: &[&str],
    all: bool,
    store_dir: &Path,
) -> Result<Vec<Manifest>, NodeError> {
    if all && !paths.is_empty() {
        return Err(NodeError::InvalidUsage(
            "serve: --all and explicit paths are mutually exclusive".into(),
        ));
    }
    if !all {
        if paths.is_empty() {
            return Err(NodeError::InvalidUsage(
                "serve: no manifest given (pass a .nptp path or --all)".into(),
            ));
        }
        let mut out = Vec::with_capacity(paths.len());
        for p in paths {
            let bytes = fs::read(p)?;
            out.push(Manifest::from_nptp(&bytes)?);
        }
        return Ok(out);
    }

    let dir = manifests_dir(store_dir);
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("nptp") {
                // A concurrent daemon unprovide can delete this file between
                // read_dir and here, or a partially-written registration can
                // fail to decode. Either way, skip it and keep serving the
                // rest instead of aborting the whole `--all` run.
                let bytes = match fs::read(&path) {
                    Ok(b) => b,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        eprintln!("serve --all: skipping {} (removed concurrently): {}", path.display(), e);
                        continue;
                    }
                    Err(e) => return Err(e.into()),
                };
                match Manifest::from_nptp(&bytes) {
                    Ok(m) => out.push(m),
                    Err(e) => {
                        eprintln!("serve --all: skipping {} (failed to decode): {}", path.display(), e);
                        continue;
                    }
                }
            }
        }
    }
    if out.is_empty() {
        return Err(NodeError::InvalidUsage(
            format!(
                "serve --all: no registered manifests in {}",
                dir.display()
            ),
        ));
    }
    Ok(out)
}

/// Register `manifest` into the store's manifest registry so `serve --all`
/// (or the daemon) can find it later. Idempotent: writing the same manifest
/// twice is a no-op after the first write.
pub fn register_manifest(store_dir: &Path, manifest: &Manifest) -> Result<(), NodeError> {
    let dir = manifests_dir(store_dir);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.nptp", manifest.root.to_hex()));
    // Stage the bytes in a tmp file and rename into place instead of an
    // exists-check-then-write (TOCTOU: two `serve`s racing on the same root
    // could otherwise interleave and truncate/corrupt the file). `fs::rename`
    // overwrites an existing destination on both Unix (rename(2)) and Windows
    // (MoveFileExW + MOVEFILE_REPLACE_EXISTING), so a concurrent registration
    // of the same manifest just clobbers with identical bytes — no error, no
    // corruption, still idempotent.
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!("{}.nptp.tmp-{}-{}", manifest.root.to_hex(), std::process::id(), n));
    fs::write(&tmp, manifest.to_nptp()?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, Ordering};

    use np2ptp_store::Store;

    use super::*;

    struct TmpDir(std::path::PathBuf);

    impl TmpDir {
        fn new() -> TmpDir {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("np2ptp-serveset-{}-{}", std::process::id(), n));
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

    #[test]
    fn collect_explicit_paths_loads_all() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let m1 = crate::pack(b"one".repeat(5000).as_slice(), Some("a".into()), &store).unwrap();
        let m2 = crate::pack(b"two".repeat(5000).as_slice(), Some("b".into()), &store).unwrap();
        let p1 = dir.path().join("a.nptp");
        std::fs::write(&p1, m1.to_nptp().unwrap()).unwrap();
        let p2 = dir.path().join("b.nptp");
        std::fs::write(&p2, m2.to_nptp().unwrap()).unwrap();
        let got = collect_serve_manifests(&[p1.to_str().unwrap(), p2.to_str().unwrap()], false, dir.path()).unwrap();
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn register_then_collect_all() {
        let dir = TmpDir::new();
        let store = Store::open(dir.path()).unwrap();
        let m = crate::pack(b"x".repeat(5000).as_slice(), Some("a".into()), &store).unwrap();
        register_manifest(dir.path(), &m).unwrap();
        register_manifest(dir.path(), &m).unwrap(); // idempotent
        let got = collect_serve_manifests(&[], true, dir.path()).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].root, m.root);
    }

    #[test]
    fn all_with_empty_registry_errors() {
        let dir = TmpDir::new();
        assert!(collect_serve_manifests(&[], true, dir.path()).is_err());
    }

    #[test]
    fn all_and_explicit_paths_are_mutually_exclusive() {
        let dir = TmpDir::new();
        let err = collect_serve_manifests(&["a.nptp"], true, dir.path()).unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }
}
