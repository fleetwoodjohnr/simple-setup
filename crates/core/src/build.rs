//! Snapshot builder: preflight → copy into a hidden staging folder (resumable) → re-verify every
//! copy → add restore tools → write manifest and checksums → rename into place. Sources are only
//! ever read; nothing at the destination is overwritten.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::adapters;
use crate::crypt::{self, Key};
use crate::discovery::{App, Inventory, LocKind, Source};
use crate::installers::{self, Collect, Shared};
use crate::manifest::{
    BaseRef, Coverage, Encryption, ExcludedSummary, FileEntry, Installer, LinkEntry, MANIFEST_FILE,
    Manifest, ManifestApp, Part, Readiness, Recipe, Root, RootKind, SCHEMA_VERSION, Status,
};
use crate::paths::{portable_component, rel_string, sanitize_component, tilde};
use crate::platform::{OsFamily, Platform, free_space};
use crate::selection::Selection;
use crate::sensitive::{Flag, classify};
use crate::{Error, IoContext, Result, invalid, registry};

pub const ENGINE_DIR: &str = "restore-engine";
pub const CHECKSUMS: &str = "checksums/SHA256SUMS";
const MARKER: &str = ".simple-setup-partial.json";
const JOURNAL: &str = ".simple-setup-journal.jsonl";
const INSTALLER_JOURNAL: &str = ".simple-setup-installers.jsonl";
const EXAMPLES: usize = 25;

/// Restore-engine binaries available on this computer, by target platform.
pub type Engines = HashMap<Platform, PathBuf>;

pub fn engine_file(p: Platform) -> &'static str {
    if p.os == OsFamily::Windows {
        "simple-setup.exe"
    } else {
        "simple-setup"
    }
}

/// Where an installed copy keeps the restore kit, relative to the folder of its program: next to
/// it (Windows, portable copies), `../lib/Simple Setup/` (Linux packages and AppImage) or
/// `../Resources/` (macOS app bundle).
pub fn kit_dirs(exe_dir: &Path) -> Vec<PathBuf> {
    vec![
        exe_dir.join("restore-kit"),
        exe_dir.join("../lib/Simple Setup/restore-kit"),
        exe_dir.join("../Resources/restore-kit"),
    ]
}

/// Engines for the host (`host`, usually the running CLI) and any in a release kit folder
/// laid out as `<kit>/<os>-<arch>/simple-setup[.exe]`.
pub fn find_engines(host: Option<PathBuf>, kits: &[PathBuf]) -> Engines {
    let mut out = Engines::new();
    for kit in kits {
        for os in [OsFamily::Linux, OsFamily::Windows, OsFamily::Macos] {
            for arch in [
                crate::platform::Arch::X86_64,
                crate::platform::Arch::Aarch64,
            ] {
                let p = Platform { os, arch };
                let f = kit.join(p.to_string()).join(engine_file(p));
                if f.is_file() {
                    out.insert(p, f);
                }
            }
        }
    }
    if let (Some(h), Ok(p)) = (host.filter(|h| h.is_file()), Platform::current()) {
        out.entry(p).or_insert(h);
    }
    out
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Progress {
    Stage {
        label: String,
        step: u32,
        steps: u32,
    },
    Copy {
        done_bytes: u64,
        total_bytes: u64,
        done_files: u64,
        total_files: u64,
        current: String,
    },
    Warning {
        message: String,
    },
    /// What a long-running step is doing right now, e.g. installing one app.
    Note {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RootSummary {
    pub id: String,
    pub kind: RootKind,
    pub app: Option<String>,
    pub label: String,
    pub original: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Preflight {
    pub roots: Vec<RootSummary>,
    pub total_files: u64,
    pub total_bytes: u64,
    pub engine_bytes: u64,
    /// Estimate; real installer sizes are known only after they are collected.
    pub installer_bytes: u64,
    pub excluded: ExcludedSummary,
    pub coverage: Vec<Coverage>,
    pub readiness: Vec<Readiness>,
    pub warnings: Vec<String>,
    pub destination_free: Option<u64>,
    pub required_bytes: u64,
    pub fits: bool,
}

struct PlannedRoot {
    root: Root,
    label: String,
}

struct PlannedFile {
    root: usize,
    src: PathBuf,
    rel: String,
    stored: String,
    size: u64,
    mtime: Option<i64>,
    mode: Option<u32>,
}

pub struct Plan {
    roots: Vec<PlannedRoot>,
    files: Vec<PlannedFile>,
    links: Vec<LinkEntry>,
    pub summary: Preflight,
}

fn portable(s: &str) -> String {
    let s = s.trim_start_matches('.');
    let s = if s.is_empty() { "item" } else { s };
    if portable_component(s) {
        s.to_string()
    } else {
        sanitize_component(s)
    }
}

fn unique(stem: String, used: &mut HashSet<String>) -> String {
    let mut candidate = stem.clone();
    let mut n = 2;
    while !used.insert(candidate.to_lowercase()) {
        candidate = format!("{stem}~{n}");
        n += 1;
    }
    candidate
}

fn mtime_mode(md: &fs::Metadata) -> (Option<i64>, Option<u32>) {
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64);
    #[cfg(unix)]
    let mode = Some(std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o777);
    #[cfg(not(unix))]
    let mode = None;
    (mtime, mode)
}

struct Walk<'a> {
    files: Vec<PlannedFile>,
    links: Vec<LinkEntry>,
    excluded: ExcludedSummary,
    used: HashSet<String>,
    secrets_included: u64,
    include_secrets: &'a [PathBuf],
    home: &'a Path,
}

impl Walk<'_> {
    fn unreadable(&mut self, what: String) {
        self.excluded.unreadable += 1;
        if self.excluded.unreadable_examples.len() < EXAMPLES {
            self.excluded.unreadable_examples.push(what);
        }
    }

    /// Returns false if the path is left out (and records why).
    fn keep(&mut self, path: &Path, excludes: &[PathBuf]) -> bool {
        if excludes.iter().any(|x| path.starts_with(x)) {
            self.excluded.by_user += 1;
            return false;
        }
        match classify(path) {
            Some(Flag::Secret(_)) if self.include_secrets.iter().any(|p| path.starts_with(p)) => {
                self.secrets_included += 1;
                true
            }
            Some(Flag::Secret(why)) => {
                self.excluded.secrets += 1;
                if self.excluded.secret_examples.len() < EXAMPLES {
                    self.excluded
                        .secret_examples
                        .push(format!("{} ({why})", tilde(path, self.home)));
                }
                false
            }
            Some(Flag::Transient(_)) => {
                self.excluded.transient += 1;
                false
            }
            None => true,
        }
    }

    fn root(&mut self, idx: usize, root: &Root, src: &Path, excludes: &[PathBuf]) {
        let Ok(md) = fs::metadata(src) else {
            self.unreadable(tilde(src, self.home));
            return;
        };
        if md.is_file() {
            if self.keep(src, excludes) {
                let name = src
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let stored = unique(format!("{}/{}", root.dir, portable(&name)), &mut self.used);
                let (mtime, mode) = mtime_mode(&md);
                self.files.push(PlannedFile {
                    root: idx,
                    src: src.into(),
                    rel: String::new(),
                    stored,
                    size: md.len(),
                    mtime,
                    mode,
                });
            }
            return;
        }
        let mut it = walkdir::WalkDir::new(src)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter();
        while let Some(next) = it.next() {
            let e = match next {
                Ok(e) => e,
                Err(err) => {
                    let p = err.path().map(|p| tilde(p, self.home)).unwrap_or_default();
                    self.unreadable(p);
                    continue;
                }
            };
            if e.depth() == 0 {
                continue;
            }
            let ft = e.file_type();
            if !self.keep(e.path(), excludes) {
                if ft.is_dir() {
                    it.skip_current_dir();
                }
                continue;
            }
            if ft.is_dir() {
                continue;
            }
            let Some(rel) = rel_string(e.path(), src) else {
                self.unreadable(format!(
                    "{} (name is not valid Unicode)",
                    e.path().to_string_lossy()
                ));
                continue;
            };
            if ft.is_symlink() {
                match fs::read_link(e.path())
                    .ok()
                    .and_then(|t| t.to_str().map(str::to_string))
                {
                    Some(target) => self.links.push(LinkEntry {
                        root: root.id.clone(),
                        rel,
                        target,
                    }),
                    None => self.unreadable(tilde(e.path(), self.home)),
                }
                continue;
            }
            if !ft.is_file() {
                self.excluded.special += 1;
                continue;
            }
            let Ok(md) = e.metadata() else {
                self.unreadable(tilde(e.path(), self.home));
                continue;
            };
            let stored = unique(
                format!(
                    "{}/{}",
                    root.dir,
                    rel.split('/')
                        .map(portable_keep_dots)
                        .collect::<Vec<_>>()
                        .join("/")
                ),
                &mut self.used,
            );
            let (mtime, mode) = mtime_mode(&md);
            self.files.push(PlannedFile {
                root: idx,
                src: e.path().into(),
                rel,
                stored,
                size: md.len(),
                mtime,
                mode,
            });
        }
    }
}

/// Like `portable` but keeps leading dots (file names inside a root stay recognizable).
fn portable_keep_dots(s: &str) -> String {
    if portable_component(s) {
        s.to_string()
    } else {
        sanitize_component(s)
    }
}

fn install_hint(app: &App, target: Platform, source: Platform, home: &Path) -> (Status, String) {
    let why = match installers::plan(app, home, source, target) {
        Ok(what) => {
            return (
                Status::Ready,
                format!("Bundled for offline install: {what}."),
            );
        }
        Err(why) => why,
    };
    if matches!(app.source, Source::AppImage | Source::Custom) && target.arch != source.arch {
        return (
            Status::Unsupported,
            format!(
                "{} was built for {}; it cannot run on {}.",
                app.name, source.arch, target.arch
            ),
        );
    }
    if target.os == source.os {
        return (Status::Manual, why);
    }
    let options: Vec<String> = app
        .canonical
        .as_deref()
        .and_then(registry::get)
        .map(|e| {
            e.packages
                .iter()
                .filter(|(m, _)| registry::method_os(m) == Some(target.os))
                .map(|(m, ids)| format!("{m} {}", ids.join(" / ")))
                .collect()
        })
        .unwrap_or_default();
    if options.is_empty() {
        (
            Status::Unsupported,
            format!(
                "No known {} version of {}. It will be skipped.",
                target.os, app.name
            ),
        )
    } else {
        (
            Status::Manual,
            format!(
                "{why} On {}, install {} with {}.",
                target.os,
                app.name,
                options.join(" or ")
            ),
        )
    }
}

pub fn part_label(p: Part) -> &'static str {
    match p {
        Part::Installer => "installer",
        Part::Config => "settings",
        Part::Data => "data",
    }
}

