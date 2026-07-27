//! Self-update: fetch the latest GitHub release, verify it, swap the binary.
//!
//! The trust decision here is **pinning**, not the OS trust store:
//!
//! * **Windows** — the downloaded `.exe` must carry an Authenticode signature
//!   whose hash still matches the file's bytes *and* whose signer certificate's
//!   SHA-1 thumbprint equals [`EXPECTED_SIGNER_THUMBPRINT`]. np2ptp is signed
//!   with a self-signed certificate, so chain-of-trust validation is skipped
//!   (`WTD_HASH_ONLY_FLAG`) — otherwise every machine that hasn't imported that
//!   certificate rejects a genuinely untampered file. Ported from np2ptp-gui's
//!   `AuthenticodeVerifier`/`BinaryManager` pair, same call sequence.
//! * **Linux** — SHA-256 of the download must match the entry for that asset in
//!   the release's `SHA256SUMS`.
//!
//! Everything fails **closed**: a network error, an unparseable release, a
//! missing `SHA256SUMS`, an unsigned or mis-signed binary — all of them abort
//! the update, delete the download, and leave the running binary untouched.
//! There is deliberately no flag anywhere in this module that skips
//! verification.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// Signer certificate pinned for Windows builds (SHA-1 thumbprint, hex).
pub const EXPECTED_SIGNER_THUMBPRINT: &str = "36477BB5DCB10D2C0381A2D79533F0386C5CCACA";

/// GitHub API endpoint for the newest published release.
pub const LATEST_RELEASE_URL: &str =
    "https://api.github.com/repos/LuanBogoqb/np2ptp/releases/latest";

/// GitHub rejects API requests without one.
pub const USER_AGENT: &str = concat!("np2ptp-selfupdate/", env!("CARGO_PKG_VERSION"));

pub const WINDOWS_ASSET: &str = "np2ptp-windows-x86_64.exe";
pub const LINUX_ASSET: &str = "np2ptp-linux-x86_64";
pub const SUMS_ASSET: &str = "SHA256SUMS";

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("update check failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("unexpected release data: {0}")]
    Parse(String),
    #[error("the latest release has no asset for this platform")]
    NoAsset,
    #[error("update timed out")]
    Timeout,
    /// Verification failed for *any* reason. The download has been deleted and
    /// the current binary is untouched.
    #[error("downloaded binary failed verification against the pinned signer")]
    BadSignature,
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("update worker failed")]
    Worker,
}

/// What an update run actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateReport {
    pub updated: bool,
    pub from: String,
    pub to: String,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

// ---------------------------------------------------------------- pure parts

/// Release tags are `v0.1.8`; `CARGO_PKG_VERSION` is `0.1.8`.
fn normalize_tag(tag: &str) -> &str {
    tag.trim().trim_start_matches(['v', 'V']).trim()
}

/// `1.2.3-rc1` -> `(1, 2, 3)`. `None` if it isn't three numbers.
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// Should `current` be replaced by the release tagged `latest_tag`?
///
/// A current version we can't read means "update" — a binary that can't tell
/// you what it is shouldn't get to opt out. An unreadable *tag* means "don't":
/// we never act on a release we failed to understand.
pub fn needs_update(current: &str, latest_tag: &str) -> bool {
    let latest = normalize_tag(latest_tag);
    let current = current.trim();
    if latest.is_empty() {
        return false;
    }
    match (parse_version(current), parse_version(latest)) {
        // Both readable: only move forward, never accept a rollback.
        (Some(cur), Some(new)) => new > cur,
        // Anything unreadable on our side: any different tag is an update.
        _ => current != latest,
    }
}

/// The release asset built for the platform this binary runs on.
#[allow(clippy::needless_lifetimes)] // the tie between input and output is the point
pub fn pick_asset<'a>(names: &'a [String]) -> Option<&'a str> {
    let want = if cfg!(windows) { WINDOWS_ASSET } else { LINUX_ASSET };
    names.iter().map(String::as_str).find(|n| *n == want)
}

