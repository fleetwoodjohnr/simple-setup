//! Restore: read the (untrusted) manifest → plan every item with a status and reason →
//! dry run (the plan itself) → restore only what the user ticked. Files are created with
//! `create_new`, so nothing existing is ever overwritten, and every restored file is checked
//! against its recorded checksum.

use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::adapters::{self, Adapter, Rule};
use crate::build::{CopyError, Failure, Progress, copy_hashed, hash_file, part_label};
use crate::crypt::Key;
use crate::installers::{self, Elevation};
use crate::manifest::{FileEntry, Installer, Manifest, Part, Root, RootKind, Status};
use crate::paths::{join_rel, tilde, untilde};
use crate::platform::{Device, OsFamily, free_space};
use crate::selection::{KnownFolder, Selection};
use crate::{Error, IoContext, Result, invalid};

/// The computer and user being restored onto.
pub struct Target {
    pub device: Device,
    pub home: PathBuf,
    pub known: HashMap<KnownFolder, PathBuf>,
    /// Where restore reports are saved (outside the snapshot, which may be read-only).
    pub reports: Option<PathBuf>,
    /// `<ID> <VERSION_ID>` of this Linux distribution; distribution packages need a match.
    pub distro: Option<String>,
    pub elevation: Elevation,
    /// Opens an encrypted snapshot (see [`Manifest::unlock`]).
    pub key: Option<Key>,
}

