//! Network access while a snapshot is built: catalog lookups and installer downloads through
//! the system's `curl`, HTTPS only. Restores never use this module; they work from the snapshot
//! alone.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::AtomicBool;

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

/// A small text answer (catalog entries, API listings).
pub fn text(url: &str, accept: Option<&str>, cancel: &AtomicBool) -> Result<String> {
    let mut c = curl(url)?;
    if let Some(a) = accept {
        c.arg("--header").arg(format!("Accept: {a}"));
    }
    c.args(["--max-filesize", "20000000"]).arg(url);
    run(&mut c, cancel).map_err(|e| invalid(format!("Looking up {url} failed: {e}")))
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
}