/// Case-insensitive hex compare that always looks at every byte.
fn hex_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x.to_ascii_lowercase() ^ y.to_ascii_lowercase();
    }
    diff == 0
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Check `bytes` against the `<hex>  <name>` line for `asset_name` in a
/// `SHA256SUMS` body.
///
/// Fails closed: no matching line, a malformed line, or several lines that
/// disagree are all [`UpdateError::BadSignature`], same as a hash mismatch.
pub fn verify_sha256(bytes: &[u8], sums_body: &str, asset_name: &str) -> Result<(), UpdateError> {
    let actual = sha256_hex(bytes);
    let mut seen = false;
    for line in sums_body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(expected), Some(name)) = (fields.next(), fields.next()) else {
            continue;
        };
        // `sha256sum -b` writes "*name"; some releases carry a path prefix.
        let name = name.trim_start_matches('*');
        let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
        if name != asset_name {
            continue;
        }
        if !hex_eq(expected, &actual) {
            return Err(UpdateError::BadSignature);
        }
        seen = true;
    }
    if seen {
        Ok(())
    } else {
        Err(UpdateError::BadSignature)
    }
}

// ----------------------------------------------------------- Windows pinning

/// The signer certificate's SHA-1 thumbprint, but only if the file's
/// Authenticode signature still matches its bytes. `None` for unsigned or
/// tampered files.
///
/// `WTD_HASH_ONLY_FLAG` makes `WinVerifyTrust` check signature/hash integrity
/// and skip CA chain validation — the pinned thumbprint below is the trust
/// decision, and the self-signed cert would otherwise fail on every machine
/// that hasn't imported it. Reading the embedded certificate *without* the
/// `WinVerifyTrust` call would be worthless: it returns metadata even for a
/// file whose content no longer matches the signature.
#[cfg(windows)]
fn verified_signer_thumbprint(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    use windows_sys::Win32::Security::WinTrust::{
        WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_DATA_0,
        WINTRUST_FILE_INFO, WTD_CHOICE_FILE, WTD_HASH_ONLY_FLAG, WTD_REVOKE_NONE,
        WTD_STATEACTION_CLOSE, WTD_STATEACTION_VERIFY, WTD_UI_NONE,
    };

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut file_info = WINTRUST_FILE_INFO {
        cbStruct: std::mem::size_of::<WINTRUST_FILE_INFO>() as u32,
        pcwszFilePath: wide.as_ptr(),
        hFile: null_mut(),
        pgKnownSubject: null_mut(),
    };
    let mut data = WINTRUST_DATA {
        cbStruct: std::mem::size_of::<WINTRUST_DATA>() as u32,
        pPolicyCallbackData: null_mut(),
        pSIPClientData: null_mut(),
        dwUIChoice: WTD_UI_NONE,
        fdwRevocationChecks: WTD_REVOKE_NONE,
        dwUnionChoice: WTD_CHOICE_FILE,
        Anonymous: WINTRUST_DATA_0 {
            pFile: &mut file_info,
        },
        dwStateAction: WTD_STATEACTION_VERIFY,
        hWVTStateData: null_mut(),
        pwszURLReference: null_mut(),
        dwProvFlags: WTD_HASH_ONLY_FLAG,
        dwUIContext: 0,
        pSignatureSettings: null_mut(),
    };
    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;

    let status = unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        )
    };
    // The verify call allocates state that only STATEACTION_CLOSE frees, so
    // close before reacting to `status`.
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast(),
        );
    }
    if status != 0 {
        return None;
    }

    signer_thumbprint(&wide)
}

