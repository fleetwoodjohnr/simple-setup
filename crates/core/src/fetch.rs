//! Network access: catalog lookups and installer downloads through the system's `curl`, HTTPS
//! only. Used while a snapshot is built, by restores for apps the snapshot holds no installer
//! for (unless the restore is offline), and by the updater. Every download is checked against a
//! published SHA-256 by its caller.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::build::hash_file;
use crate::installers::run;
use crate::{Result, invalid};

fn curl(url: &str) -> Result<Command> {
    if !url.starts_with("https://") || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid(format!(
            "Refusing to download from {url:?}: not a secure link."
        )));
    }
    let mut c = Command::new("curl");
    // `-q` ignores ~/.curlrc so downloads behave the same everywhere; redirects stay on HTTPS.
    c.args([
        "-q",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--retry",
        "3",
        "--connect-timeout",
        "30",
        // A connection that stalls (under 1 kB/s for a minute) is given up, not waited on forever.
        "--speed-limit",
        "1000",
        "--speed-time",
        "60",
        "--user-agent",
        concat!("simple-setup/", env!("CARGO_PKG_VERSION")),
    ]);
    Ok(c)
}

pub(crate) const GITHUB_API: &str = "https://api.github.com/";

/// A GitHub token raises the API limit from 60 to 5,000 requests an hour: `GITHUB_TOKEN` or
/// `GH_TOKEN`, else the token of the GitHub CLI when it is signed in (asked once). It goes to
/// curl in a private header file, never on the command line (where other users of the
/// computer could read it), and only to `api.github.com`.
struct TokenHeader(PathBuf);

/// A token the desktop app read from the system keyring (see [`set_github_token`]).
static APP_TOKEN: Mutex<Option<String>> = Mutex::new(None);

/// Uses `token` for GitHub (the desktop app passes the one stored in the system keyring);
/// `None` goes back to `GITHUB_TOKEN`/`GH_TOKEN` or the GitHub CLI. Kept in memory only.
pub fn set_github_token(token: Option<String>) {
    if let Ok(mut t) = APP_TOKEN.lock() {
        *t = token.filter(|t| !t.trim().is_empty());
    }
}

/// Whether requests to GitHub carry a token (for messages; the token itself is never shown).
pub fn has_github_token() -> bool {
    github_token().is_some()
}

