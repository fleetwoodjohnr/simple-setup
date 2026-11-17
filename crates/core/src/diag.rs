//! Diagnostic log: every command Simple Setup runs (program, arguments, exit code, duration),
//! downloads and errors, for troubleshooting. Off until the CLI or the desktop app calls
//! [`enable`]; written to the user's state folder and rotated by size. Tokens never reach it:
//! they travel in private header files, and arguments that look like credentials are hidden.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use crate::{IoContext, Result, invalid};

const FILE: &str = "simple-setup.log";
const MAX: u64 = 4 << 20;

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// `~/.local/state/simple-setup/logs` (Linux), the local app data folder elsewhere.
pub fn default_dir() -> Option<PathBuf> {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .map(|d| d.join("simple-setup").join("logs"))
}

/// Starts logging into `dir` (normally [`default_dir`]).
pub fn enable(dir: PathBuf) {
    if fs::create_dir_all(&dir).is_ok()
        && let Ok(mut d) = DIR.lock()
    {
        *d = Some(dir);
    }
    log(&format!(
        "Simple Setup {} started ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
}

/// The log folder, when logging is on.
pub fn dir() -> Option<PathBuf> {
    DIR.lock().ok()?.clone()
}

/// Appends one line. Failing to log never fails the operation.
pub fn log(line: &str) {
    let Some(dir) = dir() else { return };
    let path = dir.join(FILE);
    if fs::metadata(&path).is_ok_and(|m| m.len() > MAX) {
        let _ = fs::rename(&path, dir.join(format!("{FILE}.1")));
    }
    let line: String = line.chars().filter(|c| *c != '\r').collect();
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{} {}", crate::now(), line.replace('\n', " / ")));
}

/// Hides arguments that carry credentials (`--password=…`, `Authorization: …`, `?token=…`).
fn shown(arg: &str) -> String {
    let lower = arg.to_ascii_lowercase();
    let secret = ["authorization", "token", "password", "secret", "bearer"]
        .iter()
        .any(|k| lower.contains(k));
    if secret {
        "[hidden]".into()
    } else if arg.contains(' ') {
        format!("{arg:?}")
    } else {
        arg.to_string()
    }
}

/// Records a finished command.
pub fn command(cmd: &Command, code: Option<i32>, took: Duration, why: &str) {
    if dir().is_none() {
        return;
    }
    let mut line = format!("run {}", cmd.get_program().to_string_lossy());
    for a in cmd.get_args() {
        line.push(' ');
        line.push_str(&shown(&a.to_string_lossy()));
    }
    let outcome = match code {
        Some(0) => "exit 0".to_string(),
        Some(c) => format!("exit {c}: {why}"),
        None => format!("stopped: {why}"),
    };
    log(&format!("{line} → {outcome} ({:.1} s)", took.as_secs_f64()));
}

/// Writes the log (with the previous one) and a description of this computer to `dest`.
pub fn export(dest: &Path, system: &str) -> Result<PathBuf> {
    let dir = dir()
        .or_else(default_dir)
        .ok_or_else(|| invalid("No log folder."))?;
    let mut text = format!(
        "Simple Setup {} diagnostics, {}\n\n{system}\n\n",
        env!("CARGO_PKG_VERSION"),
        crate::now()
    );
    for name in [format!("{FILE}.1"), FILE.to_string()] {
        if let Ok(t) = fs::read_to_string(dir.join(&name)) {
            text.push_str(&format!("--- {name}\n{t}\n"));
        }
    }
    fs::write(dest, text).ctx(|| format!("Writing {}", dest.display()))?;
    Ok(dest.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_hidden() {
        assert_eq!(shown("--header"), "--header");
        assert_eq!(shown("Authorization: Bearer abc"), "[hidden]");
        assert_eq!(shown("https://x.org/a?token=1"), "[hidden]");
        assert_eq!(shown("a b"), "\"a b\"");
    }

    #[test]
    fn writes_rotates_and_exports() {
        let t = tempfile::tempdir().unwrap();
        enable(t.path().to_path_buf());
        let mut c = Command::new("dnf");
        c.args(["install", "-y", "--password=x", "jq"]);
        command(&c, Some(1), Duration::from_millis(1500), "no network");
        let text = fs::read_to_string(t.path().join(FILE)).unwrap();
        assert!(
            text.contains("run dnf install -y [hidden] jq → exit 1: no network (1.5 s)"),
            "{text}"
        );
        fs::write(t.path().join(FILE), vec![b'x'; (MAX + 1) as usize]).unwrap();
        log("after rotation");
        assert!(t.path().join(format!("{FILE}.1")).is_file());
        let out = export(&t.path().join("diag.txt"), "System: test").unwrap();
        let all = fs::read_to_string(out).unwrap();
        assert!(all.contains("System: test") && all.contains("after rotation"));
        *DIR.lock().unwrap() = None;
    }
}