fn coverage(inv: &Inventory, sel: &Selection, home: &Path) -> Vec<Coverage> {
    let source = inv.device.platform;
    let mut cov = Vec::new();
    for &target in &sel.targets {
        for c in &sel.apps {
            let Some(app) = inv.apps.iter().find(|a| a.id == c.id) else {
                continue;
            };
            let mut push = |part, (status, reason): (Status, String)| {
                cov.push(Coverage {
                    app: app.id.clone(),
                    target,
                    part,
                    status,
                    reason,
                });
            };
            if c.installer {
                push(Part::Installer, install_hint(app, target, source, home));
            }
            for (wanted, kind, part, label) in [
                (c.config, LocKind::Config, Part::Config, "settings"),
                (c.data, LocKind::Data, Part::Data, "data"),
            ] {
                if !wanted {
                    continue;
                }
                let n = app.locations.iter().filter(|l| l.kind == kind).count();
                let adapter = app
                    .canonical
                    .as_deref()
                    .and_then(adapters::for_app)
                    .filter(|a| {
                        app.locations
                            .iter()
                            .any(|l| l.kind == kind && a.covers(source.os, &tilde(&l.path, home)))
                    });
                push(
                    part,
                    if n == 0 {
                        (
                            Status::Unsupported,
                            format!("No {label} locations are known for {}.", app.name),
                        )
                    } else if target.os == source.os {
                        (
                            Status::Ready,
                            format!(
                                "Copied and restored to the same place ({n} location{}).",
                                if n == 1 { "" } else { "s" }
                            ),
                        )
                    } else if let Some(a) = adapter {
                        (
                            Status::Ready,
                            format!(
                                "Converted for {} by the {} adapter into {}; files tied to the old computer stay in the snapshot.",
                                target.os,
                                a.name,
                                a.folder(target.os)
                            ),
                        )
                    } else {
                        (
                            Status::Unsupported,
                            format!(
                                "No {label} converter from {} to {} for {} yet; the files stay in the snapshot for manual use.",
                                source.os, target.os, app.name
                            ),
                        )
                    },
                );
            }
        }
    }
    cov
}

fn readiness(
    inv: &Inventory,
    sel: &Selection,
    engines: &Engines,
    cov: &[Coverage],
) -> Vec<Readiness> {
    sel.targets
        .iter()
        .map(|&t| {
            let engine = engines.contains_key(&t);
            let mut reasons = Vec::new();
            if !engine {
                reasons.push(format!("No restore program for {t} is available on this computer, so the snapshot cannot restore itself there."));
            }
            for c in cov.iter().filter(|c| c.target == t && c.status != Status::Ready) {
                let name = inv.apps.iter().find(|a| a.id == c.app).map_or(c.app.as_str(), |a| a.name.as_str());
                reasons.push(format!("{name} ({}): {}", part_label(c.part), c.reason));
            }
            Readiness { target: t, offline_ready: reasons.is_empty(), engine_included: engine, reasons }
        })
        .collect()
}

/// Rough size of the installers that will be collected (sizes are only known afterwards).
fn installer_estimate(inv: &Inventory, cov: &[Coverage]) -> u64 {
    let source = inv.device.platform;
    let ready: Vec<&App> = cov
        .iter()
        .filter(|c| c.part == Part::Installer && c.target == source && c.status == Status::Ready)
        .filter_map(|c| inv.apps.iter().find(|a| a.id == c.app))
        .collect();
    let flatpaks: Vec<&str> = ready
        .iter()
        .filter(|a| a.source == Source::Flatpak)
        .map(|a| a.native_id.as_str())
        .collect();
    let others: u64 = ready
        .iter()
        .filter(|a| a.source != Source::Flatpak)
        .filter_map(|a| a.install_size)
        .sum();
    others
        + if flatpaks.is_empty() {
            0
        } else {
            installers::flatpak_estimate(&flatpaks)
        }
}