fn github_token() -> Option<String> {
    static GH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    let token = APP_TOKEN
        .lock()
        .ok()
        .and_then(|t| t.clone())
        .or_else(|| {
            ["GITHUB_TOKEN", "GH_TOKEN"]
                .iter()
                .find_map(|k| std::env::var(k).ok().filter(|t| !t.trim().is_empty()))
        })
        .or_else(|| {
            GH.get_or_init(|| {
                crate::installers::which("gh")?;
                let out = Command::new("gh")
                    .args(["auth", "token", "--hostname", "github.com"])
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .output()
                    .ok()?;
                out.status
                    .success()
                    .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
            })
            .clone()
        })?;
    let token = token.trim();
    (!token.is_empty() && token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .then(|| token.to_string())
}

impl TokenHeader {
    fn new(url: &str) -> Option<Self> {
        if !url.starts_with(GITHUB_API) {
            return None;
        }
        let token = github_token()?;
        let path = std::env::temp_dir().join(format!(
            "simple-setup-{}-{}.hdr",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let mut o = fs::OpenOptions::new();
        o.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
        let mut f = o.open(&path).ok()?;
        std::io::Write::write_all(
            &mut f,
            format!("Authorization: Bearer {token}\n").as_bytes(),
        )
        .ok()?;
        Some(Self(path))
    }
}

impl Drop for TokenHeader {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// GitHub API answers seen in the last few minutes, so one restore or update check asks once.
static CACHE: Mutex<Option<HashMap<String, (Instant, String)>>> = Mutex::new(None);
const CACHE_FOR: Duration = Duration::from_secs(600);

/// A small text answer (catalog entries, API listings).
pub fn text(url: &str, accept: Option<&str>, cancel: &AtomicBool) -> Result<String> {
    let cached = url.starts_with(GITHUB_API);
    if cached
        && let Some((at, body)) = CACHE
            .lock()
            .ok()
            .and_then(|c| c.as_ref()?.get(url).cloned())
        && at.elapsed() < CACHE_FOR
    {
        return Ok(body);
    }
    let mut c = curl(url)?;
    if let Some(a) = accept {
        c.arg("--header").arg(format!("Accept: {a}"));
    }
    let token = TokenHeader::new(url);
    if let Some(t) = &token {
        c.arg("--header").arg(format!("@{}", t.0.display()));
    }
    // GitHub answers a request with the ETag of an earlier answer with "304 Not Modified",
    // which does not count against the hourly limit; the earlier answer is kept on disk.
    let disk = (cached && curl_has_etag())
        .then(|| disk_cache(url))
        .flatten();
    let body_file = disk
        .as_ref()
        .map(|(dir, key)| dir.join(format!("{key}.{}.tmp", uuid::Uuid::new_v4())));
    if let (Some((dir, key)), Some(tmp)) = (&disk, &body_file) {
        let etag = dir.join(format!("{key}.etag"));
        if etag.is_file() && dir.join(format!("{key}.body")).is_file() {
            c.arg("--etag-compare").arg(&etag);
        }
        c.arg("--etag-save").arg(&etag);
        c.arg("--output").arg(tmp);
        c.args(["--write-out", "%{http_code}"]);
    }
    c.args(["--max-filesize", "20000000"]).arg(url);
    let out = run(&mut c, cancel).map_err(|e| match e {
        crate::Error::Cancelled => e,
        e if url.starts_with(GITHUB_API) && rate_limited(&e.to_string()) => {
            invalid(github_limit_message(token.is_some(), cancel))
        }
        e => invalid(format!("Looking up {url} failed: {e}")),
    });
    let body = match (&disk, &body_file) {
        (Some((dir, key)), Some(tmp)) => {
            let fresh = fs::read_to_string(tmp).ok();
            let _ = fs::remove_file(tmp);
            let stored = dir.join(format!("{key}.body"));
            match (out?.trim(), fresh) {
                ("304", _) => fs::read_to_string(&stored).map_err(|e| {
                    invalid(format!("Reading the saved answer for {url} failed: {e}"))
                })?,
                (_, Some(b)) => {
                    let _ = fs::write(&stored, &b);
                    b
                }
                (code, None) => {
                    return Err(invalid(format!(
                        "Looking up {url} failed: no answer (HTTP {code})."
                    )));
                }
            }
        }
        _ => out?,
    };
    if cached && let Ok(mut c) = CACHE.lock() {
        c.get_or_insert_with(HashMap::new)
            .insert(url.to_string(), (Instant::now(), body.clone()));
    }
    Ok(body)
}

/// curl 7.68 or later (`--etag-save`/`--etag-compare`); older ones (some Windows 10 builds)
/// simply ask again each time.
fn curl_has_etag() -> bool {
    static HAS: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *HAS.get_or_init(|| {
        let out = Command::new("curl").arg("--version").output().ok();
        let v = out
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        v.split_whitespace()
            .nth(1)
            .is_some_and(|v| crate::catalog::version_cmp(v, "7.68.0").is_ge())
    })
}

/// The folder and file name stem of the on-disk answer for `url` (GitHub API only).
fn disk_cache(url: &str) -> Option<(PathBuf, String)> {
    use sha2::{Digest, Sha256};
    let dir = dirs::cache_dir()?.join("simple-setup").join("github");
    fs::create_dir_all(&dir).ok()?;
    let key = format!("{:x}", Sha256::digest(url.as_bytes()));
    Some((dir, key[..32].to_string()))
}

fn rate_limited(curl_error: &str) -> bool {
    curl_error.contains("error: 403") || curl_error.contains("error: 429")
}

/// Explains GitHub's request limit, with the time it resets (asking costs no request).
fn github_limit_message(had_token: bool, cancel: &AtomicBool) -> String {
    #[derive(serde::Deserialize)]
    struct Core {
        limit: u64,
        remaining: u64,
        reset: u64,
    }
    #[derive(serde::Deserialize)]
    struct Resources {
        core: Core,
    }
    #[derive(serde::Deserialize)]
    struct Limits {
        resources: Resources,
    }
    let limits = curl(&format!("{GITHUB_API}rate_limit"))
        .and_then(|mut c| run(c.arg(format!("{GITHUB_API}rate_limit")), cancel))
        .ok()
        .and_then(|t| serde_json::from_str::<Limits>(&t).ok());
    limit_message(
        limits.map(|l| {
            (
                l.resources.core.limit,
                l.resources.core.remaining,
                l.resources.core.reset,
            )
        }),
        had_token,
    )
}

pub(crate) fn limit_message(core: Option<(u64, u64, u64)>, had_token: bool) -> String {
    let hint = if had_token {
        ""
    } else {
        " To raise the limit, set GITHUB_TOKEN to a GitHub token (no permissions needed) and try again."
    };
    match core {
        Some((limit, 0, reset)) => {
            let at = humantime::format_rfc3339_seconds(
                std::time::UNIX_EPOCH + Duration::from_secs(reset),
            );
            format!(
                "GitHub allows {limit} requests an hour from this network, and they are used up until {at} (UTC). Try again after that.{hint}"
            )
        }
        // Requests are left, so this refusal is about access (a private repository), or a
        // short-lived secondary limit.
        Some(_) => format!(
            "GitHub refused access. If the repository is private, add a GitHub token that can read it; otherwise GitHub is briefly limiting requests, so try again in a minute.{hint}"
        ),
        None => format!("GitHub refused the request (it may be busy or limiting requests).{hint}"),
    }
}

/// Whether GitHub can be reached within a few seconds (a plain connection, so it works even
/// where `curl` is not installed yet).
pub fn online() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    ("api.github.com", 443)
        .to_socket_addrs()
        .is_ok_and(|mut addrs| {
            addrs.any(|a| TcpStream::connect_timeout(&a, Duration::from_secs(5)).is_ok())
        })
}

pub fn json<T: DeserializeOwned>(
    url: &str,
    accept: Option<&str>,
    cancel: &AtomicBool,
) -> Result<T> {
    serde_json::from_str(&text(url, accept, cancel)?)
        .map_err(|e| invalid(format!("Unexpected answer from {url}: {e}")))
}

/// Downloads `url` to `dest` and checks it against `sha256`; a file that does not match is
/// deleted. `None` is only for catalogs that publish no checksum: the caller then pins the
/// file's own SHA-256. Returns the size.
pub fn download(url: &str, dest: &Path, sha256: Option<&str>, cancel: &AtomicBool) -> Result<u64> {
    download_with(url, dest, sha256, cancel, None)
}

/// [`download`] of a GitHub release asset through its API address, with the token: the way
/// to download from a private repository.
pub fn download_asset(
    api_url: &str,
    dest: &Path,
    sha256: Option<&str>,
    cancel: &AtomicBool,
) -> Result<u64> {
    if !api_url.starts_with(GITHUB_API) {
        return Err(invalid(format!("{api_url} is not a GitHub API address.")));
    }
    download_with(
        api_url,
        dest,
        sha256,
        cancel,
        Some("application/octet-stream"),
    )
}

/// Verified installer downloads, kept by SHA-256 so another snapshot or restore that needs the
/// same file takes it from here instead of the network.
fn installer_cache() -> Option<PathBuf> {
    let d = dirs::cache_dir()?.join("simple-setup").join("installers");
    fs::create_dir_all(&d).ok()?;
    Some(d)
}

// ponytail: the cache is trimmed to its newest files by modification time; per-snapshot
// pinning if people need older installers kept.
const CACHE_LIMIT: u64 = 8 << 30;

/// A file from the cache with exactly this checksum, put at `dest` (a hard link where the
/// drive allows, else a copy). Checked again before it is used.
fn from_cache(sha256: &str, dest: &Path, cancel: &AtomicBool) -> Option<u64> {
    let cached = installer_cache()?.join(sha256.to_ascii_lowercase());
    let (size, got) = hash_file(&cached, cancel, |_| {}).ok()?;
    if !got.eq_ignore_ascii_case(sha256) {
        let _ = fs::remove_file(&cached);
        return None;
    }
    let _ = fs::remove_file(dest);
    fs::hard_link(&cached, dest)
        .or_else(|_| fs::copy(&cached, dest).map(|_| ()))
        .ok()?;
    // Recently used files are the last to be trimmed.
    let _ = fs::File::open(&cached).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    Some(size)
}

/// Keeps a verified download in the cache, then trims the cache to its size limit.
fn to_cache(sha256: &str, file: &Path) {
    let Some(dir) = installer_cache() else { return };
    let cached = dir.join(sha256.to_ascii_lowercase());
    if !cached.exists() {
        let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        if fs::hard_link(file, &tmp)
            .or_else(|_| fs::copy(file, &tmp).map(|_| ()))
            .and_then(|()| fs::rename(&tmp, &cached))
            .is_err()
        {
            let _ = fs::remove_file(&tmp);
        }
    }
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            let modified = m.modified().ok()?;
            m.is_file().then(|| (modified, m.len(), e.path()))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let mut total = 0;
    for (_, len, p) in files {
        total += len;
        if total > CACHE_LIMIT {
            let _ = fs::remove_file(p);
        }
    }
}

fn download_with(
    url: &str,
    dest: &Path,
    sha256: Option<&str>,
    cancel: &AtomicBool,
    accept: Option<&str>,
) -> Result<u64> {
    if let Some(want) = sha256
        && let Some(size) = from_cache(want, dest, cancel)
    {
        crate::diag::log(&format!(
            "download {url}: reused the cached copy, sha256 {want}"
        ));
        return Ok(size);
    }
    // curl's own --retry skips some transient failures (a CDN resetting an HTTP/2 stream), so
    // a failed download is tried once more from the start.
    let mut tries = 0;
    loop {
        let mut c = curl(url)?;
        if let Some(a) = accept {
            c.arg("--header").arg(format!("Accept: {a}"));
        }
        // The token goes only to api.github.com; curl does not send it on to the storage
        // host the API redirects to.
        let token = TokenHeader::new(url);
        if let Some(t) = &token {
            c.arg("--header").arg(format!("@{}", t.0.display()));
        }
        c.arg("--output").arg(dest).arg(url);
        match run(&mut c, cancel) {
            Ok(_) => break,
            Err(e) => {
                let _ = fs::remove_file(dest);
                tries += 1;
                if tries == 2 || matches!(e, crate::Error::Cancelled) {
                    return Err(e);
                }
            }
        }
    }
    let (size, got) = hash_file(dest, cancel, |_| {})?;
    crate::diag::log(&format!(
        "download {url} → {size} bytes, sha256 {got} ({})",
        match sha256 {
            Some(w) if got.eq_ignore_ascii_case(w) => "matches the published checksum",
            Some(_) => "DOES NOT match the published checksum; discarded",
            None => "no published checksum",
        }
    ));
    if sha256.is_some_and(|want| !got.eq_ignore_ascii_case(want)) {
        let _ = fs::remove_file(dest);
        return Err(invalid(format!(
            "The download from {url} does not match its published checksum; it was discarded."
        )));
    }
    // Only files checked against a published checksum are shared.
    if sha256.is_some() {
        to_cache(&got, dest);
    }
    Ok(size)
}

/// Like [`download`], but an interrupted download continues where it stopped, and
/// `progress` hears the bytes on disk while it runs. A partial file is kept for the next try;
/// a complete file that does not match the checksum is deleted.
pub fn download_resumable(
    url: &str,
    dest: &Path,
    sha256: &str,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<u64> {
    let matches = |p: &Path| {
        hash_file(p, cancel, |_| {})
            .ok()
            .filter(|(_, h)| h.eq_ignore_ascii_case(sha256))
            .map(|(size, _)| size)
    };
    if let Some(size) = matches(dest) {
        return Ok(size);
    }
    let mut attempt = |resume: bool| -> Result<String> {
        let mut c = curl(url)?;
        if resume {
            c.args(["--continue-at", "-"]);
        }
        c.arg("--output").arg(dest).arg(url);
        std::thread::scope(|s| {
            let curl = s.spawn(|| run(&mut c, cancel));
            while !curl.is_finished() {
                progress(fs::metadata(dest).map_or(0, |m| m.len()));
                std::thread::sleep(Duration::from_millis(250));
            }
            curl.join()
                .unwrap_or_else(|_| Err(invalid("The download stopped unexpectedly.")))
        })
    };
    let resumed = dest.is_file();
    match attempt(resumed) {
        Err(crate::Error::Cancelled) => return Err(crate::Error::Cancelled),
        Ok(_) if matches(dest).is_some() => {}
        // A partial file that cannot be continued (the server refuses the range, or what was
        // kept was damaged): start again from the beginning, once.
        _ if resumed => {
            let _ = fs::remove_file(dest);
            attempt(false)?;
        }
        r => {
            r?;
        }
    }
    progress(fs::metadata(dest).map_or(0, |m| m.len()));
    matches(dest).ok_or_else(|| {
        let _ = fs::remove_file(dest);
        invalid(format!(
            "The download from {url} does not match its published checksum; it was discarded."
        ))
    })
}

#[cfg(test)]
mod tests {

    /// A verified file is reused from the cache by its checksum, without any network: the
    /// URL here does not exist.
    #[test]
    fn verified_downloads_are_reused() {
        use sha2::{Digest, Sha256};
        let t = tempfile::tempdir().unwrap();
        let body = b"installer bytes";
        let sha = format!("{:x}", Sha256::digest(body));
        let src = t.path().join("got");
        fs::write(&src, body).unwrap();
        to_cache(&sha, &src);
        let dest = t.path().join("again");
        let size = download(
            "https://invalid.invalid/x",
            &dest,
            Some(&sha),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(size, body.len() as u64);
        assert_eq!(fs::read(&dest).unwrap(), body);
        // A damaged cached copy is thrown away, not used.
        let cached = installer_cache().unwrap().join(&sha);
        let _ = fs::remove_file(&cached);
        fs::write(&cached, b"tampered").unwrap();
        assert!(from_cache(&sha, &t.path().join("x"), &AtomicBool::new(false)).is_none());
        assert!(!cached.exists());
    }

    /// A repeated GitHub lookup is answered from disk after a "304 Not Modified", which costs
    /// none of the hourly requests. Run with `--ignored live`.
    #[test]
    #[ignore]
    fn live_etag_cache() {
        let url = format!("{GITHUB_API}repos/fleetwoodjohnr/simple-setup/releases/latest");
        let remaining = || {
            let out = Command::new("curl")
                .args(["-s", "https://api.github.com/rate_limit"])
                .output()
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
            v["resources"]["core"]["remaining"].as_u64().unwrap()
        };
        let stop = AtomicBool::new(false);
        let first = text(&url, None, &stop).unwrap();
        let (dir, key) = disk_cache(&url).unwrap();
        assert!(dir.join(format!("{key}.etag")).is_file());
        *CACHE.lock().unwrap() = None;
        let before = remaining();
        let second = text(&url, None, &stop).unwrap();
        assert_eq!(first, second);
        assert_eq!(remaining(), before, "the conditional request was counted");
    }

    use super::*;

    #[test]
    fn only_secure_links() {
        for bad in [
            "http://example.org/x",
            "file:///etc/passwd",
            "https://a b",
            "-o/tmp/x",
        ] {
            assert!(curl(bad).is_err(), "{bad}");
        }
        assert!(curl("https://example.org/x.msi").is_ok());
    }

    #[test]
    fn rate_limit_messages() {
        assert!(rate_limited(
            "curl failed: The requested URL returned error: 403"
        ));
        assert!(!rate_limited("curl failed: Could not resolve host"));
        let m = limit_message(Some((60, 0, 1_800_000_000)), false);
        assert!(
            m.contains("60 requests")
                && m.contains("2027-01-15T08:00:00Z")
                && m.contains("GITHUB_TOKEN"),
            "{m}"
        );
        assert!(!limit_message(None, true).contains("GITHUB_TOKEN"));
        // Regression: a 403 with requests left was reported as a used-up limit.
        let m = limit_message(Some((60, 42, 1_800_000_000)), true);
        assert!(m.contains("private") && !m.contains("used up"), "{m}");
    }

    /// `cargo test -p simple-setup-core -- --ignored live`: an interrupted download resumes.
    #[test]
    #[ignore = "uses the network"]
    fn live_download_resumes() {
        let url = "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz";
        let sha = "33e15bcf1624b25cdd2a55813a47a2f95dbe126268203e76aa6a585d1e7b149c";
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("rg.tar.gz");
        let cancel = AtomicBool::new(false);
        download(url, &dest, Some(sha), &cancel).unwrap();
        let whole = fs::read(&dest).unwrap();
        // Cut off: the next attempt fetches only the rest.
        fs::write(&dest, &whole[..whole.len() / 3]).unwrap();
        let mut seen = Vec::new();
        let size = download_resumable(url, &dest, sha, &cancel, &mut |n| seen.push(n)).unwrap();
        assert_eq!(size as usize, whole.len());
        assert!(
            seen.first().is_some_and(|n| *n >= (whole.len() / 3) as u64),
            "{seen:?}"
        );
        // A damaged complete file cannot be continued: it is fetched again from the start.
        fs::write(&dest, vec![0u8; whole.len()]).unwrap();
        download_resumable(url, &dest, sha, &cancel, &mut |_| {}).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), whole);
    }
}
