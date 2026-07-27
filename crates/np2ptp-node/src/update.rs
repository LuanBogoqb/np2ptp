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
//! Everything fails **closed**: a network error, an unparseable release, an
//! unparseable release *tag*, a non-GitHub download host, an oversized body, a
//! missing `SHA256SUMS`, an unsigned or mis-signed binary — all of them abort
//! the update, delete the download, and leave the running binary untouched.
//! There is deliberately no flag anywhere in this module that skips
//! verification.
//!
//! What is verified is the file **on disk** under `<exe>.new`, which is created
//! fresh (never through a symlink, never reusing an existing file) and fsynced
//! before the rename. See the `SECURITY:` comments on
//! `verified_signer_thumbprint`, `write_staging` and `swap_in` for the
//! remaining Windows time-of-check/time-of-use window and the open question
//! about `WTD_HASH_ONLY_FLAG`.

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

/// Hard ceiling on a single downloaded asset (256 MiB). np2ptp's own binary is
/// a few MiB; anything near this is either a mistake or someone feeding us an
/// endless body to eat the machine's RAM.
pub const MAX_DOWNLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Slack given to the update worker on top of its own timeout before the
/// caller stops waiting for it and returns [`UpdateError::Timeout`].
const WORKER_JOIN_GRACE: Duration = Duration::from_secs(10);

/// Hosts an update may be fetched from. Explicit list, no wildcards except the
/// `*.github.com` suffix rule in [`is_github_url`]: GitHub serves release
/// assets from `objects.githubusercontent.com` and (newer) from
/// `release-assets.githubusercontent.com`, and redirects there from
/// `github.com`. Everything else is refused.
const ALLOWED_HOSTS: &[&str] = &[
    "github.com",
    "www.github.com",
    "api.github.com",
    "codeload.github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

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
    #[error("release asset is larger than the {0} byte download cap")]
    TooLarge(u64),
    #[error("refusing to download an update from a non-GitHub host: {0}")]
    UntrustedHost(String),
    /// Both the swap-in rename *and* the rollback rename failed: the original
    /// binary is only reachable under its `.old` name. Loud on purpose.
    #[error(
        "UPDATE FAILED AND ROLLBACK FAILED: the working binary is now at {0} — \
         rename it back to the original name by hand before running np2ptp again"
    )]
    RollbackFailed(String),
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
///
/// Exactly **one** leading `v`/`V` is stripped: `trim_start_matches` would eat
/// every one of them and turn `vv1.2.3` into a tag that looks parseable.
fn normalize_tag(tag: &str) -> &str {
    let tag = tag.trim();
    let tag = tag
        .strip_prefix('v')
        .or_else(|| tag.strip_prefix('V'))
        .unwrap_or(tag);
    tag.trim()
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
///
/// SECURITY: an unparseable *latest* tag must be `false`, never `current !=
/// latest`. Tags like `nightly`, `v1.0` or `v0.1.8.1` would otherwise compare
/// unequal forever — an endless re-download loop — and would also accept an
/// arbitrarily old attached binary as an "update", since every past release
/// carries the same pinned signer and the pin cannot tell them apart.
pub fn needs_update(current: &str, latest_tag: &str) -> bool {
    let latest = normalize_tag(latest_tag);
    let current = current.trim();
    if latest.is_empty() {
        return false;
    }
    match (parse_version(current), parse_version(latest)) {
        // Both readable: only move forward, never accept a rollback.
        (Some(c), Some(n)) => n > c,
        // We can't read ourselves but the tag is a real version: take it.
        (None, Some(_)) => current != latest,
        // Unreadable tag: never act on a release we failed to understand.
        (_, None) => false,
    }
}

/// The release asset built for the platform this binary runs on.
#[allow(clippy::needless_lifetimes)] // the tie between input and output is the point
pub fn pick_asset<'a>(names: &'a [String]) -> Option<&'a str> {
    let want = if cfg!(windows) { WINDOWS_ASSET } else { LINUX_ASSET };
    names.iter().map(String::as_str).find(|n| *n == want)
}

