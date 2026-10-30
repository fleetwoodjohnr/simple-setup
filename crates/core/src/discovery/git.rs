//! Source checkouts. Git metadata is read directly from files; `git` is never executed
//! because repository config (hooks, fsmonitor) can run code.

use std::path::{Path, PathBuf};

use super::{App, Confidence, Ctx, GitHub, Kind, LocKind, Origin, Source};

#[derive(Debug, Clone, PartialEq)]
pub struct Checkout {
    pub dir: PathBuf,
    /// `origin` (or first) remote URL with any credentials removed.
    pub remote: Option<String>,
    /// Tag at HEAD, else `branch@commit`, else the commit.
    pub reference: Option<String>,
    pub build: Vec<&'static str>,
}

impl Checkout {
    pub fn github(&self, confidence: Confidence) -> Option<GitHub> {
        Some(GitHub {
            repo: github_repo(self.remote.as_deref()?)?,
            reference: self.reference.clone(),
            asset: None,
            confidence,
            origin: Origin::Detected,
        })
    }

    pub fn github_evidence(&self) -> Vec<String> {
        let mut ev = Vec::new();
        if let Some(r) = &self.remote {
            ev.push(format!("Git remote: {r}"));
        }
        if let Some(r) = &self.reference {
            ev.push(format!("Checked out at {r}"));
        }
        if !self.build.is_empty() {
            ev.push(format!("Build files found: {}", self.build.join(", ")));
        }
        ev
    }
}

/// Removes `user:password@` from URLs so tokens never reach an inventory or manifest.
pub fn strip_credentials(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('/') {
            Some((auth, path)) => {
                let host = auth.rsplit_once('@').map_or(auth, |(_, h)| h);
                format!("{scheme}://{host}/{path}")
            }
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

/// `owner/name` for GitHub remotes in https, ssh and scp-like forms.
pub fn github_repo(url: &str) -> Option<String> {
    let url = strip_credentials(url);
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://github.com/"))
        .or_else(|| url.strip_prefix("git://github.com/"))
        .or_else(|| url.strip_prefix("git@github.com:"))?;
    let mut parts = rest
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .splitn(3, '/');
    let (owner, name) = (parts.next()?, parts.next()?);
    let ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    };
    (ok(owner) && ok(name)).then(|| format!("{owner}/{name}"))
}

/// URL of `[remote "origin"]`, else of the first remote, from a `.git/config`.
pub fn parse_remote_url(config: &str) -> Option<String> {
    let (mut section, mut first, mut origin) = (String::new(), None, None);
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            section = line.to_string();
        } else if let Some((k, v)) = line.split_once('=')
            && k.trim() == "url"
            && section.starts_with("[remote ")
        {
            let v = v.trim().trim_matches('"').to_string();
            if section == "[remote \"origin\"]" {
                origin = Some(v.clone());
            }
            first.get_or_insert(v);
        }
    }
    origin.or(first).map(|u| strip_credentials(&u))
}