/// Works out exactly what would be copied. Reads only metadata; never file contents.
pub fn plan(
    inv: &Inventory,
    sel: &Selection,
    home: &Path,
    destination: Option<&Path>,
    engines: &Engines,
) -> Result<Plan> {
    sel.validate(inv)?;
    let home = home.to_path_buf();
    let mut warnings = Vec::new();
    let mut used_dirs = HashSet::new();
    // (root, source path, excludes)
    let mut wanted: Vec<(PlannedRoot, PathBuf, Vec<PathBuf>)> = Vec::new();
    let mut next_id = 0;
    let mut add = |kind,
                   app: Option<&App>,
                   known,
                   src: &Path,
                   excludes: Vec<PathBuf>,
                   label: String,
                   used: &mut HashSet<String>| {
        let base = match kind {
            RootKind::Config => "app-configurations",
            RootKind::Data => "app-data",
            RootKind::Folder => "personal-files",
        };
        let leaf = src
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".into());
        let stem = match (app, known) {
            (Some(a), _) => format!("{base}/{}/{}", portable(&a.name), portable(&leaf)),
            (None, Some(k)) => format!("{base}/{k:?}"),
            (None, None) => format!("{base}/{}", portable(&leaf)),
        };
        let root = Root {
            id: format!("r{next_id}"),
            kind,
            app: app.map(|a| a.id.clone()),
            known,
            original: tilde(src, &home),
            dir: unique(stem, used),
            is_file: src.is_file(),
        };
        next_id += 1;
        wanted.push((PlannedRoot { root, label }, src.to_path_buf(), excludes));
    };

    for c in &sel.apps {
        let Some(app) = inv.apps.iter().find(|a| a.id == c.id) else {
            continue;
        };
        for loc in &app.locations {
            let (on, kind, what) = match loc.kind {
                LocKind::Config => (c.config, RootKind::Config, "settings"),
                LocKind::Data => (c.data, RootKind::Data, "data"),
            };
            if !on {
                continue;
            }
            if !loc.path.exists() {
                warnings.push(format!(
                    "{}: {} no longer exists and was skipped.",
                    app.name,
                    tilde(&loc.path, &home)
                ));
                continue;
            }
            let label = format!("{} — {what} ({})", app.name, tilde(&loc.path, &home));
            add(
                kind,
                Some(app),
                None,
                &loc.path,
                c.exclude.clone(),
                label,
                &mut used_dirs,
            );
        }
        let has = |k| app.locations.iter().any(|l| l.kind == k);
        if c.config && !has(LocKind::Config) || c.data && !has(LocKind::Data) {
            warnings.push(format!(
                "{}: no known location for some selected parts; add one in Applications if needed.",
                app.name
            ));
        }
    }
    for f in &sel.folders {
        add(
            RootKind::Folder,
            None,
            f.known,
            &f.path,
            f.exclude.clone(),
            tilde(&f.path, &home),
            &mut used_dirs,
        );
    }

    // A root inside another selected root would be copied twice; keep the outer one.
    let sources: Vec<(PathBuf, Vec<PathBuf>)> =
        wanted.iter().map(|w| (w.1.clone(), w.2.clone())).collect();
    let mut kept = Vec::new();
    for (i, w) in wanted.into_iter().enumerate() {
        let outer = sources.iter().enumerate().find(|(j, (s, ex))| {
            *j != i
                && w.1.starts_with(s)
                && (w.1 != *s || *j < i)
                && !ex.iter().any(|x| w.1.starts_with(x))
        });
        match outer {
            Some((_, (s, _))) => warnings.push(format!(
                "{} is already included as part of {}.",
                w.0.label,
                tilde(s, &home)
            )),
            None => kept.push(w),
        }
    }

    let mut walk = Walk {
        files: Vec::new(),
        links: Vec::new(),
        excluded: ExcludedSummary::default(),
        used: HashSet::new(),
        secrets_included: 0,
        include_secrets: &sel.include_secrets,
        home: &home,
    };
    let mut roots = Vec::new();
    for (i, (pr, src, excludes)) in kept.into_iter().enumerate() {
        let mut pr = pr;
        pr.root.id = format!("r{i}");
        walk.root(i, &pr.root, &src, &excludes);
        roots.push(pr);
    }
    if walk.secrets_included > 0 {
        warnings.push(format!(
            "{} file(s) that may contain passwords or keys are included because you chose them. Encrypt the snapshot, or keep the drive safe.",
            walk.secrets_included
        ));
    }

    let coverage = coverage(inv, sel, &home);
    let readiness = readiness(inv, sel, engines, &coverage);
    let installer_bytes = installer_estimate(inv, &coverage);
    let summaries: Vec<RootSummary> = roots
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let (files, bytes) = walk
                .files
                .iter()
                .filter(|f| f.root == i)
                .fold((0, 0), |(n, b), f| (n + 1, b + f.size));
            RootSummary {
                id: r.root.id.clone(),
                kind: r.root.kind,
                app: r.root.app.clone(),
                label: r.label.clone(),
                original: r.root.original.clone(),
                files,
                bytes,
            }
        })
        .collect();
    let total_bytes: u64 = walk.files.iter().map(|f| f.size).sum();
    let engine_bytes: u64 = sel
        .targets
        .iter()
        .filter_map(|t| engines.get(t))
        .filter_map(|p| fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    // Manifest, checksums and reports, plus file-system overhead.
    let required_bytes =
        total_bytes + installer_bytes + engine_bytes + walk.files.len() as u64 * 512 + (16 << 20);
    let destination_free = destination.and_then(free_space);
    let summary = Preflight {
        roots: summaries,
        total_files: walk.files.len() as u64,
        total_bytes,
        engine_bytes,
        installer_bytes,
        excluded: walk.excluded,
        coverage,
        readiness,
        warnings,
        destination_free,
        required_bytes,
        fits: destination_free.is_none_or(|f| f >= required_bytes),
    };
    Ok(Plan {
        roots,
        files: walk.files,
        links: walk.links,
        summary,
    })
}

// ---------------------------------------------------------------- copying and hashing

struct Meter<'a> {
    emit: &'a mut dyn FnMut(Progress),
    last: Instant,
    done_bytes: u64,
    total_bytes: u64,
    done_files: u64,
    total_files: u64,
    current: String,
}

impl Meter<'_> {
    fn stage(&mut self, label: &str, step: u32, steps: u32) {
        (self.emit)(Progress::Stage {
            label: label.into(),
            step,
            steps,
        });
    }

    fn tick(&mut self, force: bool) {
        if force || self.last.elapsed() >= Duration::from_millis(100) {
            self.last = Instant::now();
            (self.emit)(Progress::Copy {
                done_bytes: self.done_bytes,
                total_bytes: self.total_bytes,
                done_files: self.done_files,
                total_files: self.total_files,
                current: self.current.clone(),
            });
        }
    }

    fn note(&mut self, message: String) {
        (self.emit)(Progress::Note { message });
    }

    fn warn(&mut self, message: String) {
        (self.emit)(Progress::Warning { message });
    }
}