/// Is this a URL we are willing to download an update from?
///
/// SECURITY: asset URLs come out of the release JSON, which is only as
/// trustworthy as whatever answered the API call. `https` only, no odd port,
/// and the host must be in [`ALLOWED_HOSTS`] or a `*.github.com` subdomain.
/// Userinfo tricks (`https://github.com@evil.example/`) parse with
/// `host_str() == "evil.example"` and are refused here.
pub fn is_github_url(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    if parsed.port().is_some_and(|p| p != 443) {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    // A trailing dot is the same name to a resolver, a different string here.
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    ALLOWED_HOSTS.contains(&host.as_str()) || host.ends_with(".github.com")
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
        // `sha256sum -b` writes "*name". SECURITY: the rest of the field is
        // compared verbatim — stripping a path prefix would let a line for
        // `attacker/np2ptp-linux-x86_64` stand in for the real asset.
        let name = name.strip_prefix('*').unwrap_or(name);
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
///
// SECURITY: OPEN QUESTION — `WTD_HASH_ONLY_FLAG`. It is not settled here
// whether the flag still verifies the PKCS#7 signature over the
// `SpcIndirectData` structure (i.e. the signed hash is authenticated by the
// signer key) or whether it only recomputes the PE image hash and compares it
// against the *unauthenticated* value in the attribute certificate table. If
// it is the latter, an attacker can rewrite the payload, recompute the hash,
// and leave the original certificate attached — the thumbprint pin below would
// still match. Negative fixtures (tampered-payload-with-original-cert,
// swapped-signer, stripped-signature) MUST pass before the Windows path is
// trusted in production. Do not treat this function as authenticating until
// then. Left as-is deliberately; tracked outside this module.
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
    // SECURITY: a non-zero return does not guarantee both out-params were
    // filled — a file with no embedded PKCS#7 can leave `msg` null, and
    // `CryptMsgGetParam(null)` is UB. Fail closed on either handle missing.
    if ok == 0 || msg.is_null() || store.is_null() {
        if !msg.is_null() {
            unsafe { CryptMsgClose(msg) };
        }
        if !store.is_null() {
            unsafe { CertCloseStore(store, 0) };
        }
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
    // SECURITY/UB: the buffer is written as a `CERT_INFO`, which contains
    // pointers — a `Vec<u8>` is only 1-byte aligned. Back it with `u64` so the
    // allocation is 8-byte aligned (over-aligned on 32-bit, which is fine).
    let words = (size as usize).div_ceil(8).max(1);
    let mut info = vec![0u64; words];
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
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("np2ptp-update".into())
        .spawn(move || {
            let result = (|| -> Result<UpdateReport, UpdateError> {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;
                rt.block_on(async move {
                    match tokio::time::timeout(timeout, run_update(timeout)).await {
                        Ok(result) => result,
                        Err(_) => {
                            // The cancelled future may have left `<exe>.new`
                            // staged; an unverified file never survives a run.
                            if let Ok(exe) = std::env::current_exe() {
                                let _ = fs::remove_file(staging_path(&exe));
                            }
                            Err(UpdateError::Timeout)
                        }
                    }
                })
            })();
            let _ = tx.send(result);
        })?;

    // `join()` cannot be interrupted, so `timeout` would not bound the call if
    // the worker wedged inside a blocking syscall. Wait on a channel with
    // headroom over the inner timeout and abandon the thread instead of
    // hanging the caller forever.
    match rx.recv_timeout(timeout.saturating_add(WORKER_JOIN_GRACE)) {
        Ok(result) => {
            let _ = worker.join();
            result
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(UpdateError::Timeout),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(UpdateError::Worker),
    }
}

async fn run_update(timeout: Duration) -> Result<UpdateReport, UpdateError> {
    let current = env!("CARGO_PKG_VERSION").to_string();

    let client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(timeout)
        // SECURITY: no plaintext fallback, and a bounded redirect chain. Every
        // URL is host-checked before it is requested, but a redirect can still
        // move the final hop, so keep the chain short.
        .https_only(true)
        .redirect(reqwest::redirect::Policy::limited(3))
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
    if !is_github_url(&asset_url) {
        return Err(UpdateError::UntrustedHost(asset_url));
    }

    let bytes = download_capped(&client, &asset_url).await?;
    if bytes.is_empty() {
        return Err(UpdateError::BadSignature);
    }

    let exe = std::env::current_exe()?;
    let download = staging_path(&exe);

    // The blocking half runs on the blocking pool so the surrounding
    // `tokio::time::timeout` can actually fire while it is in progress.
    {
        let path = download.clone();
        tokio::task::spawn_blocking(move || write_staging(&path, &bytes))
            .await
            .map_err(|_| UpdateError::Worker)??;
    }

    // From here on, every failure deletes the download before returning.
    let verified = verify_download(&client, &release, &download, &asset_name).await;
    if let Err(err) = verified {
        let _ = fs::remove_file(&download);
        return Err(err);
    }

    let swapped = {
        let (exe, download) = (exe.clone(), download.clone());
        tokio::task::spawn_blocking(move || swap_in(&exe, &download)).await
    };
    match swapped {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            // Leave the staged binary in place only when it is the user's way
            // back: a failed rollback means `<exe>` is gone.
            if !matches!(err, UpdateError::RollbackFailed(_)) {
                let _ = fs::remove_file(&download);
            }
            return Err(err);
        }
        Err(_) => {
            let _ = fs::remove_file(&download);
            return Err(UpdateError::Worker);
        }
    }

    Ok(UpdateReport {
        updated: true,
        from: current,
        to: latest,
    })
}

/// GET `url`, refusing bodies over [`MAX_DOWNLOAD_BYTES`].
///
/// SECURITY: `Content-Length` is a hint from the other side, so the declared
/// length is only an early exit — the streamed bytes are counted as they land
/// and the transfer is cut the moment it crosses the cap. `bytes()` would
/// happily buffer an endless body into RAM.
async fn download_capped(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, UpdateError> {
    let mut resp = client.get(url).send().await?.error_for_status()?;
    if resp.content_length().is_some_and(|n| n > MAX_DOWNLOAD_BYTES) {
        return Err(UpdateError::TooLarge(MAX_DOWNLOAD_BYTES));
    }
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if body.len() as u64 + chunk.len() as u64 > MAX_DOWNLOAD_BYTES {
            return Err(UpdateError::TooLarge(MAX_DOWNLOAD_BYTES));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `O_NOFOLLOW`, spelled out because `libc` is not a dependency of this crate.
/// Same value on Linux/Android for every arch the project targets; the BSD and
/// mips numbers are here so a port doesn't silently lose the flag.
#[cfg(unix)]
const O_NOFOLLOW: i32 = if cfg!(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "dragonfly"
)) {
    0x0100
} else if cfg!(any(target_arch = "mips", target_arch = "mips64")) {
    0o100000
} else {
    0o400000
};

/// `FILE_FLAG_OPEN_REPARSE_POINT`: open the reparse point itself instead of
/// following it.
#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// Write the download to `<exe>.new` and get it fully on disk.
///
/// SECURITY: three things at once.
/// * The unlink first means a leftover `.new` from a crashed or timed-out run
///   is never written into or partially reused.
/// * `create_new` + `O_NOFOLLOW` / `FILE_FLAG_OPEN_REPARSE_POINT` means a
///   symlink, junction or pre-created file planted next to the exe by another
///   user makes the open **fail** instead of redirecting our write into a file
///   they control (and whose mode/ACL they chose).
/// * `sync_all` before the rename: `rename` orders metadata, not data, so
///   without it a power cut can leave a correctly-named binary full of zeroes.
fn write_staging(download: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    use std::io::Write;

    match fs::remove_file(download) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        opts.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let mut file = opts.open(download)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

/// Windows: pinned Authenticode signer. Elsewhere: the release's `SHA256SUMS`.
/// Every failure path here is [`UpdateError::BadSignature`].
async fn verify_download(
    client: &reqwest::Client,
    release: &Release,
    download: &Path,
    asset_name: &str,
) -> Result<(), UpdateError> {
    #[cfg(windows)]
    {
        let _ = (client, release, asset_name);
        let path = download.to_path_buf();
        tokio::task::spawn_blocking(move || match verified_signer_thumbprint(&path) {
            Some(thumb) if hex_eq(&thumb, EXPECTED_SIGNER_THUMBPRINT) => Ok(()),
            _ => Err(UpdateError::BadSignature),
        })
        .await
        .map_err(|_| UpdateError::Worker)?
    }
    #[cfg(not(windows))]
    {
        let sums_url = release
            .assets
            .iter()
            .find(|a| a.name == SUMS_ASSET)
            .map(|a| a.browser_download_url.clone())
            // No checksums published means no way to verify: fail closed.
            .ok_or(UpdateError::BadSignature)?;
        if !is_github_url(&sums_url) {
            return Err(UpdateError::UntrustedHost(sums_url));
        }
        let sums = download_capped(client, &sums_url)
            .await
            .map_err(|_| UpdateError::BadSignature)?;
        let sums = String::from_utf8_lossy(&sums).into_owned();

        // SECURITY: hash the bytes that are actually on disk, not the in-memory
        // buffer they were written from. A full HTTP round trip (this
        // `SHA256SUMS` fetch) sits between the write and this check, which is
        // all the time anyone with write access next to the exe needs to
        // replace `<exe>.new`. Verifying the buffer would bless a file nobody
        // ever looked at.
        let path = download.to_path_buf();
        let on_disk = tokio::task::spawn_blocking(move || fs::read(&path))
            .await
            .map_err(|_| UpdateError::Worker)??;
        verify_sha256(&on_disk, &sums, asset_name)
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
    #[cfg(windows)]
    {
        // SECURITY: TOCTOU mitigation. The verification above named a *path*
        // and this rename names that path again — nothing ties the two calls to
        // the same file, so anyone able to write next to the exe can swap
        // `<exe>.new` in between. Re-running the pin check immediately before
        // the rename shrinks the window from "one HTTP round trip" to
        // microseconds, at the cost of one cheap local verify. Closing it
        // outright needs the *verified handle* to be the thing renamed, which
        // `WinVerifyTrust` (path-based) plus `SetFileInformationByHandle`
        // rename-by-handle would have to be plumbed for — follow-up, not here.
        match verified_signer_thumbprint(download) {
            Some(thumb) if hex_eq(&thumb, EXPECTED_SIGNER_THUMBPRINT) => {}
            _ => return Err(UpdateError::BadSignature),
        }
    }
    #[cfg(unix)]
    {
        // Reapply the *current* binary's mode rather than a hardcoded 0o755: a
        // deployment that ships the binary 0o700, setgid, or owned by a service
        // account must not be quietly widened by an update.
        let mode = fs::metadata(exe)?.permissions();
        fs::set_permissions(download, mode)?;
    }

    let old = old_path(exe);
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)?;
    if let Err(err) = fs::rename(download, exe) {
        if fs::rename(&old, exe).is_err() {
            // Both renames failed: `<exe>` no longer exists and the only
            // working binary is the `.old` copy. Say so, loudly, by name.
            return Err(UpdateError::RollbackFailed(old.display().to_string()));
        }
        let _ = fs::remove_file(download);
        return Err(UpdateError::Io(err));
    }
    Ok(())
}

/// Best-effort delete of the leftovers an update can strand next to the exe:
/// the previous binary (`.old`) and an unverified staged download (`.new`,
/// possible after a crash or a timeout). Safe to call at every startup; errors
/// (still locked, already gone) are ignored.
pub fn cleanup_old_binary() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = fs::remove_file(old_path(&exe));
        // Nothing in this process verified that file, so it does not get to
        // stay and be reused.
        let _ = fs::remove_file(staging_path(&exe));
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
        // Older tag: no downgrade, ever.
        assert!(!needs_update("0.1.9", "v0.1.8"));
        assert!(!needs_update("2.0.0", "v1.9.9"));
        assert!(!needs_update("0.1.8", ""));
        assert!(!needs_update("0.1.8", "v"));
    }

    #[test]
    fn unparseable_latest_tag_is_never_an_update() {
        // Each of these used to return `current != latest` => endless
        // re-download loop, and a silent downgrade to whatever binary the
        // release carried (same signer, so the pin cannot object).
        for tag in ["nightly", "v1.0", "v0.1.8.1", "latest", "v2026-07-27", "release"] {
            assert!(!needs_update("0.1.8", tag), "tag {tag} must not update");
            assert!(!needs_update("", tag), "tag {tag} must not update");
        }
        // Newer, readable tag still updates.
        assert!(needs_update("0.1.8", "v0.2.0"));
        assert!(needs_update("0.1.8", "v0.1.9-rc1"));
        // Unreadable current + readable tag: take it.
        assert!(needs_update("dev", "v0.1.9"));
    }

    #[test]
    fn normalize_tag_strips_exactly_one_v() {
        assert_eq!(normalize_tag(" v0.1.8 "), "0.1.8");
        assert_eq!(normalize_tag("V0.1.8"), "0.1.8");
        assert_eq!(normalize_tag("vv0.1.8"), "v0.1.8");
        assert!(!needs_update("0.1.8", "vv0.1.9"));
    }

    #[test]
    fn only_github_hosts_are_downloadable() {
        assert!(is_github_url(
            "https://objects.githubusercontent.com/x/np2ptp-linux-x86_64"
        ));
        assert!(is_github_url(
            "https://release-assets.githubusercontent.com/x/asset"
        ));
        assert!(is_github_url("https://api.github.com/repos/a/b"));
        assert!(is_github_url("https://uploads.github.com/asset"));
        assert!(is_github_url("https://github.com./a/b")); // trailing dot

        assert!(!is_github_url("http://github.com/a/b")); // plaintext
        assert!(!is_github_url("https://github.com.evil.example/a")); // suffix trick
        assert!(!is_github_url("https://github.com@evil.example/a")); // userinfo trick
        assert!(!is_github_url("https://evil.example/np2ptp-linux-x86_64"));
        assert!(!is_github_url("https://raw.githubusercontent.com/a/b")); // not a release host
        assert!(!is_github_url("https://github.com:8443/a/b")); // odd port
        assert!(!is_github_url("file:///tmp/np2ptp"));
        assert!(!is_github_url("not a url"));
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
    fn sums_filename_is_compared_verbatim() {
        let bytes = b"payload";
        let name = "np2ptp-linux-x86_64";
        // A line for a *different* path that merely ends in the asset name must
        // not stand in for the asset's own line.
        let sums = format!("{}  attacker/{}\n", sha256_hex(bytes), name);
        assert!(matches!(
            verify_sha256(bytes, &sums, name),
            Err(UpdateError::BadSignature)
        ));
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