fn git_dir(dir: &Path) -> Option<PathBuf> {
    let dot = dir.join(".git");
    if dot.is_dir() {
        return Some(dot);
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let p = PathBuf::from(text.strip_prefix("gitdir:")?.trim());
    Some(if p.is_absolute() { p } else { dir.join(p) })
}

fn resolve_ref(gd: &Path, name: &str) -> Option<String> {
    if let Ok(h) = std::fs::read_to_string(gd.join(name)) {
        return Some(h.trim().to_string());
    }
    let packed = std::fs::read_to_string(gd.join("packed-refs")).ok()?;
    packed.lines().find_map(|l| {
        l.split_once(' ')
            .filter(|(_, r)| *r == name)
            .map(|(h, _)| h.to_string())
    })
}

fn tag_at(gd: &Path, commit: &str) -> Option<String> {
    let mut tags: Vec<(String, String)> = std::fs::read_dir(gd.join("refs/tags"))
        .map(|rd| {
            rd.flatten()
                .filter_map(|e| {
                    let h = std::fs::read_to_string(e.path()).ok()?;
                    Some((
                        e.file_name().to_string_lossy().into_owned(),
                        h.trim().to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if let Ok(packed) = std::fs::read_to_string(gd.join("packed-refs")) {
        let mut last: Option<String> = None;
        for l in packed.lines() {
            // "^<hash>" peels the annotated tag on the previous line.
            if let (Some(peeled), Some(t)) = (l.strip_prefix('^'), last.take()) {
                tags.push((t, peeled.to_string()));
            } else if let Some((h, r)) = l.split_once(' ')
                && let Some(t) = r.strip_prefix("refs/tags/")
            {
                tags.push((t.to_string(), h.to_string()));
                last = Some(t.to_string());
            }
        }
    }
    tags.into_iter().find(|(_, h)| h == commit).map(|(t, _)| t)
}

pub fn read_checkout(dir: &Path) -> Option<Checkout> {
    let gd = git_dir(dir)?;
    let remote = std::fs::read_to_string(gd.join("config"))
        .ok()
        .and_then(|c| parse_remote_url(&c));
    let head = std::fs::read_to_string(gd.join("HEAD")).ok()?;
    let head = head.trim();
    let (branch, commit) = match head.strip_prefix("ref: ") {
        Some(r) => (
            r.strip_prefix("refs/heads/").map(str::to_string),
            resolve_ref(&gd, r),
        ),
        None => (None, Some(head.to_string())),
    };
    let reference = commit.as_ref().map(|c| {
        tag_at(&gd, c).unwrap_or_else(|| {
            let short = &c[..c.len().min(12)];
            branch
                .as_ref()
                .map_or_else(|| short.to_string(), |b| format!("{b}@{short}"))
        })
    });
    let build = [
        ("Cargo.toml", "Rust (Cargo)"),
        ("package.json", "Node.js (npm)"),
        ("pyproject.toml", "Python"),
        ("setup.py", "Python"),
        ("go.mod", "Go"),
        ("CMakeLists.txt", "CMake"),
        ("meson.build", "Meson"),
        ("Makefile", "Make"),
    ]
    .into_iter()
    .filter(|(f, _)| dir.join(f).is_file())
    .map(|(_, l)| l)
    .collect();
    Some(Checkout {
        dir: dir.to_path_buf(),
        remote,
        reference,
        build,
    })
}

/// The checkout a file lives in, searching parents strictly below `home` (a home directory
/// that is itself a repository is not treated as the program's source).
pub fn checkout_containing(path: &Path, home: &Path) -> Option<Checkout> {
    path.ancestors()
        .skip(1)
        .take_while(|d| d.starts_with(home) && *d != home)
        .find_map(read_checkout)
}

/// Finds checkouts up to three levels below each user-approved folder.
pub fn scan(cx: &mut Ctx, dirs: &[PathBuf]) {
    for root in dirs {
        let mut it = walkdir::WalkDir::new(root).max_depth(3).into_iter();
        while let Some(Ok(e)) = it.next() {
            if !e.file_type().is_dir() {
                continue;
            }
            let name = e.file_name().to_string_lossy();
            if e.depth() > 0
                && (name.starts_with('.') || name == "node_modules" || name == "target")
            {
                it.skip_current_dir();
                continue;
            }
            let Some(co) = read_checkout(e.path()) else {
                continue;
            };
            it.skip_current_dir();
            let label = e
                .path()
                .file_name()
                .map_or_else(|| co.dir.to_string_lossy(), |n| n.to_string_lossy())
                .into_owned();
            let mut app = App::new(
                Source::GitClone,
                &co.dir.to_string_lossy(),
                &label,
                Kind::SourceBuild,
            );
            app.version = co.reference.clone();
            app.github = co.github(Confidence::Confirmed);
            app.confidence = if app.github.is_some() {
                Confidence::Confirmed
            } else {
                Confidence::Probable
            };
            app.evidence
                .push(format!("Git checkout at {}", co.dir.display()));
            app.evidence.extend(co.github_evidence());
            app.add_location(LocKind::Data, co.dir.clone(), Origin::Detected);
            app.path = Some(co.dir.clone());
            cx.apps.push(app);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_urls() {
        for u in [
            "https://github.com/owner/repo.git",
            "https://user:ghp_secret@github.com/owner/repo",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo/",
        ] {
            assert_eq!(github_repo(u).as_deref(), Some("owner/repo"), "{u}");
        }
        assert_eq!(github_repo("https://gitlab.com/owner/repo"), None);
        assert_eq!(strip_credentials("https://u:tok@host/x"), "https://host/x");
    }

    #[test]
    fn reads_checkout_files() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("proj");
        std::fs::create_dir_all(d.join(".git/refs/heads")).unwrap();
        std::fs::write(
            d.join(".git/config"),
            "[core]\n\tbare = false\n[remote \"upstream\"]\n\turl = https://github.com/up/x\n[remote \"origin\"]\n\turl = https://me:tok@github.com/me/proj.git\n",
        )
        .unwrap();
        std::fs::write(d.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(d.join(".git/refs/heads/main"), "0123456789abcdef0123\n").unwrap();
        std::fs::write(
            d.join(".git/packed-refs"),
            "# pack\nffff refs/tags/v0.9\n0123456789abcdef0123 refs/tags/v1.0\n",
        )
        .unwrap();
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        let co = read_checkout(&d).unwrap();
        assert_eq!(co.remote.as_deref(), Some("https://github.com/me/proj.git"));
        assert_eq!(co.reference.as_deref(), Some("v1.0"));
        assert_eq!(co.build, vec!["Rust (Cargo)"]);
        assert_eq!(co.github(Confidence::Confirmed).unwrap().repo, "me/proj");

        std::fs::create_dir_all(d.join("bin")).unwrap();
        assert_eq!(
            checkout_containing(&d.join("bin/run"), t.path())
                .unwrap()
                .dir,
            d
        );
        assert!(checkout_containing(&d.join("bin/run"), &d).is_none());
    }
}
