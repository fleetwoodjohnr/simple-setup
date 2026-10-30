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
        "--user-agent",
        concat!("simple-setup/", env!("CARGO_PKG_VERSION")),
    ]);
    Ok(c)
}

const GITHUB_API: &str = "https://api.github.com/";

/// A GitHub token from `GITHUB_TOKEN` or `GH_TOKEN` raises the API limit from 60 to 5,000
/// requests an hour. It goes to curl in a private header file, never on the command line
/// (where other users of the computer could read it).
struct TokenHeader(PathBuf);

impl TokenHeader {
    fn new(url: &str) -> Option<Self> {
        if !url.starts_with(GITHUB_API) {
            return None;
        }
        let token = ["GITHUB_TOKEN", "GH_TOKEN"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|t| !t.trim().is_empty()))?;
        let token = token.trim();
        if !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return None;
        }
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
    c.args(["--max-filesize", "20000000"]).arg(url);
    let body = run(&mut c, cancel).map_err(|e| match e {
        crate::Error::Cancelled => e,
        e if url.starts_with(GITHUB_API) && rate_limited(&e.to_string()) => {
            invalid(github_limit_message(token.is_some(), cancel))
        }
        e => invalid(format!("Looking up {url} failed: {e}")),
    })?;
    if cached && let Ok(mut c) = CACHE.lock() {
        c.get_or_insert_with(HashMap::new)
            .insert(url.to_string(), (Instant::now(), body.clone()));
    }
    Ok(body)
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
        _ => format!("GitHub refused the request (it may be busy or limiting requests).{hint}"),
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
/// deleted. Returns the size.
pub fn download(url: &str, dest: &Path, sha256: &str, cancel: &AtomicBool) -> Result<u64> {
    let mut c = curl(url)?;
    c.arg("--output").arg(dest).arg(url);
    if let Err(e) = run(&mut c, cancel) {
        let _ = fs::remove_file(dest);
        return Err(e);
    }
    let (size, got) = hash_file(dest, cancel, |_| {})?;
    if !got.eq_ignore_ascii_case(sha256) {
        let _ = fs::remove_file(dest);
        return Err(invalid(format!(
            "The download from {url} does not match its published checksum; it was discarded."
        )));
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
        download(url, &dest, sha, &cancel).unwrap();
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
