//! Size estimates and folder listings for the selection screens. Metadata only: file
//! contents are never read.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Serialize;

use crate::sensitive::{Flag, classify};

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct Measure {
    pub path: PathBuf,
    pub exists: bool,
    pub files: u64,
    pub bytes: u64,
    /// Secret-flagged items that will be left out unless the user opts in.
    pub secrets: u64,
    /// Bytes of caches and lock files that are always left out.
    pub transient_bytes: u64,
    /// Stopped early (very large folder); numbers are lower bounds.
    pub partial: bool,
}

/// Totals for what a copy of `path` would include, honouring `exclude`.
// ponytail: time-boxed walk; replace with a cached background index if huge folders matter.
pub fn measure(path: &Path, exclude: &[PathBuf], budget: Duration) -> Measure {
    let start = Instant::now();
    let mut m = Measure {
        path: path.to_path_buf(),
        exists: path.exists(),
        ..Default::default()
    };
    let mut it = walkdir::WalkDir::new(path).follow_links(false).into_iter();
    while let Some(Ok(e)) = it.next() {
        if start.elapsed() > budget {
            m.partial = true;
            break;
        }
        let skip_dir = |it: &mut walkdir::IntoIter| {
            if e.file_type().is_dir() {
                it.skip_current_dir();
            }
        };
        if exclude.iter().any(|x| e.path().starts_with(x)) {
            skip_dir(&mut it);
            continue;
        }
        match classify(e.path()) {
            Some(Flag::Secret(_)) => {
                m.secrets += 1;
                skip_dir(&mut it);
                continue;
            }
            Some(Flag::Transient(_)) => {
                m.transient_bytes += e.metadata().map(|md| md.len()).unwrap_or(0);
                continue;
            }
            None => {}
        }
        if e.file_type().is_file() {
            m.files += 1;
            m.bytes += e.metadata().map(|md| md.len()).unwrap_or(0);
        }
    }
    m
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct DirItem {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub size: Option<u64>,
    /// Why this item is protected (left out unless opted in), if it is.
    pub secret: Option<String>,
    pub transient: bool,
}

/// One level of a folder, folders first, for the include/exclude tree.
pub fn list_dir(path: &Path) -> std::io::Result<Vec<DirItem>> {
    let mut out: Vec<DirItem> = std::fs::read_dir(path)?
        .flatten()
        .map(|e| {
            let p = e.path();
            let md = std::fs::symlink_metadata(&p).ok();
            let flag = classify(&p).or_else(|| classify(&p.join("x")));
            DirItem {
                name: e.file_name().to_string_lossy().into_owned(),
                is_dir: md.as_ref().is_some_and(|m| m.is_dir()),
                size: md.as_ref().filter(|m| m.is_file()).map(|m| m.len()),
                secret: match flag {
                    Some(Flag::Secret(why)) => Some(why.to_string()),
                    _ => None,
                },
                transient: matches!(flag, Some(Flag::Transient(_))),
                path: p,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_and_flags() {
        // Windows' temp folder (AppData/Local/Temp) is itself skipped as temporary files.
        let t = if cfg!(windows) {
            tempfile::tempdir_in(".")
        } else {
            tempfile::tempdir()
        }
        .unwrap();
        std::fs::create_dir_all(t.path().join(".ssh")).unwrap();
        std::fs::write(t.path().join(".ssh/id_ed25519"), "k").unwrap();
        std::fs::write(t.path().join("a.txt"), "hello").unwrap();
        std::fs::create_dir_all(t.path().join("skip")).unwrap();
        std::fs::write(t.path().join("skip/b.txt"), "zzz").unwrap();
        let m = measure(t.path(), &[t.path().join("skip")], Duration::from_secs(5));
        assert_eq!((m.files, m.bytes, m.secrets), (1, 5, 1));
        let items = list_dir(t.path()).unwrap();
        assert_eq!(items[0].name, ".ssh");
        assert!(
            items[0].secret.is_some(),
            "folder holding secrets is flagged"
        );
    }
}
