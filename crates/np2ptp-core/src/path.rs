//! Validation for peer-supplied relative paths (manifest `files[].path`).
//!
//! A manifest arrives from the network, and its paths decide where fetched
//! bytes land on disk. The Merkle root commits to the chunk list but **not**
//! to the path strings, so a hostile manifest can be internally consistent
//! and still try to write outside the output directory. Every consumer of a
//! peer-supplied path (store export, node `write_tree`, daemon output) must
//! run [`validate_relative_path`] before building a `PathBuf` from it.
//!
//! `Path::components()` is the authority — not a `split('/')` — because on
//! Windows `\` is also a separator (`"x\..\..\evil"` has no `..` component
//! when split on `/` alone), and pushing a component with a drive/UNC prefix
//! (`"C:\evil"`, `"\\?\C:\evil"`) *replaces* the destination instead of
//! appending to it. Backslash is rejected on every platform so a manifest is
//! validated the same way everywhere.

/// Reject a path that is empty, absolute, escapes its output directory, or
/// carries Windows-specific escapes (backslash separators, drive/UNC prefixes,
/// NTFS alternate data streams, reserved device names). All of the manifest's
/// own writers produce '/'-separated paths, so none of this rejects anything
/// NP2PTP itself packs.
pub fn validate_relative_path(path: &str) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("path must not be empty");
    }
    if path.contains('\\') {
        // Windows treats this as a separator even where we don't; reject on
        // every platform so validation doesn't depend on the host OS.
        return Err("path must not contain backslashes");
    }
    let mut components = 0usize;
    for comp in std::path::Path::new(path).components() {
        match comp {
            std::path::Component::Normal(os) => {
                let name = os.to_str().ok_or("path component is not valid UTF-8")?;
                // ':' has no business in a relative path and enables NTFS
                // alternate data streams (`file.exe:evil.exe`).
                if name.contains(':') {
                    return Err("path component must not contain ':'");
                }
                // Windows silently strips trailing dots/spaces, so the file
                // would land under a different name than the manifest lists.
                if name.ends_with('.') || name.ends_with(' ') {
                    return Err("path component ends with '.' or ' '");
                }
                if is_windows_reserved_device(name) {
                    return Err("path uses a reserved Windows device name");
                }
                components += 1;
            }
            // Root/Prefix (`C:`, `\\?\…`), CurDir (`.`), ParentDir (`..`).
            _ => return Err("path must be relative, without '.' or '..'"),
        }
    }
    if components == 0 {
        return Err("path must name at least one file");
    }
    Ok(())
}

/// Windows reserves a set of device names in any directory, with or without an
/// extension — writing `CON` or `NUL` hangs or misroutes the handle. The
/// NTFS metadata names (`$MFT` and friends) are in the same bucket.
fn is_windows_reserved_device(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("");
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL"
            | "COM1" | "COM2" | "COM3" | "COM4" | "COM5" | "COM6" | "COM7" | "COM8" | "COM9"
            | "LPT1" | "LPT2" | "LPT3" | "LPT4" | "LPT5" | "LPT6" | "LPT7" | "LPT8" | "LPT9"
            | "$MFT" | "$MFTMIRR" | "$LOGFILE" | "$VOLUME" | "$ATTRDEF" | "$BITMAP"
            | "$BOOT" | "$BADCLUS" | "$SECURE" | "$UPCASE" | "$EXTEND" | "$QUOTA"
            | "$OBJID" | "$REPARSE"
    )
}

#[cfg(test)]
mod tests {
    use super::validate_relative_path;

    #[test]
    fn accepts_ordinary_relative_paths() {
        for ok in ["file.txt", "dir/file.txt", "a/b/c.tar.gz", "deep/nested/名前.dat"] {
            assert!(validate_relative_path(ok).is_ok(), "{ok} should pass");
        }
    }

    #[test]
    fn rejects_escape_attempts() {
        for bad in [
            "",                       // empty
            "..",                     // parent
            "a/../b",                 // interior parent
            "../escape",              // leading parent
            "/etc/passwd",            // absolute (Unix)
            "./here",                 // CurDir
            "a\\..\\..\\evil.exe",    // Windows backslash traversal
            "C:\\Users\\evil",        // drive prefix (push() resets dest)
            "\\\\?\\C:\\evil",        // UNC prefix
            "C:evil",                 // drive-relative
            "file.exe:evil.exe",      // NTFS alternate data stream
            "CON",                    // reserved device names
            "NUL.txt",
            "COM1",
            "$MFT",                   // NTFS metadata names
            "evil.",                  // trailing dot/space — Windows strips them
            "evil ",
        ] {
            assert!(validate_relative_path(bad).is_err(), "{bad} should be rejected");
        }
    }
}