pub(crate) enum CopyError {
    Read(std::io::Error),
    Write(std::io::Error),
    Cancelled,
}

/// Streams `src` into `dst` while hashing; returns (bytes, sha256 hex).
pub(crate) fn copy_hashed(
    src: &mut impl Read,
    dst: &mut impl Write,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> std::result::Result<(u64, String), CopyError> {
    let mut buf = vec![0u8; 1 << 20];
    let mut h = Sha256::new();
    let mut total = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(CopyError::Cancelled);
        }
        let n = src.read(&mut buf).map_err(CopyError::Read)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        dst.write_all(&buf[..n]).map_err(CopyError::Write)?;
        total += n as u64;
        on_bytes(n as u64);
    }
    dst.flush().map_err(CopyError::Write)?;
    Ok((total, format!("{:x}", h.finalize())))
}

/// Like [`copy_hashed`], but stores the copy encrypted; also returns the stored copy's hash.
fn encrypt_copy(
    key: &Key,
    src: &mut impl Read,
    out: &mut File,
    cancel: &AtomicBool,
    on_bytes: impl FnMut(u64),
) -> std::result::Result<(u64, String, Option<String>), CopyError> {
    let hashing = Hashing {
        inner: out,
        h: Sha256::new(),
        n: 0,
    };
    let mut w = key
        .encrypt_to(hashing)
        .map_err(|e| CopyError::Write(std::io::Error::other(e.to_string())))?;
    let (n, sha) = copy_hashed(src, &mut w, cancel, on_bytes)?;
    let hashing = w.finish().map_err(CopyError::Write)?;
    Ok((n, sha, Some(format!("{:x}", hashing.h.finalize()))))
}

pub(crate) fn hash_file(
    path: &Path,
    cancel: &AtomicBool,
    mut on_bytes: impl FnMut(u64),
) -> Result<(u64, String)> {
    let mut f = File::open(path).ctx(|| format!("Reading {}", path.display()))?;
    let mut buf = vec![0u8; 1 << 20];
    let mut h = Sha256::new();
    let mut total = 0;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let n = f
            .read(&mut buf)
            .ctx(|| format!("Reading {}", path.display()))?;
        if n == 0 {
            return Ok((total, format!("{:x}", h.finalize())));
        }
        h.update(&buf[..n]);
        total += n as u64;
        on_bytes(n as u64);
    }
}

#[derive(Serialize, Deserialize)]
struct JournalLine {
    stored: String,
    src_size: u64,
    src_mtime: Option<i64>,
    size: u64,
    sha256: String,
    #[serde(default)]
    stored_sha256: Option<String>,
    #[serde(default)]
    stored_size: Option<u64>,
}

impl JournalLine {
    fn write(&self, out: &mut File) -> Result<()> {
        let mut text = serde_json::to_string(self).map_err(|e| invalid(e.to_string()))?;
        text.push('\n');
        out.write_all(text.as_bytes())
            .ctx(|| "Writing the build journal".into())
    }
}

#[derive(Serialize, Deserialize, PartialEq)]
struct Marker {
    name: String,
    selection: Selection,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Failure {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct BuildReport {
    pub path: PathBuf,
    pub id: String,
    pub name: String,
    pub files: u64,
    pub bytes: u64,
    pub resumed_files: u64,
    pub failed: Vec<Failure>,
    pub warnings: Vec<String>,
    pub excluded: ExcludedSummary,
    pub coverage: Vec<Coverage>,
    pub readiness: Vec<Readiness>,
    /// Files and installers taken over from the base snapshot instead of copied again.
    pub reused_files: u64,
    pub reused_bytes: u64,
    /// For an encrypted snapshot: opens it without the password. Shown once, never stored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_key: Option<String>,
}

pub struct BuildRequest<'a> {
    pub inventory: &'a Inventory,
    pub selection: &'a Selection,
    pub name: &'a str,
    pub destination: &'a Path,
    pub engines: &'a Engines,
    /// Home folder of the user whose files are copied (stored as `~/…` in the manifest).
    pub home: &'a Path,
    /// Encrypt the snapshot with this password (see `crypt`).
    pub password: Option<&'a str>,
    /// An earlier snapshot to update: unchanged files and installers are taken from it (hard
    /// links where the drive supports them) instead of copied again.
    pub base: Option<&'a Path>,
}

/// The snapshot an incremental build starts from.
struct Base {
    dir: PathBuf,
    m: Manifest,
    key: Option<Key>,
}

impl Base {
    fn open(dir: &Path, password: Option<&str>) -> Result<Self> {
        let outer = Manifest::read(dir)?;
        let key = outer.unlock(dir, password).map_err(|e| match e {
            Error::Locked => {
                invalid("The earlier snapshot is encrypted; enter its password to update it.")
            }
            e => e,
        })?;
        let m = Manifest::open(dir, key.as_ref())?;
        Ok(Self {
            dir: dir.to_path_buf(),
            m,
            key,
        })
    }

