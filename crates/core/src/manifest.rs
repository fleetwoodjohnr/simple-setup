//! The snapshot manifest (`simple-setup.json`): versioned, self-describing, and treated as
//! untrusted input when read back. It never contains commands; restore actions are derived
//! from typed fields by code.

use std::collections::HashSet;
use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::discovery::{Confidence, GitHub, Source};
use crate::paths::check_rel;
use crate::platform::{Device, Platform};
use crate::selection::{KnownFolder, Selection};
use crate::{IoContext, Result, invalid};

pub const SCHEMA_VERSION: u32 = 1;
pub const MANIFEST_FILE: &str = "simple-setup.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub id: String,
    pub name: String,
    pub created_at: String,
    /// Program and version that wrote the snapshot.
    pub generator: String,
    pub source: Device,
    pub targets: Vec<Platform>,
    /// Exactly what the user chose, so restore can show it and it can be reused.
    pub selection: Selection,
    pub apps: Vec<ManifestApp>,
    /// Every copied tree or file: app settings, app data, personal folders.
    pub roots: Vec<Root>,
    pub files: Vec<FileEntry>,
    /// Offline installers collected for selected apps (one per app and platform).
    #[serde(default)]
    pub installers: Vec<Installer>,
    pub links: Vec<LinkEntry>,
    pub excluded: ExcludedSummary,
    pub coverage: Vec<Coverage>,
    pub readiness: Vec<Readiness>,
    pub warnings: Vec<String>,
    pub total_bytes: u64,
    /// Set when the snapshot is encrypted. This file then lists no apps, folders or files; the
    /// full manifest is in `encryption.manifest`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption: Option<Encryption>,
    /// The earlier snapshot this one updates; unchanged files were taken from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<BaseRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Encryption {
    /// `age` file, encrypted with the password, holding the key that opens everything else.
    pub key_file: String,
    /// `age` file holding the full manifest.
    pub manifest: String,
    /// Public half of the key (an `age1…` recipient); used to encrypt incremental updates.
    pub recipient: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BaseRef {
    pub id: String,
    pub name: String,
    /// Files and installers taken over from it instead of copied again.
    pub reused_files: u64,
    pub reused_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ManifestApp {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub source: Source,
    pub native_id: String,
    pub canonical: Option<String>,
    pub repository: Option<String>,
    pub github: Option<GitHub>,
    pub confidence: Confidence,
    pub evidence: Vec<String>,
    pub installer: bool,
    pub config: bool,
    pub data: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RootKind {
    Config,
    Data,
    Folder,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Root {
    /// Short unique ID, e.g. `r3`.
    pub id: String,
    pub kind: RootKind,
    /// Owning app ID for settings/data roots.
    pub app: Option<String>,
    pub known: Option<KnownFolder>,
    /// Source location: `~/…` when under the home folder, else absolute.
    pub original: String,
    /// Bundle-relative directory holding the copied content.
    pub dir: String,
    /// The root is a single file rather than a folder.
    pub is_file: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileEntry {
    pub root: String,
    /// Path within the root as it was on the source ('/'-separated; empty for a single-file root).
    pub rel: String,
    /// Bundle-relative path of the stored copy (portable name).
    pub stored: String,
    pub size: u64,
    pub sha256: String,
    /// Modification time, seconds since the Unix epoch.
    pub mtime: Option<i64>,
    /// Unix permission bits (0o777 mask).
    pub mode: Option<u32>,
    /// SHA-256 of the stored copy when it is encrypted (`sha256` is of the original content).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stored_sha256: Option<String>,
}

impl FileEntry {
    /// Checksum of the file as it is in the snapshot (what `SHA256SUMS` lists).
    pub fn stored_hash(&self) -> &str {
        self.stored_sha256.as_deref().unwrap_or(&self.sha256)
    }
}

/// A bundled installer file. Its checksum is checked again right before it is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoredFile {
    pub stored: String,
    pub size: u64,
    pub sha256: String,
}

/// A stored file identified by a Flatpak ref, package name or snap name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Package {
    pub name: String,
    pub file: StoredFile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapFile {
    pub name: String,
    pub snap: StoredFile,
    pub assertion: StoredFile,
}

/// How an app is reinstalled. Each variant maps to fixed commands in code; the manifest never
/// supplies a command line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recipe {
    /// `flatpak install --user --bundle` of each bundle in order (runtimes first, app last).
    Flatpak {
        app_ref: String,
        bundles: Vec<Package>,
    },
    /// `dnf install` of the bundled packages this computer is missing.
    Rpm {
        /// `<os-release ID> <VERSION_ID>` the packages were collected for, e.g. `fedora 44`.
        distro: String,
        packages: Vec<String>,
        rpms: Vec<Package>,
    },
    /// `snap ack` + `snap install` of each snap in order (bases first, app last).
    Snap { classic: bool, snaps: Vec<SnapFile> },
    /// A self-contained program file copied back to where it was, under the home folder.
    Program { original: String, file: StoredFile },
    /// A Windows installer run with fixed unattended options for its type, then confirmed in
    /// the installed-programs list.
    Windows {
        installer: WinInstaller,
        file: StoredFile,
        /// Names to look for in the installed-programs list afterwards.
        product: Vec<String>,
    },
    /// A macOS app copied out of a disk image or zip archive into Applications, then confirmed
    /// by its bundle identifier. Installer packages are kept for the user to open.
    MacApp {
        container: MacContainer,
        file: StoredFile,
        /// The `.app` inside, when the catalog names it; otherwise the user installs it.
        app: Option<String>,
        bundle_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WinInstaller {
    /// `msiexec /i <file> /passive /norestart`
    Msi,
    /// NSIS: `<file> /S`
    Nullsoft,
    /// Inno Setup: `<file> /VERYSILENT /SUPPRESSMSGBOXES /NORESTART /SP-`
    Inno,
    /// WiX bundle: `<file> /quiet /norestart`
    Burn,
    /// No known unattended mode: kept for the user to run.
    Exe,
    /// App package: kept for the user to open (App Installer).
    Msix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MacContainer {
    Dmg,
    Zip,
    Pkg,
}

impl Recipe {
    pub fn files(&self) -> Vec<&StoredFile> {
        match self {
            Self::Flatpak { bundles: p, .. } | Self::Rpm { rpms: p, .. } => {
                p.iter().map(|p| &p.file).collect()
            }
            Self::Snap { snaps, .. } => {
                snaps.iter().flat_map(|s| [&s.snap, &s.assertion]).collect()
            }
            Self::Program { file, .. } | Self::Windows { file, .. } | Self::MacApp { file, .. } => {
                vec![file]
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Installer {
    pub app: String,
    pub platform: Platform,
    pub recipe: Recipe,
    /// Where the files came from and what was checked when they were collected.
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkEntry {
    pub root: String,
    pub rel: String,
    pub target: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExcludedSummary {
    pub secrets: u64,
    pub transient: u64,
    pub by_user: u64,
    pub unreadable: u64,
    pub special: u64,
    /// First few excluded secret paths, so the user can see what was protected.
    pub secret_examples: Vec<String>,
    pub unreadable_examples: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    Installer,
    Config,
    Data,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Included and restorable offline on this target.
    Ready,
    /// Included or known, but the user must do something on the target.
    Manual,
    /// Cannot be restored on this target; will be skipped.
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub app: String,
    pub target: Platform,
    pub part: Part,
    pub status: Status,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Readiness {
    pub target: Platform,
    /// True only if every selected item is restorable offline and a restore engine is bundled.
    pub offline_ready: bool,
    pub engine_included: bool,
    pub reasons: Vec<String>,
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Package, snap and Flatpak names end up as command arguments: no option-like or odd values.
pub(crate) fn plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && !s.starts_with(['-', '.'])
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-".contains(c))
}

fn flatpak_ref(s: &str) -> bool {
    let p: Vec<&str> = s.split('/').collect();
    p.len() == 4
        && matches!(p[0], "app" | "runtime")
        && plain_name(p[1])
        && matches!(p[2], "x86_64" | "aarch64")
        && plain_name(p[3])
}

pub(crate) fn check_recipe(r: &Recipe) -> Result<()> {
    for f in r.files() {
        check_rel(&f.stored, true)?;
        if !f.stored.starts_with("installers/") || !is_sha256(&f.sha256) {
            return Err(invalid(format!("Bad installer file entry {:?}", f.stored)));
        }
    }
    let ok = match r {
        Recipe::Flatpak { app_ref, bundles } => {
            app_ref.starts_with("app/")
                && flatpak_ref(app_ref)
                && bundles.iter().all(|b| flatpak_ref(&b.name))
                && bundles.last().is_some_and(|b| &b.name == app_ref)
        }
        Recipe::Rpm { packages, rpms, .. } => {
            !packages.is_empty()
                && packages.iter().all(|p| plain_name(p))
                && rpms.iter().all(|p| plain_name(&p.name))
        }
        Recipe::Snap { snaps, .. } => {
            !snaps.is_empty() && snaps.iter().all(|s| plain_name(&s.name))
        }
        Recipe::Program { original, .. } => {
            original.starts_with("~/")
                && original.len() > 2
                && !original.split(['/', '\\']).any(|c| c == ".." || c == ".")
        }
        Recipe::Windows { product, .. } => {
            !product.is_empty()
                && product.iter().all(|p| {
                    !p.trim().is_empty() && p.len() <= 200 && !p.chars().any(char::is_control)
                })
        }
        Recipe::MacApp { app, bundle_id, .. } => {
            app.as_deref().is_none_or(|a| {
                a.len() > 4
                    && a.ends_with(".app")
                    && !a.starts_with('.')
                    && !a.contains(['/', '\\', ':'])
                    && !a.chars().any(char::is_control)
            }) && bundle_id.as_deref().is_none_or(plain_name)
        }
    };
    if ok {
        Ok(())
    } else {
        Err(invalid("Unsafe installer recipe in snapshot."))
    }
}

pub fn schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Manifest)).unwrap_or_default()
}

impl Manifest {
    /// Structural checks on an untrusted manifest. Does not touch the file system.
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(invalid(format!(
                "This snapshot uses format version {}; this program reads version {SCHEMA_VERSION}.",
                self.schema_version
            )));
        }
        let mut roots = HashSet::new();
        for r in &self.roots {
            if !roots.insert(r.id.as_str()) {
                return Err(invalid(format!("Duplicate root {} in snapshot.", r.id)));
            }
            check_rel(&r.dir, true)?;
            if !(r.original.starts_with("~/")
                || Path::new(&r.original).is_absolute()
                || r.original.starts_with('/'))
                || r.original.split(['/', '\\']).any(|c| c == "..")
            {
                return Err(invalid(format!(
                    "Unsafe original location in snapshot: {:?}",
                    r.original
                )));
            }
        }
        let mut stored = HashSet::new();
        for f in &self.files {
            let root = self
                .roots
                .iter()
                .find(|r| r.id == f.root)
                .ok_or_else(|| invalid("File without a root."))?;
            check_rel(&f.stored, true)?;
            if !f.stored.starts_with(&format!("{}/", root.dir))
                || !stored.insert(f.stored.to_lowercase())
            {
                return Err(invalid(format!(
                    "Unsafe or duplicate stored path: {:?}",
                    f.stored
                )));
            }
            if root.is_file != f.rel.is_empty() {
                return Err(invalid(format!("Inconsistent entry {:?}", f.stored)));
            }
            if !f.rel.is_empty() {
                check_rel(&f.rel, false)?;
            }
            if !is_sha256(&f.sha256) || f.stored_sha256.as_deref().is_some_and(|h| !is_sha256(h)) {
                return Err(invalid(format!("Bad checksum for {:?}", f.stored)));
            }
            if f.stored_sha256.is_some() != self.encryption.is_some() {
                return Err(invalid(format!(
                    "Inconsistent encryption for {:?}",
                    f.stored
                )));
            }
        }
        for i in &self.installers {
            check_recipe(&i.recipe)?;
        }
        for l in &self.links {
            check_rel(&l.rel, false)?;
            if !roots.contains(l.root.as_str()) {
                return Err(invalid("Link without a root."));
            }
        }
        if let Some(e) = &self.encryption {
            check_rel(&e.key_file, true)?;
            check_rel(&e.manifest, true)?;
        }
        Ok(())
    }

    /// The full manifest: decrypted with `key` for an encrypted snapshot (else [`Error::Locked`]).
    ///
    /// [`Error::Locked`]: crate::Error::Locked
    pub fn open(bundle: &Path, key: Option<&crate::crypt::Key>) -> Result<Self> {
        let outer = Self::read(bundle)?;
        let Some(e) = &outer.encryption else {
            return Ok(outer);
        };
        let key = key.ok_or(crate::Error::Locked)?;
        let p = crate::paths::join_rel(bundle, &e.manifest, true)?;
        let sealed = std::fs::read(&p).ctx(|| format!("Reading {}", p.display()))?;
        let inner: Self = serde_json::from_slice(&key.open(&sealed)?)
            .map_err(|e| invalid(format!("Damaged encrypted manifest: {e}")))?;
        inner.validate()?;
        if inner.id != outer.id || inner.encryption != outer.encryption {
            return Err(invalid(
                "The encrypted manifest belongs to a different snapshot.",
            ));
        }
        Ok(inner)
    }

    /// The key for this snapshot from a password or recovery key; `None` if it is not encrypted.
    pub fn unlock(&self, bundle: &Path, secret: Option<&str>) -> Result<Option<crate::crypt::Key>> {
        match (&self.encryption, secret) {
            (None, _) => Ok(None),
            (Some(_), None) => Err(crate::Error::Locked),
            (Some(e), Some(s)) => {
                crate::crypt::Key::unlock(bundle, s, Some(&e.recipient)).map(Some)
            }
        }
    }

    pub fn read(bundle: &Path) -> Result<Self> {
        let p = bundle.join(MANIFEST_FILE);
        let bytes = std::fs::read(&p).ctx(|| format!("Reading {}", p.display()))?;
        let m: Self = serde_json::from_slice(&bytes)
            .map_err(|e| invalid(format!("Not a valid Simple Setup snapshot: {e}")))?;
        m.validate()?;
        Ok(m)
    }
}