/// The `X509Certificate.CreateFromSignedFile` half of the C# original: pull the
/// signer cert out of the embedded PKCS#7 blob and read its SHA-1 thumbprint.
#[cfg(windows)]
fn signer_thumbprint(wide_path: &[u16]) -> Option<String> {
    use std::ptr::null_mut;

    use windows_sys::Win32::Security::Cryptography::{
        CertCloseStore, CryptMsgClose, CryptQueryObject, HCERTSTORE,
        CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED, CERT_QUERY_FORMAT_FLAG_BINARY,
        CERT_QUERY_OBJECT_FILE,
    };

    let mut store: HCERTSTORE = null_mut();
    let mut msg: *mut core::ffi::c_void = null_mut();
    let ok = unsafe {
        CryptQueryObject(
            CERT_QUERY_OBJECT_FILE,
            wide_path.as_ptr().cast(),
            CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
            CERT_QUERY_FORMAT_FLAG_BINARY,
            0,
            null_mut(),
            null_mut(),
            null_mut(),
            &mut store,
            &mut msg,
            null_mut(),
        )
    };
    if ok == 0 {
        return None;
    }

    let out = signer_thumbprint_from_msg(store, msg);

    unsafe {
        CryptMsgClose(msg);
        CertCloseStore(store, 0);
    }
    out
}

#[cfg(windows)]
fn signer_thumbprint_from_msg(
    store: windows_sys::Win32::Security::Cryptography::HCERTSTORE,
    msg: *mut core::ffi::c_void,
) -> Option<String> {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Security::Cryptography::{
        CertFindCertificateInStore, CertFreeCertificateContext,
        CertGetCertificateContextProperty, CryptMsgGetParam, CERT_FIND_SUBJECT_CERT,
        CERT_SHA1_HASH_PROP_ID, CMSG_SIGNER_CERT_INFO_PARAM, PKCS_7_ASN_ENCODING,
        X509_ASN_ENCODING,
    };

    let mut size: u32 = 0;
    let sized = unsafe { CryptMsgGetParam(msg, CMSG_SIGNER_CERT_INFO_PARAM, 0, null_mut(), &mut size) };
    if sized == 0 || size == 0 {
        return None;
    }
    let mut info = vec![0u8; size as usize];
    let got = unsafe {
        CryptMsgGetParam(
            msg,
            CMSG_SIGNER_CERT_INFO_PARAM,
            0,
            info.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if got == 0 {
        return None;
    }

    let cert = unsafe {
        CertFindCertificateInStore(
            store,
            X509_ASN_ENCODING | PKCS_7_ASN_ENCODING,
            0,
            CERT_FIND_SUBJECT_CERT,
            info.as_ptr().cast(),
            null(),
        )
    };
    if cert.is_null() {
        return None;
    }

    let mut hash = [0u8; 20];
    let mut hash_len = hash.len() as u32;
    let got = unsafe {
        CertGetCertificateContextProperty(
            cert,
            CERT_SHA1_HASH_PROP_ID,
            hash.as_mut_ptr().cast(),
            &mut hash_len,
        )
    };
    unsafe { CertFreeCertificateContext(cert) };
    if got == 0 || hash_len as usize != hash.len() {
        return None;
    }
    Some(hash.iter().map(|b| format!("{b:02X}")).collect())
}

// ------------------------------------------------------------ effectful part

/// Check GitHub for a newer release and, if there is one, install it.
///
/// `timeout` bounds the whole operation (metadata + download + verification).
/// On success with `updated == true` the new binary is in place and the old one
/// is next to it as `<exe>.old`; the running process image is never touched, so
/// the update takes effect on the next start.
pub fn check_and_update(timeout: Duration) -> Result<UpdateReport, UpdateError> {
    // A dedicated thread with its own runtime: callers may already be inside a
    // tokio runtime, where `block_on` would panic.
    let worker = std::thread::Builder::new()
        .name("np2ptp-update".into())
        .spawn(move || -> Result<UpdateReport, UpdateError> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async move {
                match tokio::time::timeout(timeout, run_update(timeout)).await {
                    Ok(result) => result,
                    Err(_) => Err(UpdateError::Timeout),
                }
            })
        })?;
    worker.join().map_err(|_| UpdateError::Worker)?
}