    /// Puts the base's copy of `stored` at `dst`: a hard link, else a copy.
    fn take(&self, stored: &str, dst: &Path) -> bool {
        let Ok(src) = crate::paths::join_rel(&self.dir, stored, true) else {
            return false;
        };
        if let Some(d) = dst.parent()
            && fs::create_dir_all(d).is_err()
        {
            return false;
        }
        let _ = fs::remove_file(dst);
        fs::hard_link(&src, dst).is_ok() || fs::copy(&src, dst).is_ok()
    }
}

/// Hashes what passes through to `inner` (the encrypted bytes, for an encrypted copy).
struct Hashing<W> {
    inner: W,
    h: Sha256,
    n: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(b)?;
        self.h.update(&b[..n]);
        self.n += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn check_snapshot_name(name: &str) -> Result<()> {
    if name.trim() != name
        || name.is_empty()
        || name.len() > 100
        || !portable_component(name)
        || name.starts_with('.')
    {
        return Err(invalid(
            "Snapshot names cannot be empty, start with a dot, or contain / \\ : * ? \" < > |.",
        ));
    }
    Ok(())
}

pub fn staging_path(destination: &Path, name: &str) -> PathBuf {
    destination.join(format!(".{name}.partial"))
}

/// Removes an interrupted build. Only folders carrying our marker are touched.
pub fn discard_partial(destination: &Path, name: &str) -> Result<()> {
    check_snapshot_name(name)?;
    let staging = staging_path(destination, name);
    if !staging.join(MARKER).is_file() {
        return Err(invalid(
            "There is no interrupted snapshot with that name here.",
        ));
    }
    fs::remove_dir_all(&staging).ctx(|| format!("Removing {}", staging.display()))
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<String> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).ctx(|| format!("Creating {}", d.display()))?;
    }
    let mut f = File::create(path).ctx(|| format!("Writing {}", path.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .ctx(|| format!("Writing {}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

const LAUNCHER_SH: &str = r#"#!/bin/sh
# Simple Setup restore launcher. Runs the restore program bundled in this snapshot;
# nothing is downloaded. Extra arguments (for example --dry-run) are passed through.
set -eu
here=$(cd "$(dirname "$0")/.." && pwd)
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) echo "This computer's processor ($(uname -m)) is not supported." >&2; exit 1 ;;
esac
engine="$here/restore-engine/@OS@-$arch/simple-setup"
if [ ! -f "$engine" ]; then
  echo "This snapshot has no restore program for @OS@-$arch." >&2
  exit 1
fi
if [ ! -x "$engine" ]; then
  # Drives mounted without execute permission: run a private copy instead.
  tmp=$(mktemp -d)
  cp "$engine" "$tmp/simple-setup"
  chmod 700 "$tmp/simple-setup"
  engine="$tmp/simple-setup"
fi
exec "$engine" restore "$here" "$@"
"#;

const LAUNCHER_PS1: &str = r#"# Simple Setup restore launcher. Runs the restore program bundled in this snapshot;
# nothing is downloaded. Start it with:
#   powershell -ExecutionPolicy Bypass -File restore-windows.ps1
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$arch = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64' } else { 'x86_64' }
$engine = Join-Path $root "restore-engine\windows-$arch\simple-setup.exe"
if (-not (Test-Path $engine)) { Write-Error "This snapshot has no restore program for windows-$arch."; exit 1 }
& $engine restore $root @args
exit $LASTEXITCODE
"#;

/// Double-clickable on Windows (a .ps1 file opens in Notepad when double-clicked).
const LAUNCHER_CMD: &str = r#"@echo off
rem Simple Setup restore launcher: double-click to restore this snapshot. Nothing is downloaded.
rem Extra arguments (for example --dry-run) are passed through.
setlocal
set "ARCH=x86_64"
if /I "%PROCESSOR_ARCHITECTURE%"=="ARM64" set "ARCH=aarch64"
pushd "%~dp0.."
set "ENGINE=%CD%\restore-engine\windows-%ARCH%\simple-setup.exe"
if not exist "%ENGINE%" (
  echo This snapshot has no restore program for windows-%ARCH%.
  popd
  pause
  exit /b 1
)
"%ENGINE%" restore "%CD%" %*
set "STATUS=%ERRORLEVEL%"
popd
pause
exit /b %STATUS%
"#;

fn readme(m: &Manifest) -> String {
    let mut s = format!(
        "Simple Setup snapshot \"{}\"\nCreated {} on {} ({}, {}).\n\n\
         This folder holds the offline installers, applications' settings, data and personal\n\
         files chosen when the snapshot was made. Everything is described in simple-setup.json; checksums are in\n\
         checksums/SHA256SUMS (check with: sha256sum -c checksums/SHA256SUMS).\n\nTo restore:\n",
        m.name, m.created_at, m.source.hostname, m.source.os_name, m.source.platform
    );
    let oses: HashSet<OsFamily> = m.targets.iter().map(|t| t.os).collect();
    if oses.contains(&OsFamily::Linux) {
        s += "  Linux:   open a terminal here and run  sh launchers/restore-linux.sh --dry-run  then without --dry-run\n";
    }
    if oses.contains(&OsFamily::Macos) {
        s += "  macOS:   double-click launchers/restore-macos.command (right-click > Open the first time)\n";
    }
    if oses.contains(&OsFamily::Windows) {
        s += "  Windows: double-click launchers\\restore-windows.cmd\n           (or: powershell -ExecutionPolicy Bypass -File launchers\\restore-windows.ps1)\n";
    }
    s += "\nThe restore shows a preview and asks before changing anything. Existing files are never overwritten.\n";
    s
}

fn report_text(r: &BuildReport) -> String {
    let mut s = format!(
        "Snapshot \"{}\" ({})\nFiles: {}  Bytes: {}\n",
        r.name, r.id, r.files, r.bytes
    );
    for t in &r.readiness {
        s += &format!(
            "\n{}: {}\n",
            t.target,
            if t.offline_ready {
                "ready to restore offline"
            } else {
                "NOT fully offline-ready"
            }
        );
        for why in &t.reasons {
            s += &format!("  - {why}\n");
        }
    }
    let e = &r.excluded;
    s += &format!(
        "\nLeft out: {} secret item(s), {} cache/lock item(s), {} excluded by you, {} unreadable, {} special file(s)\n",
        e.secrets, e.transient, e.by_user, e.unreadable, e.special
    );
    for f in &r.failed {
        s += &format!("FAILED {}: {}\n", f.path, f.error);
    }
    for w in &r.warnings {
        s += &format!("Warning: {w}\n");
    }
    s
}

/// Collects the planned installers for every selected target, reusing ones an interrupted build
/// finished. If collection fails, or the installer cannot run unattended, the app falls back to
/// manual instructions and the snapshot is no longer reported offline-ready for that target.
/// The earlier snapshot's installer for `app` on `target`, if the installed version is unchanged.
fn take_installer(b: &Base, app: &App, target: Platform, staging: &Path) -> Option<Installer> {
    let old = b.m.apps.iter().find(|a| a.id == app.id)?;
    if app.version.is_none() || old.version != app.version {
        return None;
    }
    let i =
        b.m.installers
            .iter()
            .find(|i| i.app == app.id && i.platform == target)?;
    for f in i.recipe.files() {
        let dst = staging.join(f.stored.split('/').collect::<PathBuf>());
        if !(fs::metadata(&dst).is_ok_and(|m| m.len() == f.size) || b.take(&f.stored, &dst)) {
            return None;
        }
    }
    let mut i = i.clone();
    i.evidence.push(format!(
        "Taken from the earlier snapshot \"{}\": {} {} is still the installed version.",
        b.m.name,
        app.name,
        app.version.as_deref().unwrap_or_default()
    ));
    Some(i)
}

#[allow(clippy::too_many_arguments)]
fn collect_installers(
    req: &BuildRequest,
    staging: &Path,
    earlier: Option<&Base>,
    coverage: &mut [Coverage],
    meter: &mut Meter,
    cancel: &AtomicBool,
    warnings: &mut Vec<String>,
    reused: (&mut u64, &mut u64),
) -> Result<Vec<Installer>> {
    let source = req.inventory.device.platform;
    let path = staging.join(INSTALLER_JOURNAL);
    let on_disk = |i: &Installer| {
        i.recipe.files().iter().all(|f| {
            fs::metadata(staging.join(f.stored.split('/').collect::<PathBuf>()))
                .is_ok_and(|m| m.len() == f.size)
        })
    };
    let mut done: Vec<Installer> = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Installer>(l).ok())
        .filter(on_disk)
        .collect();
    let mut shared = Shared::new();
    for i in &done {
        if let Recipe::Flatpak { bundles, .. } = &i.recipe {
            for b in bundles {
                shared.insert(b.name.clone(), b.file.clone());
            }
        }
    }
    let todo: Vec<(&App, Platform)> = coverage
        .iter()
        .filter(|c| c.part == Part::Installer && c.status == Status::Ready)
        .filter_map(|c| Some((req.inventory.apps.iter().find(|a| a.id == c.app)?, c.target)))
        .collect();
    let mut journal = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ctx(|| "Opening the installer journal".into())?;
    let mut out = Vec::new();
    meter.total_files = todo.len() as u64;
    meter.total_bytes = todo.len() as u64;
    for (n, &(app, target)) in todo.iter().enumerate() {
        meter.done_files = n as u64;
        meter.done_bytes = n as u64;
        let resumed = done
            .iter()
            .position(|i| i.app == app.id && i.platform == target)
            .map(|i| done.swap_remove(i));
        let taken = match (&resumed, earlier) {
            (None, Some(b)) => take_installer(b, app, target, staging),
            _ => None,
        };
        if let Some(i) = &taken {
            *reused.0 += i.recipe.files().len() as u64;
            *reused.1 += i.recipe.files().iter().map(|f| f.size).sum::<u64>();
            let mut line = serde_json::to_string(i).map_err(|e| invalid(e.to_string()))?;
            line.push('\n');
            journal
                .write_all(line.as_bytes())
                .ctx(|| "Writing the installer journal".into())?;
        }
        let result = match resumed.or(taken) {
            Some(inst) => Ok(inst),
            None => {
                meter.current = format!("{} ({target})", app.name);
                meter.tick(true);
                let mut status = |s: String| meter.note(format!("{}: {s}", app.name));
                installers::collect(
                    app,
                    source,
                    target,
                    &mut Collect {
                        staging,
                        home: req.home,
                        cancel,
                        shared: &mut shared,
                        status: &mut status,
                    },
                )
                .and_then(|(recipe, evidence)| {
                    let inst = Installer {
                        app: app.id.clone(),
                        platform: target,
                        recipe,
                        evidence,
                    };
                    let mut line =
                        serde_json::to_string(&inst).map_err(|e| invalid(e.to_string()))?;
                    line.push('\n');
                    journal
                        .write_all(line.as_bytes())
                        .ctx(|| "Writing the installer journal".into())?;
                    Ok(inst)
                })
            }
        };
        let cov = coverage
            .iter_mut()
            .find(|c| c.app == app.id && c.part == Part::Installer && c.target == target);
        match result {
            Ok(inst) => {
                if let (Some(c), Some(why)) = (cov, installers::by_hand(&inst.recipe)) {
                    c.status = Status::Manual;
                    c.reason = why;
                }
                out.push(inst);
            }
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => {
                let msg = format!(
                    "{} ({target}): the installer could not be prepared ({e}).",
                    app.name
                );
                meter.warn(msg.clone());
                warnings.push(msg);
                if let Some(c) = cov {
                    c.status = Status::Manual;
                    c.reason = format!(
                        "The installer could not be prepared ({e}). Install {} on the new computer from its usual source.",
                        app.name
                    );
                }
            }
        }
    }
    meter.done_files = todo.len() as u64;
    meter.done_bytes = todo.len() as u64;
    meter.current.clear();
    meter.tick(true);
    Ok(out)
}

pub fn build(
    req: &BuildRequest,
    emit: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<BuildReport> {
    const STEPS: u32 = 6;
    check_snapshot_name(req.name)?;
    let final_dir = req.destination.join(req.name);
    if final_dir.exists() {
        return Err(invalid(format!(
            "{} already exists. Choose another name.",
            final_dir.display()
        )));
    }
    let mut meter = Meter {
        emit,
        last: Instant::now(),
        done_bytes: 0,
        total_bytes: 0,
        done_files: 0,
        total_files: 0,
        current: String::new(),
    };
    meter.stage("Checking your selection", 1, STEPS);
    let mut plan = plan(
        req.inventory,
        req.selection,
        req.home,
        Some(req.destination),
        req.engines,
    )?;

    let staging = staging_path(req.destination, req.name);
    let marker = Marker {
        name: req.name.into(),
        selection: req.selection.clone(),
    };
    let mut journal: HashMap<String, JournalLine> = HashMap::new();
    if staging.exists() {
        let found: Option<Marker> = fs::read(staging.join(MARKER))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        if found.as_ref() != Some(&marker) {
            return Err(invalid(format!(
                "An interrupted snapshot named \"{}\" with a different selection is in this folder. Discard it or pick another name.",
                req.name
            )));
        }
        if let Ok(f) = File::open(staging.join(JOURNAL)) {
            for line in BufReader::new(f).lines().map_while(std::result::Result::ok) {
                if let Ok(j) = serde_json::from_str::<JournalLine>(&line) {
                    journal.insert(j.stored.clone(), j);
                }
            }
        }
    } else {
        fs::create_dir_all(&staging).ctx(|| format!("Creating {}", staging.display()))?;
        let bytes = serde_json::to_vec(&marker).map_err(|e| invalid(e.to_string()))?;
        write_new(&staging.join(MARKER), &bytes)?;
    }
    let earlier = req.base.map(|b| Base::open(b, req.password)).transpose()?;
    // The key an interrupted build already sealed, the earlier snapshot's, or a new one.
    let key_file = staging.join(crypt::KEY_FILE);
    let key = match req.password {
        None if key_file.is_file() => {
            return Err(invalid(
                "This interrupted snapshot is encrypted; enter its password to continue.",
            ));
        }
        None => None,
        Some(pw) if key_file.is_file() => Some(Key::unlock(&staging, pw, None)?),
        Some(pw) => {
            let k = earlier
                .as_ref()
                .and_then(|b| b.key.clone())
                .unwrap_or_else(Key::generate);
            write_new(&key_file, &k.sealed_with(pw)?)?;
            Some(k)
        }
    };
    // Files are taken over only when both snapshots store them the same way.
    let same_way =
        |b: &&Base| b.key.as_ref().map(Key::recipient) == key.as_ref().map(Key::recipient);
    let reusable: HashMap<(&str, &str), &FileEntry> = earlier
        .as_ref()
        .filter(same_way)
        .map(|b| {
            b.m.files
                .iter()
                .filter_map(|f| {
                    let r = b.m.roots.iter().find(|r| r.id == f.root)?;
                    Some(((r.original.as_str(), f.rel.as_str()), f))
                })
                .collect()
        })
        .unwrap_or_default();
    let (mut reused_files, mut reused_bytes) = (0u64, 0u64);
    if let Some(k) = &key {
        // Names of apps, folders and files stay inside the encrypted manifest; on the drive
        // every copy is data/<keyed hash of where it came from>.
        for pf in &mut plan.files {
            let h = k.opaque_name(&format!(
                "{}\n{}",
                plan.roots[pf.root].root.original, pf.rel
            ));
            pf.stored = format!("data/{}/{}", &h[..2], &h[2..]);
        }
        for r in &mut plan.roots {
            r.root.dir = "data".into();
        }
    }
    let already: u64 = journal.values().map(|j| j.size).sum();
    if let Some(free) = plan.summary.destination_free
        && free + already < plan.summary.required_bytes
    {
        return Err(invalid(format!(
            "Not enough space: {} bytes needed, {} free at the destination.",
            plan.summary.required_bytes.saturating_sub(already),
            free
        )));
    }
    let mut journal_out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(staging.join(JOURNAL))
        .ctx(|| "Opening the build journal".into())?;

    let mut warnings = plan.summary.warnings.clone();
    let mut coverage = plan.summary.coverage.clone();
    meter.stage("Preparing installers", 2, STEPS);
    let installers = collect_installers(
        req,
        &staging,
        earlier.as_ref(),
        &mut coverage,
        &mut meter,
        cancel,
        &mut warnings,
        (&mut reused_files, &mut reused_bytes),
    )?;
    let readiness = readiness(req.inventory, req.selection, req.engines, &coverage);

    meter.stage("Copying selected files", 3, STEPS);
    meter.done_bytes = 0;
    meter.done_files = 0;
    meter.total_bytes = plan.summary.total_bytes;
    meter.total_files = plan.files.len() as u64;
    let mut entries = Vec::with_capacity(plan.files.len());
    let mut failed = Vec::new();
    let mut resumed = 0;
    for pf in &plan.files {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        let root = &plan.roots[pf.root].root;
        meter.current = pf.src.to_string_lossy().into_owned();
        meter.tick(false);
        let dst = staging.join(pf.stored.split('/').collect::<PathBuf>());
        let entry = |size, sha256, stored_sha256| FileEntry {
            root: root.id.clone(),
            rel: pf.rel.clone(),
            stored: pf.stored.clone(),
            size,
            sha256,
            mtime: pf.mtime,
            mode: pf.mode,
            stored_sha256,
        };
        if let Some(j) = journal.get(&pf.stored).filter(|j| {
            j.src_size == pf.size
                && j.src_mtime == pf.mtime
                && j.stored_sha256.is_some() == key.is_some()
                && fs::metadata(&dst).is_ok_and(|m| m.len() == j.stored_size.unwrap_or(j.size))
        }) {
            entries.push(entry(j.size, j.sha256.clone(), j.stored_sha256.clone()));
            resumed += 1;
            meter.done_bytes += pf.size;
            meter.done_files += 1;
            continue;
        }
        // Unchanged since the earlier snapshot (same size and modification time): take its copy.
        if let (Some(b), Some(old)) = (
            &earlier,
            reusable.get(&(root.original.as_str(), pf.rel.as_str())),
        ) && old.size == pf.size
            && old.mtime.is_some()
            && old.mtime == pf.mtime
            && b.take(&old.stored, &dst)
        {
            JournalLine {
                stored: pf.stored.clone(),
                src_size: pf.size,
                src_mtime: pf.mtime,
                size: old.size,
                sha256: old.sha256.clone(),
                stored_sha256: old.stored_sha256.clone(),
                stored_size: fs::metadata(&dst).ok().map(|m| m.len()),
            }
            .write(&mut journal_out)?;
            entries.push(entry(
                old.size,
                old.sha256.clone(),
                old.stored_sha256.clone(),
            ));
            reused_files += 1;
            reused_bytes += pf.size;
            meter.done_bytes += pf.size;
            meter.done_files += 1;
            continue;
        }
        let mut src = match File::open(&pf.src) {
            Ok(f) => f,
            Err(e) => {
                failed.push(Failure {
                    path: pf.src.to_string_lossy().into(),
                    error: e.to_string(),
                });
                continue;
            }
        };
        if let Some(d) = dst.parent() {
            fs::create_dir_all(d).ctx(|| format!("Creating {}", d.display()))?;
        }
        let mut out = File::create(&dst).ctx(|| format!("Writing {}", dst.display()))?;
        let base = meter.done_bytes;
        let mut copied = 0;
        let progress = |n| {
            copied += n;
            meter.done_bytes = base + copied;
            meter.tick(false);
        };
        let result = match &key {
            None => copy_hashed(&mut src, &mut out, cancel, progress).map(|(n, h)| (n, h, None)),
            Some(k) => encrypt_copy(k, &mut src, &mut out, cancel, progress),
        }
        .and_then(|r| out.sync_all().map(|()| r).map_err(CopyError::Write));
        match result {
            Ok((size, sha, stored_sha)) => {
                let after = fs::metadata(&pf.src).ok();
                if after.is_none_or(|m| m.len() != pf.size || mtime_mode(&m).0 != pf.mtime) {
                    let msg = format!(
                        "{} changed while it was copied; the app may be running. Close it and build again for a consistent copy.",
                        tilde(&pf.src, req.home)
                    );
                    meter.warn(msg.clone());
                    warnings.push(msg);
                }
                JournalLine {
                    stored: pf.stored.clone(),
                    src_size: pf.size,
                    src_mtime: pf.mtime,
                    size,
                    sha256: sha.clone(),
                    stored_sha256: stored_sha.clone(),
                    stored_size: fs::metadata(&dst).ok().map(|m| m.len()),
                }
                .write(&mut journal_out)?;
                entries.push(entry(size, sha, stored_sha));
                meter.done_bytes = base + pf.size.max(copied);
            }
            Err(CopyError::Cancelled) => return Err(Error::Cancelled),
            Err(CopyError::Write(e)) => {
                return Err(e)
                    .ctx(|| format!("Writing {} (is the drive full or removed?)", dst.display()));
            }
            Err(CopyError::Read(e)) => {
                drop(out);
                let _ = fs::remove_file(&dst);
                failed.push(Failure {
                    path: pf.src.to_string_lossy().into(),
                    error: e.to_string(),
                });
                meter.done_bytes = base + pf.size;
            }
        }
        meter.done_files += 1;
    }
    meter.tick(true);

    meter.stage("Verifying copies", 4, STEPS);
    meter.done_bytes = 0;
    meter.done_files = 0;
    meter.total_bytes = entries.iter().map(|e| e.size).sum();
    meter.total_files = entries.len() as u64;
    for e in &entries {
        let p = staging.join(e.stored.split('/').collect::<PathBuf>());
        meter.current = e.stored.clone();
        let base = meter.done_bytes;
        let mut seen = 0;
        let (_, sha) = hash_file(&p, cancel, |n| {
            seen += n;
            meter.done_bytes = base + seen;
            meter.tick(false);
        })?;
        if sha != e.stored_hash() {
            return Err(invalid(format!(
                "{} did not read back correctly from the destination. The drive may be faulty; build again to retry.",
                e.stored
            )));
        }
        meter.done_files += 1;
    }
    meter.tick(true);

    meter.stage("Adding restore tools", 5, STEPS);
    let mut extra: Vec<(String, String)> = Vec::new(); // (sha, path) for SHA256SUMS
    for &t in &req.selection.targets {
        let Some(src) = req.engines.get(&t) else {
            continue;
        };
        let rel = format!("{ENGINE_DIR}/{t}/{}", engine_file(t));
        let dst = staging.join(&rel);
        if let Some(d) = dst.parent() {
            fs::create_dir_all(d).ctx(|| format!("Creating {}", d.display()))?;
        }
        let mut out = File::create(&dst).ctx(|| format!("Writing {}", dst.display()))?;
        let mut input = File::open(src).ctx(|| format!("Reading {}", src.display()))?;
        let (_, sha) = match copy_hashed(&mut input, &mut out, cancel, |_| {}) {
            Ok(v) => v,
            Err(CopyError::Cancelled) => return Err(Error::Cancelled),
            Err(CopyError::Read(e) | CopyError::Write(e)) => {
                return Err(e).ctx(|| format!("Copying restore program for {t}"));
            }
        };
        #[cfg(unix)]
        fs::set_permissions(&dst, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .ctx(|| format!("Setting permissions on {rel}"))?;
        extra.push((sha, rel));
    }
    let oses: HashSet<OsFamily> = req.selection.targets.iter().map(|t| t.os).collect();
    for (os, file, text) in [
        (
            OsFamily::Linux,
            "launchers/restore-linux.sh",
            LAUNCHER_SH.replace("@OS@", "linux"),
        ),
        (
            OsFamily::Macos,
            "launchers/restore-macos.command",
            LAUNCHER_SH.replace("@OS@", "macos"),
        ),
        (
            OsFamily::Windows,
            "launchers/restore-windows.ps1",
            LAUNCHER_PS1.to_string(),
        ),
        (
            OsFamily::Windows,
            "launchers/restore-windows.cmd",
            LAUNCHER_CMD.replace('\n', "\r\n"),
        ),
    ] {
        if oses.contains(&os) {
            let p = staging.join(file);
            extra.push((write_new(&p, text.as_bytes())?, file.into()));
            #[cfg(unix)]
            fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .ctx(|| format!("Setting permissions on {file}"))?;
        }
    }

    meter.stage("Writing manifest and report", 6, STEPS);
    let s = &plan.summary;
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name.into(),
        created_at: crate::now(),
        generator: format!("simple-setup {}", env!("CARGO_PKG_VERSION")),
        source: req.inventory.device.clone(),
        targets: req.selection.targets.clone(),
        selection: req.selection.clone(),
        apps: req
            .selection
            .apps
            .iter()
            .filter_map(|c| {
                let a = req.inventory.apps.iter().find(|a| a.id == c.id)?;
                Some(ManifestApp {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    version: a.version.clone(),
                    source: a.source,
                    native_id: a.native_id.clone(),
                    canonical: a.canonical.clone(),
                    repository: a.repository.clone(),
                    github: a.github.clone(),
                    confidence: a.confidence,
                    evidence: a.evidence.clone(),
                    installer: c.installer,
                    config: c.config,
                    data: c.data,
                })
            })
            .collect(),
        roots: plan.roots.iter().map(|r| r.root.clone()).collect(),
        total_bytes: entries.iter().map(|e| e.size).sum(),
        files: entries,
        installers,
        links: plan.links.clone(),
        excluded: s.excluded.clone(),
        coverage,
        readiness,
        warnings: warnings.clone(),
        encryption: key.as_ref().map(|k| Encryption {
            key_file: crypt::KEY_FILE.into(),
            manifest: crypt::SEALED_MANIFEST.into(),
            recipient: k.recipient(),
        }),
        base: earlier.as_ref().map(|b| BaseRef {
            id: b.m.id.clone(),
            name: b.m.name.clone(),
            reused_files,
            reused_bytes,
        }),
    };
    manifest.validate()?;
    let json = serde_json::to_vec_pretty(&manifest).map_err(|e| invalid(e.to_string()))?;
    let public = match &key {
        None => {
            extra.push((
                write_new(&staging.join(MANIFEST_FILE), &json)?,
                MANIFEST_FILE.into(),
            ));
            manifest.clone()
        }
        Some(k) => {
            let sealed = staging.join(crypt::SEALED_MANIFEST);
            extra.push((
                write_new(&sealed, &k.seal(&json)?)?,
                crypt::SEALED_MANIFEST.into(),
            ));
            let outer = public_manifest(&manifest);
            outer.validate()?;
            let j = serde_json::to_vec_pretty(&outer).map_err(|e| invalid(e.to_string()))?;
            extra.push((
                write_new(&staging.join(MANIFEST_FILE), &j)?,
                MANIFEST_FILE.into(),
            ));
            extra.push((
                hash_file(&key_file, cancel, |_| {})?.1,
                crypt::KEY_FILE.into(),
            ));
            outer
        }
    };

    let report = BuildReport {
        path: final_dir.clone(),
        id: manifest.id.clone(),
        name: manifest.name.clone(),
        files: manifest.files.len() as u64,
        bytes: manifest.total_bytes,
        resumed_files: resumed,
        failed,
        warnings,
        excluded: manifest.excluded.clone(),
        coverage: manifest.coverage.clone(),
        readiness: manifest.readiness.clone(),
        reused_files,
        reused_bytes,
        recovery_key: None,
    };
    extra.push((
        write_new(&staging.join("README.txt"), readme(&public).as_bytes())?,
        "README.txt".into(),
    ));
    let report_json = serde_json::to_vec_pretty(&report).map_err(|e| invalid(e.to_string()))?;
    for (rel, bytes) in [
        ("reports/build-report.json", report_json),
        (
            "reports/build-report.txt",
            report_text(&report).into_bytes(),
        ),
    ] {
        // Reports name apps and folders, so an encrypted snapshot keeps them encrypted too.
        let (rel, bytes) = match &key {
            Some(k) => (format!("{rel}.age"), k.seal(&bytes)?),
            None => (rel.to_string(), bytes),
        };
        extra.push((write_new(&staging.join(&rel), &bytes)?, rel));
    }

    let mut sums: Vec<String> = manifest
        .files
        .iter()
        .map(|f| format!("{}  {}", f.stored_hash(), f.stored))
        .collect();
    let mut seen = HashSet::new();
    for i in &manifest.installers {
        for f in i.recipe.files() {
            if seen.insert(f.stored.as_str()) {
                sums.push(format!("{}  {}", f.sha256, f.stored));
            }
        }
    }
    sums.extend(extra.iter().map(|(h, p)| format!("{h}  {p}")));
    sums.push(String::new());
    write_new(&staging.join(CHECKSUMS), sums.join("\n").as_bytes())?;

    drop(journal_out);
    fs::remove_file(staging.join(JOURNAL)).ctx(|| "Removing the build journal".into())?;
    let _ = fs::remove_file(staging.join(INSTALLER_JOURNAL));
    fs::remove_file(staging.join(MARKER)).ctx(|| "Removing the build marker".into())?;
    if final_dir.exists() {
        return Err(invalid(format!(
            "{} appeared during the build; the finished copy is left at {}.",
            final_dir.display(),
            staging.display()
        )));
    }
    fs::rename(&staging, &final_dir).ctx(|| format!("Finishing {}", final_dir.display()))?;
    let mut report = report;
    report.recovery_key = key.map(|k| k.recovery_key());
    Ok(report)
}

/// What an encrypted snapshot shows without its password: name, source, platforms, readiness.
fn public_manifest(m: &Manifest) -> Manifest {
    Manifest {
        selection: Selection {
            apps: vec![],
            folders: vec![],
            include_secrets: vec![],
            targets: m.targets.clone(),
        },
        apps: vec![],
        roots: vec![],
        files: vec![],
        installers: vec![],
        links: vec![],
        excluded: ExcludedSummary::default(),
        coverage: vec![],
        readiness: m
            .readiness
            .iter()
            .map(|r| Readiness {
                reasons: if r.reasons.is_empty() {
                    vec![]
                } else {
                    vec!["Details are in the encrypted part of the snapshot.".into()]
                },
                ..r.clone()
            })
            .collect(),
        warnings: vec![],
        ..m.clone()
    }
}