impl Target {
    pub fn this_user(reports: Option<PathBuf>, elevation: Elevation) -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| invalid("No home folder for this user."))?;
        let known = KnownFolder::ALL
            .iter()
            .filter_map(|k| Some((*k, k.path()?)))
            .collect();
        Ok(Self {
            device: Device::detect()?,
            home,
            known,
            reports,
            distro: crate::platform::distro(),
            elevation,
            key: None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Conflict {
    /// Leave the existing file alone and skip the snapshot's copy.
    Skip,
    /// Restore next to it as `name (restored).ext`.
    KeepBoth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Installer,
    Config,
    Data,
    Folder,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct PlanItem {
    /// Root ID, or `install:<app id>`.
    pub id: String,
    pub kind: ItemKind,
    pub app: Option<String>,
    pub label: String,
    pub status: Status,
    pub reason: String,
    /// Installing needs an administrator password.
    pub admin: bool,
    pub destination: Option<String>,
    pub files: u64,
    pub bytes: u64,
    /// Different files already at the destination.
    pub conflicts: u64,
    /// Files already present with identical content (left as they are).
    pub identical: u64,
    /// Files that stay in the snapshot because they belong to the old computer.
    pub kept: u64,
    pub conflict_examples: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RestorePlan {
    pub snapshot_id: String,
    pub snapshot_name: String,
    pub created_at: String,
    pub source: Device,
    pub this_device: Device,
    pub prepared_for_this_platform: bool,
    /// What the user originally chose when making the snapshot.
    pub selection: Selection,
    pub items: Vec<PlanItem>,
    pub required_bytes: u64,
    pub free_bytes: Option<u64>,
    /// Some restorable item asks for an administrator password when it runs.
    pub needs_admin: bool,
    pub warnings: Vec<String>,
}

/// Where a root goes on this computer, or why it cannot be restored automatically.
fn destination(
    root: &Root,
    m: &Manifest,
    t: &Target,
) -> std::result::Result<PathBuf, (Status, String)> {
    let same_os = m.source.platform.os == t.device.platform.os;
    if root.kind != RootKind::Folder && !same_os {
        return Err((
            Status::Unsupported,
            format!(
                "No converter for these settings from {} to {} yet. The files stay in the snapshot ({}).",
                m.source.platform.os, t.device.platform.os, root.dir
            ),
        ));
    }
    if let Some(k) = root.known {
        return t
            .known
            .get(&k)
            .cloned()
            .or_else(|| Some(t.home.join(k.name())))
            .ok_or((Status::Unsupported, String::new()));
    }
    if root.original.starts_with("~/") {
        return untilde(&root.original, &t.home).map_err(|e| (Status::Unsupported, e.to_string()));
    }
    if root.kind == RootKind::Folder {
        // Outside the home folder on the source: restore into a clearly named folder instead.
        let leaf = Path::new(&root.original)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or("files".into());
        return Ok(t
            .home
            .join(format!("Restored from {}", m.source.hostname))
            .join(leaf));
    }
    Err((
        Status::Manual,
        format!(
            "This was stored outside the home folder ({}); copy it by hand from {} if you need it.",
            root.original, root.dir
        ),
    ))
}

/// Where a root's files go: the same folder as before, or (on another operating system) the
/// app's folder there, through its settings adapter.
enum Placement {
    Dir(PathBuf),
    Adapted(&'static Adapter, PathBuf),
}

impl Placement {
    fn folder(&self) -> &Path {
        match self {
            Self::Dir(d) | Self::Adapted(_, d) => d,
        }
    }
}

fn placement(
    root: &Root,
    m: &Manifest,
    t: &Target,
) -> std::result::Result<Placement, (Status, String)> {
    let (from, to) = (m.source.platform.os, t.device.platform.os);
    if root.kind == RootKind::Folder || from == to {
        return destination(root, m, t).map(Placement::Dir);
    }
    let adapter = root
        .app
        .as_ref()
        .and_then(|id| m.apps.iter().find(|a| &a.id == id))
        .and_then(|a| a.canonical.as_deref())
        .and_then(adapters::for_app)
        .filter(|a| a.covers(from, &root.original));
    match adapter {
        Some(a) => untilde(a.folder(to), &t.home)
            .map(|d| Placement::Adapted(a, d))
            .map_err(|e| (Status::Unsupported, e.to_string())),
        None => destination(root, m, t).map(Placement::Dir),
    }
}

/// Where one file goes, with its rewritten content when the adapter converts it.
enum Put {
    To(PathBuf, Option<Vec<u8>>),
    Keep(&'static str),
}

fn put(
    bundle: &Path,
    place: &Placement,
    root: &Root,
    f: &FileEntry,
    m: &Manifest,
    t: &Target,
) -> Result<Put> {
    let windows = t.device.platform.os == OsFamily::Windows;
    let (a, folder) = match place {
        Placement::Dir(d) => return Ok(Put::To(file_dest(d, f, windows)?, None)),
        Placement::Adapted(a, d) => (a, d),
    };
    let full = if f.rel.is_empty() {
        root.original.clone()
    } else {
        format!("{}/{}", root.original, f.rel)
    };
    let (from, to) = (m.source.platform.os, t.device.platform.os);
    let Some(logical) = a.logical(from, &full) else {
        return Ok(Put::Keep("Outside the app's settings folder."));
    };
    let dest = join_rel(folder, &logical, windows)?;
    Ok(match a.rule(&logical) {
        Rule::Keep(why) => Put::Keep(why),
        Rule::Copy => Put::To(dest, None),
        Rule::Convert if f.size > 10 << 20 => Put::Keep("Too large to convert."),
        Rule::Convert => {
            let src = join_rel(bundle, &f.stored, true)?;
            let bytes = fs::read(&src).ctx(|| format!("Reading {}", f.stored))?;
            let bytes = match (&f.stored_sha256, &t.key) {
                (None, _) => bytes,
                (Some(_), Some(k)) => k.open(&bytes)?,
                (Some(_), None) => return Err(crate::Error::Locked),
            };
            let sha = format!("{:x}", <sha2::Sha256 as sha2::Digest>::digest(&bytes));
            if sha != f.sha256 {
                return Err(invalid(
                    "The copy in the snapshot is damaged (checksum mismatch); not restored.",
                ));
            }
            Put::To(dest, Some(adapters::convert(&logical, &bytes, from, to)))
        }
    })
}

/// `None` if nothing is at `p`; else whether it already has the content being restored.
fn present(p: &Path, f: &FileEntry, data: Option<&[u8]>) -> Option<bool> {
    fs::symlink_metadata(p).ok()?;
    Some(match data {
        Some(d) => fs::read(p).is_ok_and(|have| have == d),
        None => same_content(p, f),
    })
}

fn file_dest(dest_root: &Path, f: &FileEntry, windows: bool) -> Result<PathBuf> {
    if f.rel.is_empty() {
        Ok(dest_root.to_path_buf())
    } else {
        join_rel(dest_root, &f.rel, windows)
    }
}

fn same_content(p: &Path, f: &FileEntry) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.len() == f.size)
        && hash_file(p, &AtomicBool::new(false), |_| {}).is_ok_and(|(_, h)| h == f.sha256)
}

pub fn plan(bundle: &Path, t: &Target) -> Result<RestorePlan> {
    let m = Manifest::open(bundle, t.key.as_ref())?;
    Ok(plan_for(bundle, &m, t))
}

fn plan_for(bundle: &Path, m: &Manifest, t: &Target) -> RestorePlan {
    let here = t.device.platform;
    let prepared = m.targets.contains(&here);
    let mut warnings = Vec::new();
    if !prepared {
        warnings.push(format!(
            "This snapshot was prepared for {}, not {here}. Personal files can still be restored.",
            m.targets
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut items = Vec::new();
    let mut required = 0;
    for a in m.apps.iter().filter(|a| a.installer) {
        let mut item = PlanItem {
            id: format!("install:{}", a.id),
            kind: ItemKind::Installer,
            app: Some(a.name.clone()),
            label: format!("{} — {}", a.name, part_label(Part::Installer)),
            status: Status::Unsupported,
            reason: String::new(),
            admin: false,
            destination: None,
            files: 0,
            bytes: 0,
            conflicts: 0,
            identical: 0,
            kept: 0,
            conflict_examples: Vec::new(),
        };
        if let Some(inst) = installer_for(m, &a.id, t) {
            let files = inst.recipe.files();
            item.files = files.len() as u64;
            item.bytes = files.iter().map(|f| f.size).sum();
            // Running as administrator already (e.g. root): nobody will be asked for a password.
            item.admin = installers::needs_admin(&inst.recipe) && t.elevation != Elevation::None;
            match installers::check(&inst.recipe, t.distro.as_deref(), &t.home) {
                Ok(true) => {
                    item.status = Status::Ready;
                    item.admin = false;
                    item.reason =
                        format!("{} is already installed; it will only be checked.", a.name);
                }
                Ok(false) => {
                    item.status = Status::Ready;
                    required += item.bytes;
                    item.reason = format!(
                        "Installs from the snapshot without internet access{}.",
                        if item.admin {
                            "; you will be asked for an administrator password"
                        } else {
                            ""
                        }
                    );
                }
                Err(why) => {
                    item.status = Status::Manual;
                    item.admin = false;
                    item.reason = why;
                }
            }
            items.push(item);
            continue;
        }
        let cov = m
            .coverage
            .iter()
            .find(|c| c.app == a.id && c.part == Part::Installer && c.target == here)
            .or_else(|| {
                m.coverage
                    .iter()
                    .find(|c| c.app == a.id && c.part == Part::Installer && c.target.os == here.os)
            });
        // No installer for this platform in the snapshot: only instructions remain.
        (item.status, item.reason) = match cov {
            Some(c) if c.status == Status::Unsupported => (Status::Unsupported, c.reason.clone()),
            Some(c) => (Status::Manual, c.reason.clone()),
            None => (
                Status::Unsupported,
                format!("This snapshot has no installer for {} on {here}.", a.name),
            ),
        };
        items.push(item);
    }
    for root in &m.roots {
        let app = root
            .app
            .as_ref()
            .and_then(|id| m.apps.iter().find(|a| &a.id == id));
        let (kind, what) = match root.kind {
            RootKind::Config => (ItemKind::Config, "settings"),
            RootKind::Data => (ItemKind::Data, "data"),
            RootKind::Folder => (ItemKind::Folder, "folder"),
        };
        let label = match app {
            Some(a) => format!("{} — {what} ({})", a.name, root.original),
            None => root.original.clone(),
        };
        let files: Vec<&FileEntry> = m.files.iter().filter(|f| f.root == root.id).collect();
        let mut item = PlanItem {
            id: root.id.clone(),
            kind,
            app: app.map(|a| a.name.clone()),
            label,
            status: Status::Ready,
            reason: String::new(),
            admin: false,
            destination: None,
            files: files.len() as u64,
            bytes: files.iter().map(|f| f.size).sum(),
            conflicts: 0,
            identical: 0,
            kept: 0,
            conflict_examples: Vec::new(),
        };
        match placement(root, m, t) {
            Err((status, reason)) => {
                item.status = status;
                item.reason = reason;
            }
            Ok(place) => {
                let dest = tilde(place.folder(), &t.home);
                item.destination = Some(dest.clone());
                let mut kept = Vec::new();
                for f in &files {
                    let (p, data) = match put(bundle, &place, root, f, m, t) {
                        Ok(Put::To(p, data)) => (p, data),
                        Ok(Put::Keep(why)) => {
                            item.kept += 1;
                            if kept.len() < 3 {
                                kept.push(format!("{} ({why})", f.rel));
                            }
                            continue;
                        }
                        Err(_) => {
                            item.conflicts += 1;
                            continue;
                        }
                    };
                    match present(&p, f, data.as_deref()) {
                        None => required += f.size,
                        Some(true) => item.identical += 1,
                        Some(false) => {
                            item.conflicts += 1;
                            if item.conflict_examples.len() < 10 {
                                item.conflict_examples.push(tilde(&p, &t.home));
                            }
                        }
                    }
                }
                item.reason = match place {
                    Placement::Dir(_) => format!("Restored into {dest}"),
                    Placement::Adapted(a, _) => format!(
                        "Converted for {} by the {} adapter into {dest}.{}",
                        here.os,
                        a.name,
                        if kept.is_empty() {
                            String::new()
                        } else {
                            format!(
                                " {} file(s) tied to the old computer stay in the snapshot, e.g. {}.",
                                item.kept,
                                kept.join("; ")
                            )
                        }
                    ),
                };
            }
        }
        items.push(item);
    }
    RestorePlan {
        snapshot_id: m.id.clone(),
        snapshot_name: m.name.clone(),
        created_at: m.created_at.clone(),
        source: m.source.clone(),
        this_device: t.device.clone(),
        prepared_for_this_platform: prepared,
        selection: m.selection.clone(),
        required_bytes: required,
        free_bytes: free_space(&t.home),
        needs_admin: items.iter().any(|i| i.admin && i.status == Status::Ready),
        items,
        warnings,
    }
}

fn installer_for<'m>(m: &'m Manifest, app: &str, t: &Target) -> Option<&'m Installer> {
    m.installers
        .iter()
        .find(|i| i.app == app && i.platform == t.device.platform)
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RestoreChoice {
    /// Plan item IDs to restore. Everything else is left untouched.
    pub items: Vec<String>,
    pub conflict: Conflict,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Installed (or already present) and confirmed by the package manager or checksum.
    Installed,
    Restored,
    Partial,
    Failed,
    Manual,
    Skipped,
    NotSelected,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ItemResult {
    pub id: String,
    pub label: String,
    pub outcome: Outcome,
    pub message: String,
    pub restored: u64,
    pub identical: u64,
    pub kept_both: u64,
    pub skipped_conflicts: u64,
    /// Files left in the snapshot because they belong to the old computer.
    pub kept: u64,
    pub failed: Vec<Failure>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RestoreReport {
    pub snapshot_id: String,
    pub snapshot_name: String,
    pub finished_at: String,
    pub items: Vec<ItemResult>,
    pub manual_steps: Vec<String>,
    pub report_file: Option<PathBuf>,
}

/// Creates the directories between `root` and `dir`, refusing to pass through symbolic links
/// (a planted link could otherwise redirect writes outside the destination).
pub(crate) fn ensure_dirs(root: &Path, dir: &Path) -> Result<()> {
    let base = if dir.starts_with(root) { root } else { dir };
    fs::create_dir_all(base).ctx(|| format!("Creating {}", base.display()))?;
    let mut cur = base.to_path_buf();
    for c in dir.strip_prefix(base).unwrap_or(Path::new("")).components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(md) if md.file_type().is_symlink() => {
                return Err(invalid(format!(
                    "{} is a symbolic link; not writing through it.",
                    cur.display()
                )));
            }
            Ok(md) if md.is_dir() => {}
            Ok(_) => {
                return Err(invalid(format!(
                    "A file is in the way at {}.",
                    cur.display()
                )));
            }
            Err(_) => fs::create_dir(&cur).ctx(|| format!("Creating {}", cur.display()))?,
        }
    }
    Ok(())
}

fn keep_both_name(p: &Path) -> PathBuf {
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    (1..)
        .map(|n| {
            let tag = if n == 1 {
                "restored".to_string()
            } else {
                format!("restored {n}")
            };
            p.with_file_name(format!("{stem} ({tag}){ext}"))
        })
        .find(|c| fs::symlink_metadata(c).is_err())
        .unwrap_or_else(|| p.to_path_buf())
}

/// Copies (and, for an encrypted snapshot, decrypts) one file out of the bundle into a new
/// file. Never overwrites.
pub(crate) fn restore_file(
    bundle: &Path,
    f: &FileEntry,
    dest: &Path,
    key: Option<&Key>,
    cancel: &AtomicBool,
) -> std::result::Result<(), String> {
    let src = join_rel(bundle, &f.stored, true).map_err(|e| e.to_string())?;
    let file = File::open(&src).map_err(|e| format!("Missing from the snapshot: {e}"))?;
    let mut input: Box<dyn std::io::Read> = match (&f.stored_sha256, key) {
        (None, _) => Box::new(file),
        (Some(_), Some(k)) => Box::new(
            k.decrypt_from(std::io::BufReader::new(file))
                .map_err(|e| e.to_string())?,
        ),
        (Some(_), None) => return Err(crate::Error::Locked.to_string()),
    };
    let mut out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| e.to_string())?;
    match copy_hashed(&mut input, &mut out, cancel, |_| {}) {
        Ok((_, sha)) if sha == f.sha256 => {
            let _ = out.sync_all();
        }
        Ok(_) => {
            drop(out);
            let _ = fs::remove_file(dest);
            return Err(
                "The copy in the snapshot is damaged (checksum mismatch); not restored.".into(),
            );
        }
        Err(e) => {
            drop(out);
            let _ = fs::remove_file(dest);
            return Err(match e {
                CopyError::Cancelled => "Cancelled".into(),
                CopyError::Read(e) => format!("Reading the snapshot failed: {e}"),
                CopyError::Write(e) => format!("Writing failed: {e}"),
            });
        }
    }
    if let Some(t) = f.mtime.filter(|t| *t >= 0) {
        let _ = out.set_modified(UNIX_EPOCH + Duration::from_secs(t as u64));
    }
    #[cfg(unix)]
    if let Some(mode) = f.mode {
        let _ = fs::set_permissions(
            dest,
            std::os::unix::fs::PermissionsExt::from_mode(mode & 0o777),
        );
    }
    Ok(())
}

/// Writes an adapter's rewritten file into a new file. Never overwrites.
fn write_converted(dest: &Path, data: &[u8]) -> std::result::Result<(), String> {
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .and_then(|mut f| f.write_all(data))
        .map_err(|e| e.to_string())
}

/// Relative link targets that stay inside the restored root; anything else is skipped.
fn safe_link_target(rel: &str, target: &str) -> bool {
    if target.is_empty() || target.starts_with('/') || target.contains('\\') || target.contains(':')
    {
        return false;
    }
    let mut depth = rel.split('/').count() as i64 - 1;
    for c in target.split('/') {
        match c {
            ".." => depth -= 1,
            "." | "" => {}
            _ => depth += 1,
        }
        if depth < 0 {
            return false;
        }
    }
    true
}

pub fn execute(
    bundle: &Path,
    t: &Target,
    choice: &RestoreChoice,
    emit: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<RestoreReport> {
    let m = Manifest::open(bundle, t.key.as_ref())?;
    let plan = plan_for(bundle, &m, t);
    let windows = t.device.platform.os == OsFamily::Windows;
    let total_bytes: u64 = plan
        .items
        .iter()
        .filter(|i| choice.items.contains(&i.id))
        .map(|i| i.bytes)
        .sum();
    let total_files: u64 = plan
        .items
        .iter()
        .filter(|i| choice.items.contains(&i.id))
        .map(|i| i.files)
        .sum();
    let (mut done_bytes, mut done_files) = (0, 0);
    let mut results = Vec::new();
    let mut manual = Vec::new();
    emit(Progress::Stage {
        label: "Restoring".into(),
        step: 1,
        steps: 1,
    });

    for item in &plan.items {
        let mut r = ItemResult {
            id: item.id.clone(),
            label: item.label.clone(),
            outcome: Outcome::NotSelected,
            message: String::new(),
            restored: 0,
            identical: 0,
            kept_both: 0,
            skipped_conflicts: 0,
            kept: 0,
            failed: Vec::new(),
        };
        if !choice.items.contains(&item.id) {
            results.push(r);
            continue;
        }
        match item.status {
            Status::Unsupported => {
                r.outcome = Outcome::Skipped;
                r.message = item.reason.clone();
            }
            Status::Manual => {
                r.outcome = Outcome::Manual;
                r.message = item.reason.clone();
                manual.push(format!("{}: {}", item.label, item.reason));
            }
            Status::Ready if item.kind == ItemKind::Installer => {
                let inst = m
                    .apps
                    .iter()
                    .find(|a| item.id == format!("install:{}", a.id))
                    .and_then(|a| installer_for(&m, &a.id, t))
                    .ok_or_else(|| invalid("Plan and manifest disagree."))?;
                let mut status = |s: String| {
                    emit(Progress::Note {
                        message: format!("{}: {s}", item.app.as_deref().unwrap_or_default()),
                    })
                };
                let result =
                    installers::install(bundle, &inst.recipe, &t.home, t.elevation, &mut status);
                done_bytes += item.bytes;
                done_files += item.files;
                let ok = installers::installed(&inst.recipe, &t.home);
                (r.outcome, r.message) = match (result, ok) {
                    (Ok(msg), true) => (Outcome::Installed, format!("{msg} Confirmed installed.")),
                    (Ok(_), false) => (
                        Outcome::Failed,
                        "The installer finished, but the app is not installed afterwards.".into(),
                    ),
                    (Err(e), _) => (Outcome::Failed, e),
                };
                if r.outcome == Outcome::Failed {
                    r.failed.push(Failure {
                        path: item.label.clone(),
                        error: r.message.clone(),
                    });
                }
            }
            Status::Ready => {
                let root = m
                    .roots
                    .iter()
                    .find(|x| x.id == item.id)
                    .ok_or_else(|| invalid("Plan and manifest disagree."))?;
                let place = placement(root, &m, t).map_err(|(_, why)| invalid(why))?;
                let dest_root = place.folder().to_path_buf();
                for f in m.files.iter().filter(|f| f.root == root.id) {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(Error::Cancelled);
                    }
                    emit(Progress::Copy {
                        done_bytes,
                        total_bytes,
                        done_files,
                        total_files,
                        current: f.rel.clone(),
                    });
                    done_bytes += f.size;
                    done_files += 1;
                    let fail = |r: &mut ItemResult, error: String| {
                        r.failed.push(Failure {
                            path: f.rel.clone(),
                            error,
                        })
                    };
                    let (mut dest, data) = match put(bundle, &place, root, f, &m, t) {
                        Ok(Put::To(d, data)) => (d, data),
                        Ok(Put::Keep(_)) => {
                            r.kept += 1;
                            continue;
                        }
                        Err(e) => {
                            fail(&mut r, e.to_string());
                            continue;
                        }
                    };
                    if let Err(e) = ensure_dirs(&dest_root, dest.parent().unwrap_or(&dest_root)) {
                        fail(&mut r, e.to_string());
                        continue;
                    }
                    if let Some(same) = present(&dest, f, data.as_deref()) {
                        if same {
                            r.identical += 1;
                            continue;
                        }
                        match choice.conflict {
                            Conflict::Skip => {
                                r.skipped_conflicts += 1;
                                continue;
                            }
                            Conflict::KeepBoth => {
                                dest = keep_both_name(&dest);
                                r.kept_both += 1;
                            }
                        }
                    }
                    let written = match &data {
                        Some(d) => write_converted(&dest, d),
                        None => restore_file(bundle, f, &dest, t.key.as_ref(), cancel),
                    };
                    match written {
                        Ok(()) => r.restored += 1,
                        Err(e) if e == "Cancelled" => return Err(Error::Cancelled),
                        Err(e) => fail(&mut r, e),
                    }
                }
                if matches!(place, Placement::Adapted(..)) {
                    r.kept += m.links.iter().filter(|l| l.root == root.id).count() as u64;
                }
                for l in m
                    .links
                    .iter()
                    .filter(|l| l.root == root.id && matches!(place, Placement::Dir(_)))
                {
                    let Ok(dest) = join_rel(&dest_root, &l.rel, windows) else {
                        continue;
                    };
                    if !cfg!(unix) || !safe_link_target(&l.rel, &l.target) {
                        r.failed.push(Failure {
                            path: l.rel.clone(),
                            error: format!(
                                "Link to {} not recreated (unsafe or unsupported here).",
                                l.target
                            ),
                        });
                        continue;
                    }
                    if fs::symlink_metadata(&dest).is_ok()
                        || ensure_dirs(&dest_root, dest.parent().unwrap_or(&dest_root)).is_err()
                    {
                        continue;
                    }
                    #[cfg(unix)]
                    if let Err(e) = std::os::unix::fs::symlink(&l.target, &dest) {
                        r.failed.push(Failure {
                            path: l.rel.clone(),
                            error: e.to_string(),
                        });
                    }
                }
                r.outcome = if r.failed.is_empty() {
                    Outcome::Restored
                } else if r.restored > 0 {
                    Outcome::Partial
                } else {
                    Outcome::Failed
                };
                r.message = format!(
                    "{} restored, {} already present, {} kept alongside existing files, {} skipped because a different file exists, {} failed.{}",
                    r.restored,
                    r.identical,
                    r.kept_both,
                    r.skipped_conflicts,
                    r.failed.len(),
                    if r.kept > 0 {
                        format!(
                            " {} left in the snapshot (tied to the old computer).",
                            r.kept
                        )
                    } else {
                        String::new()
                    }
                );
            }
        }
        results.push(r);
    }
    emit(Progress::Copy {
        done_bytes: total_bytes,
        total_bytes,
        done_files: total_files,
        total_files,
        current: String::new(),
    });

    let mut report = RestoreReport {
        snapshot_id: m.id.clone(),
        snapshot_name: m.name.clone(),
        finished_at: crate::now(),
        items: results,
        manual_steps: manual,
        report_file: None,
    };
    if let Some(dir) = &t.reports {
        let stem = format!("restore-{}-{}", m.id, report.finished_at.replace(':', "-"));
        // Two restores finishing in the same second keep both reports.
        if let Some(file) = (1u32..)
            .map(|n| match n {
                1 => dir.join(format!("{stem}.json")),
                _ => dir.join(format!("{stem}-{n}.json")),
            })
            .find(|f| !f.exists())
            && fs::create_dir_all(dir).is_ok()
            && let Ok(json) = serde_json::to_vec_pretty(&report)
            && fs::write(&file, json).is_ok()
        {
            report.report_file = Some(file);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_targets_stay_inside() {
        assert!(safe_link_target("a/link", "b"));
        assert!(safe_link_target("a/link", "../b"));
        assert!(!safe_link_target("link", "../b"));
        assert!(!safe_link_target("a/link", "/etc/passwd"));
        assert!(!safe_link_target("a/b/link", "../../../x"));
    }

    #[test]
    fn keep_both_names() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("a.txt");
        fs::write(&p, "x").unwrap();
        assert_eq!(keep_both_name(&p), t.path().join("a (restored).txt"));
        fs::write(t.path().join("a (restored).txt"), "x").unwrap();
        assert_eq!(keep_both_name(&p), t.path().join("a (restored 2).txt"));
    }

    #[test]
    fn refuses_to_write_through_symlinks() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("root");
        let outside = t.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, root.join("evil")).unwrap();
            assert!(ensure_dirs(&root, &root.join("evil/sub")).is_err());
        }
        assert!(ensure_dirs(&root, &root.join("ok/sub")).is_ok());
    }
}