async fn run_update(timeout: Duration) -> Result<UpdateReport, UpdateError> {
    let current = env!("CARGO_PKG_VERSION").to_string();

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        .build()?;

    let release: Release = client
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let latest = normalize_tag(&release.tag_name).to_string();
    if latest.is_empty() {
        return Err(UpdateError::Parse("release has an empty tag".into()));
    }
    if !needs_update(&current, &release.tag_name) {
        return Ok(UpdateReport {
            updated: false,
            from: current,
            to: latest,
        });
    }

    let names: Vec<String> = release.assets.iter().map(|a| a.name.clone()).collect();
    let asset_name = pick_asset(&names).ok_or(UpdateError::NoAsset)?.to_string();
    let asset_url = release
        .assets
        .iter()
        .find(|a| a.name == asset_name)
        .map(|a| a.browser_download_url.clone())
        .ok_or(UpdateError::NoAsset)?;

    let bytes = client
        .get(&asset_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    if bytes.is_empty() {
        return Err(UpdateError::BadSignature);
    }

    let exe = std::env::current_exe()?;
    let download = staging_path(&exe);
    fs::write(&download, &bytes)?;

    // From here on, every failure deletes the download before returning.
    let verified = verify_download(&client, &release, &download, &bytes, &asset_name).await;
    if let Err(err) = verified {
        let _ = fs::remove_file(&download);
        return Err(err);
    }

    swap_in(&exe, &download)?;

    Ok(UpdateReport {
        updated: true,
        from: current,
        to: latest,
    })
}

/// Windows: pinned Authenticode signer. Elsewhere: the release's `SHA256SUMS`.
/// Every failure path here is [`UpdateError::BadSignature`].
async fn verify_download(
    client: &reqwest::Client,
    release: &Release,
    download: &Path,
    bytes: &[u8],
    asset_name: &str,
) -> Result<(), UpdateError> {
    #[cfg(windows)]
    {
        let _ = (client, release, bytes, asset_name);
        match verified_signer_thumbprint(download) {
            Some(thumb) if hex_eq(&thumb, EXPECTED_SIGNER_THUMBPRINT) => Ok(()),
            _ => Err(UpdateError::BadSignature),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = download;
        let sums_url = release
            .assets
            .iter()
            .find(|a| a.name == SUMS_ASSET)
            .map(|a| a.browser_download_url.clone())
            // No checksums published means no way to verify: fail closed.
            .ok_or(UpdateError::BadSignature)?;
        let sums = client
            .get(&sums_url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|_| UpdateError::BadSignature)?
            .text()
            .await
            .map_err(|_| UpdateError::BadSignature)?;
        verify_sha256(bytes, &sums, asset_name)
    }
}

fn staging_path(exe: &Path) -> PathBuf {
    let mut p = exe.as_os_str().to_os_string();
    p.push(".new");
    PathBuf::from(p)
}

fn old_path(exe: &Path) -> PathBuf {
    let mut p = exe.as_os_str().to_os_string();
    p.push(".old");
    PathBuf::from(p)
}

/// Move the verified download into place: rename the running exe out of the way
/// (Windows allows renaming a running image, not overwriting it), then rename
/// the download in. If the second rename fails, put the original back.
fn swap_in(exe: &Path, download: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(download, fs::Permissions::from_mode(0o755))?;
    }

    let old = old_path(exe);
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)?;
    if let Err(err) = fs::rename(download, exe) {
        let _ = fs::rename(&old, exe);
        let _ = fs::remove_file(download);
        return Err(UpdateError::Io(err));
    }
    Ok(())
}

