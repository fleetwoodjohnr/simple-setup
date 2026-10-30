//! Path safety. Everything read from a manifest is untrusted, and bundles must survive
//! FAT/exFAT/NTFS drives and restores onto Windows, so names are checked both ways.

use std::path::{Path, PathBuf};

use crate::{Result, invalid};

const WINDOWS_RESERVED: &[&str] = &["CON", "PRN", "AUX", "NUL"];

/// True if `name` is a valid file name on Linux, Windows and macOS alike.
pub fn portable_component(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.len() > 255 {
        return false;
    }
    if name
        .chars()
        .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        || name.ends_with(['.', ' '])
    {
        return false;
    }
    let stem = name
        .split('.')
        .next()
        .unwrap_or(name)
        .trim_end()
        .to_ascii_uppercase();
    let numbered = (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.len() == 4
        && stem.as_bytes()[3].is_ascii_digit();
    !(WINDOWS_RESERVED.contains(&stem.as_str()) || numbered)
}

/// A portable stand-in for a non-portable name. Uniqueness is the caller's job.
pub fn sanitize_component(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    while s.ends_with(['.', ' ']) {
        s.pop();
    }
    if s.len() > 200 {
        let mut cut = 200;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
    if !portable_component(&s) {
        s.insert(0, '_');
    }
    s
}

/// Validates a '/'-separated relative path from a manifest. Rejects absolute paths, drive
/// letters, backslashes, `.`/`..` and empty components. With `portable`, every component must
/// also be valid on Windows (used for paths *inside* the bundle, which we always write portably).
pub fn check_rel(rel: &str, portable: bool) -> Result<()> {
    let bad = || invalid(format!("Unsafe path in snapshot: {rel:?}"));
    if rel.is_empty() || rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        return Err(bad());
    }
    for c in rel.split('/') {
        if c.is_empty()
            || c == "."
            || c == ".."
            || c.contains(':')
            || c.chars().any(char::is_control)
        {
            return Err(bad());
        }
        if portable && !portable_component(c) {
            return Err(bad());
        }
    }
    Ok(())
}

/// The current user's home folder.
pub fn home_dir() -> Option<PathBuf> {
    dirs::home_dir()
}

/// `root` joined with a checked relative path; the result cannot escape `root`.
pub fn join_rel(root: &Path, rel: &str, portable: bool) -> Result<PathBuf> {
    check_rel(rel, portable)?;
    Ok(rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c)))
}

/// `path` relative to `base` as a '/'-separated string, if it is inside it.
pub fn rel_string(path: &Path, base: &Path) -> Option<String> {
    let rel = path.strip_prefix(base).ok()?;
    let parts: Option<Vec<&str>> = rel.components().map(|c| c.as_os_str().to_str()).collect();
    Some(parts?.join("/"))
}

/// Replaces a leading home directory with `~` for display and portable storage.
pub fn tilde(path: &Path, home: &Path) -> String {
    match rel_string(path, home) {
        Some(r) if r.is_empty() => "~".into(),
        Some(r) => format!("~/{r}"),
        None => path.to_string_lossy().into_owned(),
    }
}

/// Expands a stored `~/…` path against `home`. Non-tilde paths must be absolute on this OS.
pub fn untilde(stored: &str, home: &Path) -> Result<PathBuf> {
    match stored.strip_prefix("~/") {
        Some(rel) => join_rel(home, rel, false),
        None if stored == "~" => Ok(home.to_path_buf()),
        None if Path::new(stored).is_absolute()
            && !stored.split(['/', '\\']).any(|c| c == "..") =>
        {
            Ok(PathBuf::from(stored))
        }
        None => Err(invalid(format!("Unsafe path in snapshot: {stored:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_portability() {
        for ok in ["notes.txt", "COM10", "console.log", ".bashrc", "a b"] {
            assert!(portable_component(ok), "{ok}");
        }
        for bad in [
            "CON", "con.txt", "Nul", "LPT1.log", "com3", "a:b", "x?", "trail.", "trail ", "", "..",
            "a\u{1}b",
        ] {
            assert!(!portable_component(bad), "{bad}");
        }
        assert_eq!(sanitize_component("a:b?."), "a_b_");
        assert_eq!(sanitize_component("CON"), "_CON");
        assert!(portable_component(&sanitize_component(&"é".repeat(300))));
    }

    #[test]
    fn relative_paths_cannot_escape() {
        assert!(check_rel("personal-files/Documents/a.txt", true).is_ok());
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "a/./b",
            "C:/x",
            "a\\b",
            "a/CON",
        ] {
            assert!(check_rel(bad, true).is_err(), "{bad}");
        }
        assert!(check_rel("a/CON", false).is_ok());
        let root = Path::new("/r");
        assert_eq!(join_rel(root, "a/b", true).unwrap(), Path::new("/r/a/b"));
    }

    #[test]
    fn tilde_round_trip() {
        let home = Path::new("/home/u");
        assert_eq!(tilde(Path::new("/home/u/.config/x"), home), "~/.config/x");
        assert_eq!(
            untilde("~/.config/x", Path::new("/home/v")).unwrap(),
            Path::new("/home/v/.config/x")
        );
        assert!(untilde("~/../etc", home).is_err());
        assert!(untilde("relative/x", home).is_err());
        assert!(untilde("/abs/../etc", home).is_err());
    }
}