/// Best-effort delete of the previous binary left behind by an update. Safe to
/// call at every startup; errors (still locked, already gone) are ignored.
pub fn cleanup_old_binary() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(old_path(&exe));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn needs_update_semantics() {
        assert!(!needs_update("0.1.8", "v0.1.8"));
        assert!(needs_update("0.1.8", "v0.1.9"));
        assert!(needs_update("", "v0.1.9")); // unreadable current version
    }

    #[test]
    fn never_downgrades_or_acts_on_an_unreadable_tag() {
        assert!(!needs_update("0.1.9", "v0.1.8"));
        assert!(!needs_update("0.1.8", ""));
        assert!(!needs_update("0.1.8", "v"));
    }

    #[test]
    fn picks_platform_asset() {
        let names = vec![
            "SHA256SUMS".into(),
            "np2ptp-windows-x86_64.exe".into(),
            "np2ptp-linux-x86_64".into(),
        ];
        #[cfg(windows)]
        assert_eq!(pick_asset(&names), Some("np2ptp-windows-x86_64.exe"));
        #[cfg(not(windows))]
        assert_eq!(pick_asset(&names), Some("np2ptp-linux-x86_64"));
    }

    #[test]
    fn no_asset_for_this_platform() {
        let names = vec!["SHA256SUMS".into(), "np2ptp-macos-arm64".into()];
        assert_eq!(pick_asset(&names), None);
    }

    #[test]
    fn sha256sums_verification() {
        let bytes = b"np2ptp release payload";
        let name = "np2ptp-linux-x86_64";
        let sums = format!(
            "{}  SHA256SUMS.other\n{}  {}\n",
            sha256_hex(b"unrelated"),
            sha256_hex(bytes),
            name
        );

        assert!(verify_sha256(bytes, &sums, name).is_ok());
        assert!(matches!(
            verify_sha256(b"np2ptp release payloa\x00", &sums, name),
            Err(UpdateError::BadSignature)
        ));
    }

    #[test]
    fn sha256sums_missing_entry_fails_closed() {
        let bytes = b"payload";
        let sums = format!("{}  some-other-file\n", sha256_hex(bytes));
        assert!(matches!(
            verify_sha256(bytes, &sums, "np2ptp-linux-x86_64"),
            Err(UpdateError::BadSignature)
        ));
        assert!(matches!(
            verify_sha256(bytes, "", "np2ptp-linux-x86_64"),
            Err(UpdateError::BadSignature)
        ));
        assert!(matches!(
            verify_sha256(bytes, "garbage line without a hash", "np2ptp-linux-x86_64"),
            Err(UpdateError::BadSignature)
        ));
    }

    #[test]
    fn sha256sums_accepts_binary_mode_and_uppercase() {
        let bytes = b"payload";
        let name = "np2ptp-linux-x86_64";
        let sums = format!("{}  *{}\n", sha256_hex(bytes).to_uppercase(), name);
        assert!(verify_sha256(bytes, &sums, name).is_ok());
    }

    #[test]
    fn conflicting_sums_entries_fail_closed() {
        let bytes = b"payload";
        let name = "np2ptp-linux-x86_64";
        let sums = format!(
            "{}  {}\n{}  {}\n",
            sha256_hex(bytes),
            name,
            sha256_hex(b"something else"),
            name
        );
        assert!(matches!(
            verify_sha256(bytes, &sums, name),
            Err(UpdateError::BadSignature)
        ));
    }

    #[test]
    fn thumbprint_compare_is_case_insensitive_but_exact() {
        assert!(hex_eq(
            EXPECTED_SIGNER_THUMBPRINT,
            &EXPECTED_SIGNER_THUMBPRINT.to_lowercase()
        ));
        assert!(!hex_eq(EXPECTED_SIGNER_THUMBPRINT, ""));
        assert!(!hex_eq(
            EXPECTED_SIGNER_THUMBPRINT,
            "36477BB5DCB10D2C0381A2D79533F0386C5CCAC9"
        ));
        // A prefix must not pass.
        assert!(!hex_eq(
            EXPECTED_SIGNER_THUMBPRINT,
            &EXPECTED_SIGNER_THUMBPRINT[..10]
        ));
    }

    #[test]
    fn staging_and_old_paths_sit_next_to_the_exe() {
        let exe = Path::new("C:/tools/np2ptp.exe");
        assert!(staging_path(exe).ends_with("np2ptp.exe.new"));
        assert!(old_path(exe).ends_with("np2ptp.exe.old"));
    }
}
