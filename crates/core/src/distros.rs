//! Linux distribution profiles: how a distribution is recognised (from `/etc/os-release`), what
//! it installs with, which repositories it trusts and which portable formats it supports.
//!
//! Profiles are data. Built-in ones are embedded (`registry/distros.json`); official updates
//! (downloaded from this project's GitHub releases) and the user's own profiles are layered on
//! top, and a profile with the same ID in a later layer replaces an earlier one. Every profile
//! may name a `parent` and then states only what differs: a new Ubuntu-based distribution needs
//! an ID, a name and `"parent": "ubuntu"`.
//!
//! Imported profiles are untrusted. They are size-limited and schema-checked, and none may carry
//! commands, except that a user profile may describe a `custom` package manager as fixed
//! argument lists. Those run only after the user trusted that exact command set (its SHA-256 is
//! recorded; any edit clears the trust), and never through a shell.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use globset::Glob;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::platform::OsRelease;
use crate::{IoContext, Result, invalid};

pub const SCHEMA_VERSION: u32 = 1;
/// The profile every chain ends in.
pub const ROOT: &str = "linux";
/// Largest profile file accepted.
pub const MAX_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 8;
const MAX_PROFILES: usize = 500;
const ARCHES: &[&str] = &[
    "x86_64", "aarch64", "armv7", "i686", "riscv64", "ppc64le", "s390x",
];
/// Programs a custom adapter may not start: shells and interpreters would turn its fixed
/// arguments back into a script, and elevation is added by Simple Setup itself.
const REFUSED_PROGRAMS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "fish", "ksh", "csh", "tcsh", "env", "sudo", "pkexec", "doas",
    "su", "run0", "python", "python3", "perl", "ruby", "node", "lua", "busybox", "eval", "xargs",
];

/// The package manager a profile installs with. Every adapter except `custom` is a fixed set of
/// command lines in [`crate::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Adapter {
    /// Nothing is installed with a system package manager (only portable formats).
    None,
    Apt,
    Dnf,
    Zypper,
    Pacman,
    Apk,
    /// Layers packages onto Fedora's read-only image (takes effect after a restart).
    RpmOstree,
    /// openSUSE's immutable editions (takes effect after a restart).
    TransactionalUpdate,
    /// The profile's own `custom` commands (needs the user's trust).
    Custom,
}

impl Adapter {
    /// The command whose presence shows the adapter can work.
    pub fn tool(self) -> Option<&'static str> {
        Some(match self {
            Self::Apt => "apt-get",
            Self::Dnf => "dnf",
            Self::Zypper => "zypper",
            Self::Pacman => "pacman",
            Self::Apk => "apk",
            Self::RpmOstree => "rpm-ostree",
            Self::TransactionalUpdate => "transactional-update",
            Self::None | Self::Custom => return None,
        })
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Apt => "APT",
            Self::Dnf => "DNF",
            Self::Zypper => "Zypper",
            Self::Pacman => "pacman",
            Self::Apk => "apk",
            Self::RpmOstree => "rpm-ostree",
            Self::TransactionalUpdate => "transactional-update",
            Self::Custom => "custom commands",
        }
    }

    /// Installs take effect only after a restart (image-based systems).
    pub fn needs_restart(self) -> bool {
        matches!(self, Self::RpmOstree | Self::TransactionalUpdate)
    }
}

/// Native package file formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum Format {
    #[serde(rename = "deb")]
    Deb,
    #[serde(rename = "rpm")]
    Rpm,
    #[serde(rename = "pkg.tar.zst")]
    Pacman,
    #[serde(rename = "apk")]
    Apk,
}

impl Format {
    pub fn label(self) -> &'static str {
        match self {
            Self::Deb => "DEB",
            Self::Rpm => "RPM",
            Self::Pacman => "pkg.tar.zst",
            Self::Apk => "APK",
        }
    }
}

/// Whether a portable format works on a distribution out of the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Avail {
    /// Installed and set up by default.
    Default,
    /// Can be installed from the distribution's own repositories.
    Available,
    /// Not usable there (switched off, or the system cannot run it).
    Unsupported,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Universal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flatpak: Option<Avail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snap: Option<Avail>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appimage: Option<Avail>,
}

/// How `/etc/os-release` identifies the distribution.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Match {
    /// `ID` values.
    #[serde(default)]
    pub ids: Vec<String>,
    /// `VARIANT_ID` values; when given, one must match (e.g. `silverblue`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variants: Vec<String>,
    /// Lowest and highest `VERSION_ID` (dotted numbers, inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_version: Option<String>,
}

/// A repository whose packages count as the distribution's own. `id` is a glob matched against
/// what the package manager reports: the DNF repo ID, the APT release `Origin`, the Zypper
/// repository alias or name, the pacman repository, or the apk repository URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Packages from it are installed without asking (default true).
    #[serde(default = "yes")]
    pub official: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    /// Try this kind of install first.
    Prefer,
    /// Never use this kind of install.
    Avoid,
    /// Do not install the app on this distribution at all.
    Skip,
}

/// Kinds of installation a rule can name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MethodKind {
    /// The distribution's package manager.
    Native,
    Flatpak,
    Snap,
    /// A verified download (GitHub release, vendor installer).
    Download,
    /// Built from source code.
    Source,
}

/// A distribution-specific decision about one app (`app`: registry ID, or `*` for all).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub app: String,
    pub action: RuleAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<MethodKind>,
    pub reason: String,
}

/// A package manager Simple Setup has no built-in adapter for, as fixed argument lists.
/// `{packages}` (install) stands for all package names, `{package}` (query, available) for
/// one; each must be a whole argument. Nothing goes through a shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CustomAdapter {
    /// Shown in plans, e.g. "xbps".
    pub label: String,
    pub install: Vec<String>,
    /// Exits 0 when the package is installed.
    pub query: Vec<String>,
    /// Exits 0 when the package can be installed (optional).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub available: Vec<String>,
    /// Install runs with administrator rights.
    #[serde(default = "yes")]
    pub admin: bool,
}

/// One profile as written. Everything but `id` is optional and inherited from the parent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, rename = "match", skip_serializing_if = "Option::is_none")]
    pub matching: Option<Match>,
    /// Broad ecosystem shown to people (`debian`, `fedora`, `suse`…).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Processor architectures the distribution is built for (`uname -m` style names).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_manager: Option<Adapter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formats: Option<Vec<Format>>,
    /// Keys of the application registry holding this distribution's package names (`dpkg`,
    /// `rpm`, `suse`…). The profile IDs of the chain are looked up first, so the registry can
    /// hold names for one distribution alone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<Vec<String>>,
    /// Repositories of a child are added to the parent's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repositories: Option<Vec<Repository>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universal: Option<Universal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub immutable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolling: Option<bool>,
    /// A child's rule for an app replaces the parent's rules for that app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<Rule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomAdapter>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// A profile file (`distros.json`, an import or an official update).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileFile {
    pub schema_version: u32,
    /// Release of the set (dotted numbers); an official update must be newer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub distros: Vec<Profile>,
}

/// Where a profile comes from.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Layer {
    Builtin,
    Official,
    Custom,
}

/// A profile with its whole chain applied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Resolved {
    pub id: String,
    pub name: String,
    /// This profile, its parent, … down to [`ROOT`].
    pub chain: Vec<String>,
    pub family: String,
    pub arch: Vec<String>,
    pub package_manager: Adapter,
    pub formats: Vec<Format>,
    /// Registry keys for package names, in lookup order (chain IDs, then `names`).
    pub names: Vec<String>,
    /// The declared `names` alone: the package-name spaces this distribution shares with
    /// others (an app's own package name is reused only where one is shared).
    pub namespaces: Vec<String>,
    pub repositories: Vec<Repository>,
    pub flatpak: Avail,
    pub snap: Avail,
    pub appimage: Avail,
    pub immutable: bool,
    pub rolling: bool,
    pub rules: Vec<Rule>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom: Option<CustomAdapter>,
    /// The user trusted the custom commands (always false without them).
    #[serde(default)]
    pub trusted: bool,
    pub layer: Layer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

impl Resolved {
    pub fn has_format(&self, f: Format) -> bool {
        self.formats.contains(&f)
    }

    /// The registry's package names for app `entry` on this distribution, best key first.
    pub fn package_names(&self, packages: &BTreeMap<String, Vec<String>>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for key in &self.names {
            for n in packages.get(key).into_iter().flatten() {
                if !out.contains(n) {
                    out.push(n.clone());
                }
            }
        }
        out
    }

    /// Whether the profile's package manager can be used (custom commands need trust).
    pub fn usable_manager(&self) -> Option<Adapter> {
        match self.package_manager {
            Adapter::None => None,
            Adapter::Custom if !self.trusted || self.custom.is_none() => None,
            a => Some(a),
        }
    }

    /// The rules for `app` (registry ID), its own before the `*` ones.
    pub fn rules_for<'a>(&'a self, app: Option<&str>) -> impl Iterator<Item = &'a Rule> {
        let own: Vec<&Rule> = self
            .rules
            .iter()
            .filter(|r| app.is_some_and(|a| r.app == a))
            .collect();
        let any = self.rules.iter().filter(|r| r.app == "*");
        own.into_iter().chain(any)
    }

    /// Whether a repository reported by the package manager is one of this distribution's own.
    pub fn official_repo(&self, repo: &str) -> Option<bool> {
        self.repositories
            .iter()
            .find(|r| glob_match(&r.id, repo))
            .map(|r| r.official)
    }
}

fn glob_match(pattern: &str, s: &str) -> bool {
    Glob::new(pattern).is_ok_and(|g| g.compile_matcher().is_match(s))
}

/// How a computer's distribution was recognised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", content = "via")]
pub enum Matched {
    /// Its `ID` (and `VARIANT_ID`, version) match a profile.
    Exact,
    /// Not known by its `ID`; treated like the named distribution from its `ID_LIKE`.
    Like(String),
    /// Nothing matched; only portable formats are assumed.
    Generic,
    /// Chosen by the user in Settings.
    Override,
}

impl Matched {
    pub fn explain(&self, profile: &Resolved, os: &OsRelease) -> String {
        let shown = os.display();
        match self {
            Self::Exact => format!("Detected {shown} ({} profile).", profile.name),
            Self::Like(_) => format!(
                "{shown} is not a known distribution; it says it is like {}, so its profile is used.",
                profile.name
            ),
            Self::Generic => format!(
                "{shown} is not a known distribution. Only portable formats (Flatpak, AppImage) are assumed; add a profile for it in Settings."
            ),
            Self::Override => format!(
                "Set by you in Settings: treated as {} (detected {shown}).",
                profile.name
            ),
        }
    }
}

/// What the user changed: their profiles, which are switched off, and trusted commands.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct UserProfiles {
    pub profiles: Vec<Profile>,
    pub disabled: Vec<String>,
    /// Profile ID → SHA-256 of the custom commands the user trusted.
    pub trusted: BTreeMap<String, String>,
}

/// A row of the profile list.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Listed {
    pub id: String,
    pub name: String,
    pub parent: Option<String>,
    pub family: String,
    pub layer: Layer,
    pub enabled: bool,
    /// A user or official profile replaces the built-in one of the same ID.
    pub replaces: Option<Layer>,
    pub package_manager: Adapter,
    pub immutable: bool,
    pub has_custom: bool,
    pub trusted: bool,
    /// Problems that keep the profile from being used.
    pub problems: Vec<String>,
}

/// All profiles known to this computer.
#[derive(Debug, Clone)]
pub struct Distros {
    builtin: Vec<Profile>,
    official: Option<ProfileFile>,
    user: UserProfiles,
    /// Problems reading the official or user file (the rest still works).
    pub load_problems: Vec<String>,
}

static BUILTIN: LazyLock<ProfileFile> = LazyLock::new(|| {
    // Embedded at build time and covered by tests.
    #[allow(clippy::unwrap_used)]
    serde_json::from_str(include_str!("../registry/distros.json")).unwrap()
});

/// The profile ID a stored choice means: `other` (used before profiles existed) is the generic
/// Linux profile.
pub fn canonical(id: &str) -> &str {
    if id == "other" { ROOT } else { id }
}

pub fn builtin_version() -> &'static str {
    BUILTIN.version.as_deref().unwrap_or("0")
}

const OFFICIAL_FILE: &str = "distros/official.json";
const USER_FILE: &str = "distros/user.json";

impl Distros {
    pub fn builtin() -> Self {
        Self {
            builtin: BUILTIN.distros.clone(),
            official: None,
            user: UserProfiles::default(),
            load_problems: Vec::new(),
        }
    }

    /// Built-in profiles plus the official update and the user's profiles saved under the
    /// Simple Setup configuration folder `dir`.
    pub fn load(dir: &Path) -> Self {
        let mut d = Self::builtin();
        match fs::read(dir.join(OFFICIAL_FILE)) {
            Ok(b) => match serde_json::from_slice::<ProfileFile>(&b) {
                Ok(f) if newer(f.version.as_deref(), Some(builtin_version())) => {
                    d.official = Some(f)
                }
                Ok(_) => {}
                Err(e) => d.load_problems.push(format!(
                    "The downloaded distribution profiles are damaged: {e}"
                )),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => d.load_problems.push(e.to_string()),
        }
        match fs::read(dir.join(USER_FILE)) {
            Ok(b) => match serde_json::from_slice::<UserProfiles>(&b) {
                Ok(u) => d.user = u,
                Err(e) => d
                    .load_problems
                    .push(format!("Your distribution profiles could not be read: {e}")),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => d.load_problems.push(e.to_string()),
        }
        d
    }

    /// The profiles of this user (`~/.config/simple-setup`).
    pub fn load_default() -> Self {
        match crate::store::Store::open_default() {
            Ok(s) => Self::load(s.root()),
            Err(_) => Self::builtin(),
        }
    }

    pub fn user(&self) -> &UserProfiles {
        &self.user
    }

    pub fn official_version(&self) -> Option<&str> {
        self.official.as_ref().and_then(|f| f.version.as_deref())
    }

    /// The version that an official update must exceed.
    pub fn current_version(&self) -> &str {
        self.official_version().unwrap_or(builtin_version())
    }

    fn layers(&self) -> impl Iterator<Item = (&Profile, Layer)> {
        let official = self.official.iter().flat_map(|f| &f.distros);
        self.builtin
            .iter()
            .map(|p| (p, Layer::Builtin))
            .chain(official.map(|p| (p, Layer::Official)))
            .chain(self.user.profiles.iter().map(|p| (p, Layer::Custom)))
    }

    /// The effective profile with this ID (later layers replace earlier ones).
    pub fn get(&self, id: &str) -> Option<(&Profile, Layer)> {
        self.layers().filter(|(p, _)| p.id == id).last()
    }

    fn enabled(&self, id: &str) -> bool {
        !self.user.disabled.iter().any(|d| d == id)
    }

    /// Every effective profile ID, built-in order first.
    pub fn ids(&self) -> Vec<String> {
        let mut seen = HashSet::new();
        self.layers()
            .filter(|(p, _)| seen.insert(p.id.clone()))
            .map(|(p, _)| p.id.clone())
            .collect()
    }

    /// Applies the chain of `id`.
    pub fn resolve(&self, id: &str) -> Result<Resolved> {
        let mut chain: Vec<(&Profile, Layer)> = Vec::new();
        let mut next = Some(id.to_string());
        while let Some(cur) = next {
            if chain.iter().any(|(p, _)| p.id == cur) {
                return Err(invalid(format!(
                    "The distribution profile {id:?} inherits from itself (through {cur:?})."
                )));
            }
            if chain.len() >= MAX_DEPTH {
                return Err(invalid(format!(
                    "The distribution profile {id:?} has more than {MAX_DEPTH} levels of parents."
                )));
            }
            let (p, layer) = self.get(&cur).ok_or_else(|| {
                invalid(format!(
                    "No distribution profile {cur:?}{}.",
                    if cur == id {
                        String::new()
                    } else {
                        format!(" (the parent of {id:?})")
                    }
                ))
            })?;
            chain.push((p, layer));
            next = p
                .parent
                .clone()
                .or_else(|| (cur != ROOT).then(|| ROOT.into()));
        }
        let pick = |f: &dyn Fn(&Profile) -> Option<String>| chain.iter().find_map(|(p, _)| f(p));
        let (leaf, layer) = chain[0];
        let universal = |f: &dyn Fn(&Universal) -> Option<Avail>| {
            chain
                .iter()
                .find_map(|(p, _)| p.universal.as_ref().and_then(f))
                .unwrap_or(Avail::Available)
        };
        let mut names: Vec<String> = chain.iter().map(|(p, _)| p.id.clone()).collect();
        let namespaces = chain
            .iter()
            .find_map(|(p, _)| p.names.clone())
            .unwrap_or_default();
        names.extend(namespaces.iter().cloned());
        names.retain(|n| n != ROOT);
        let mut seen = HashSet::new();
        names.retain(|n| seen.insert(n.clone()));
        let mut repositories = Vec::new();
        for (p, _) in &chain {
            repositories.extend(p.repositories.iter().flatten().cloned());
        }
        // A child's rule for an app replaces the parent's rules for that app.
        let mut rules: Vec<Rule> = Vec::new();
        for (p, _) in &chain {
            let mine: Vec<&Rule> = p.rules.iter().flatten().collect();
            let decided: HashSet<String> = rules.iter().map(|r| r.app.clone()).collect();
            rules.extend(
                mine.into_iter()
                    .filter(|r| !decided.contains(&r.app))
                    .cloned(),
            );
        }
        let package_manager = chain
            .iter()
            .find_map(|(p, _)| p.package_manager)
            .unwrap_or(Adapter::None);
        // Custom commands belong to the profile that states them; a child inherits them only
        // together with its parent's trust.
        let custom_owner = chain.iter().find(|(p, _)| p.custom.is_some());
        let custom = custom_owner.and_then(|(p, _)| p.custom.clone());
        let trusted = custom_owner.is_some_and(|(p, l)| {
            *l == Layer::Custom
                && p.custom.as_ref().is_some_and(|c| {
                    self.user.trusted.get(&p.id) == Some(&command_digest(&p.id, c))
                })
        });
        Ok(Resolved {
            id: leaf.id.clone(),
            name: pick(&|p| p.name.clone()).unwrap_or_else(|| leaf.id.clone()),
            chain: chain.iter().map(|(p, _)| p.id.clone()).collect(),
            family: pick(&|p| p.family.clone()).unwrap_or_else(|| ROOT.into()),
            arch: chain
                .iter()
                .find_map(|(p, _)| p.arch.clone())
                .unwrap_or_default(),
            package_manager,
            formats: chain
                .iter()
                .find_map(|(p, _)| p.formats.clone())
                .unwrap_or_default(),
            names,
            namespaces,
            repositories,
            flatpak: universal(&|u| u.flatpak),
            snap: universal(&|u| u.snap),
            appimage: universal(&|u| u.appimage),
            immutable: chain.iter().find_map(|(p, _)| p.immutable).unwrap_or(false),
            rolling: chain.iter().find_map(|(p, _)| p.rolling).unwrap_or(false),
            rules,
            custom,
            trusted,
            layer,
            notes: leaf.notes.clone(),
        })
    }

    /// The profile for a computer with this `/etc/os-release`: by `ID` (and `VARIANT_ID` and
    /// version), else by the first `ID_LIKE` entry that names a profile, else the generic one.
    pub fn detect(&self, os: &OsRelease) -> (Resolved, Matched) {
        if let Some(id) = self.best_match(&os.id, os, true)
            && let Ok(r) = self.resolve(&id)
        {
            return (r, Matched::Exact);
        }
        for like in &os.id_like {
            if let Some(id) = self.best_match(like, os, false)
                && let Ok(r) = self.resolve(&id)
            {
                return (r, Matched::Like(like.clone()));
            }
        }
        #[allow(clippy::unwrap_used)] // the root profile is built in and tested
        let mut generic = self
            .resolve(ROOT)
            .unwrap_or_else(|_| Self::builtin().resolve(ROOT).unwrap());
        generic.name = os.display();
        (generic, Matched::Generic)
    }

    /// The most specific enabled profile matching `id`: one whose variants include the
    /// computer's `VARIANT_ID` beats one without variants; a user profile beats a built-in one.
    fn best_match(&self, id: &str, os: &OsRelease, exact: bool) -> Option<String> {
        let mut best: Option<(u8, Layer, String)> = None;
        for pid in self.ids() {
            let Some((p, layer)) = self.get(&pid) else {
                continue;
            };
            let Some(m) = &p.matching else { continue };
            if !self.enabled(&pid) || !m.ids.iter().any(|x| x == id) {
                continue;
            }
            let score = if m.variants.is_empty() {
                1
            } else if exact
                && os
                    .variant_id
                    .as_deref()
                    .is_some_and(|v| m.variants.iter().any(|x| x == v))
            {
                2
            } else {
                continue;
            };
            if exact && !version_in(m, os.version_id.as_deref()) {
                continue;
            }
            if best
                .as_ref()
                .is_none_or(|(s, l, _)| (score, layer) > (*s, *l))
            {
                best = Some((score, layer, pid));
            }
        }
        best.map(|(_, _, id)| id)
    }

    /// Every effective profile, with whether it can be used.
    pub fn list(&self) -> Vec<Listed> {
        self.ids()
            .into_iter()
            .filter_map(|id| {
                let (p, layer) = self.get(&id)?;
                let replaces = self
                    .layers()
                    .filter(|(q, l)| q.id == id && *l < layer)
                    .map(|(_, l)| l)
                    .last();
                let (resolved, problems) = match self.resolve(&id) {
                    Ok(r) => (Some(r), Vec::new()),
                    Err(e) => (None, vec![e.to_string()]),
                };
                Some(Listed {
                    name: resolved
                        .as_ref()
                        .map_or_else(|| p.name.clone().unwrap_or(id.clone()), |r| r.name.clone()),
                    parent: p.parent.clone(),
                    family: resolved
                        .as_ref()
                        .map_or(String::new(), |r| r.family.clone()),
                    layer,
                    enabled: self.enabled(&id),
                    replaces,
                    package_manager: resolved
                        .as_ref()
                        .map_or(Adapter::None, |r| r.package_manager),
                    immutable: resolved.as_ref().is_some_and(|r| r.immutable),
                    has_custom: resolved.as_ref().is_some_and(|r| r.custom.is_some()),
                    trusted: resolved.as_ref().is_some_and(|r| r.trusted),
                    problems,
                    id,
                })
            })
            .collect()
    }

    /// These profiles combined with the given user profiles (for validating before saving).
    fn with_user(&self, profiles: Vec<Profile>) -> Self {
        let mut d = self.clone();
        for p in profiles {
            d.user.profiles.retain(|q| q.id != p.id);
            d.user.profiles.push(p);
        }
        d
    }
}

fn newer(a: Option<&str>, b: Option<&str>) -> bool {
    crate::update::version_cmp(a.unwrap_or("0"), b.unwrap_or("0")).is_gt()
}

fn version_in(m: &Match, v: Option<&str>) -> bool {
    use crate::update::version_cmp;
    let Some(v) = v else {
        // Rolling releases have no VERSION_ID: only a profile without bounds fits.
        return m.min_version.is_none() && m.max_version.is_none();
    };
    m.min_version
        .as_deref()
        .is_none_or(|lo| version_cmp(v, lo).is_ge())
        && m.max_version
            .as_deref()
            .is_none_or(|hi| version_cmp(v, hi).is_le())
}

/// SHA-256 over a profile's custom commands: what the user's trust is tied to.
pub fn command_digest(id: &str, c: &CustomAdapter) -> String {
    let mut h = Sha256::new();
    h.update(id.as_bytes());
    h.update([0]);
    h.update(serde_json::to_vec(c).unwrap_or_default());
    format!("{:x}", h.finalize())
}

// ---------------------------------------------------------------- reading and checking

/// Reads a profile file: JSON, or YAML when `yaml` (by file extension). Size-limited.
pub fn parse(text: &str, yaml: bool) -> Result<ProfileFile> {
    if text.len() > MAX_BYTES {
        return Err(invalid(format!(
            "The profile file is larger than {} KB.",
            MAX_BYTES / 1024
        )));
    }
    let f: ProfileFile = if yaml {
        serde_saphyr::from_str(text)
            .map_err(|e| invalid(format!("This is not a valid profile file: {e}")))?
    } else {
        serde_json::from_str(text)
            .map_err(|e| invalid(format!("This is not a valid profile file: {e}")))?
    };
    if f.schema_version != SCHEMA_VERSION {
        return Err(invalid(format!(
            "The profile file uses format version {}; this program reads version {SCHEMA_VERSION}.",
            f.schema_version
        )));
    }
    Ok(f)
}

/// Reads a profile file from disk; `.yaml`/`.yml` files are read as YAML.
pub fn read_file(path: &Path) -> Result<ProfileFile> {
    let meta = fs::metadata(path).ctx(|| format!("Reading {}", path.display()))?;
    if meta.len() > MAX_BYTES as u64 {
        return Err(invalid(format!(
            "{} is larger than {} KB.",
            path.display(),
            MAX_BYTES / 1024
        )));
    }
    let text = fs::read_to_string(path).ctx(|| format!("Reading {}", path.display()))?;
    let yaml = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"));
    parse(&text, yaml)
}

fn plain_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
}

fn plain_text(s: &str, max: usize) -> bool {
    s.chars().count() <= max && !s.chars().any(char::is_control)
}

fn version_syntax(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

fn check_argv(what: &str, argv: &[String], placeholder: &str, out: &mut Vec<String>) {
    let Some(prog) = argv.first() else {
        out.push(format!("The {what} command is empty."));
        return;
    };
    if argv.len() > 32 {
        out.push(format!("The {what} command has more than 32 arguments."));
    }
    let base = prog.rsplit('/').next().unwrap_or(prog);
    let plain = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
            && !s.starts_with(['-', '.'])
    };
    let absolute = prog.starts_with('/')
        && !prog.split('/').any(|c| c == ".." || c == ".")
        && prog[1..].split('/').all(plain);
    if !(plain(prog) || absolute) {
        out.push(format!(
            "The {what} command must start with a program name or an absolute path, not {prog:?}."
        ));
    }
    if REFUSED_PROGRAMS
        .iter()
        .any(|r| base == *r || base.starts_with("python"))
    {
        out.push(format!(
            "The {what} command may not start {base}: shells, interpreters and elevation tools are not allowed (Simple Setup asks for administrator rights itself)."
        ));
    }
    let mut found = false;
    for a in &argv[1..] {
        if a.len() > 200 || a.chars().any(char::is_control) {
            out.push(format!(
                "An argument of the {what} command is too long or has control characters."
            ));
        }
        if a == placeholder {
            found = true;
        } else if a.contains('{') || a.contains('}') {
            out.push(format!(
                "The {what} command may only use {placeholder} as a whole argument, not {a:?}."
            ));
        }
    }
    if !found {
        out.push(format!(
            "The {what} command must contain {placeholder} as an argument."
        ));
    }
}

fn check_profile(p: &Profile, official: bool, out: &mut Vec<String>) {
    let at = |s: String| format!("{}: {s}", p.id);
    if !plain_id(&p.id) {
        out.push(format!(
            "{:?} is not a valid profile ID (lowercase letters, digits, '.', '_', '-'; at most 64).",
            p.id
        ));
        return;
    }
    if let Some(n) = &p.name
        && (n.trim().is_empty() || !plain_text(n, 100))
    {
        out.push(at(
            "the name must be plain text of at most 100 characters.".into()
        ));
    }
    if let Some(n) = &p.notes
        && !plain_text(n, 1000)
    {
        out.push(at(
            "the notes must be plain text of at most 1000 characters.".into(),
        ));
    }
    if let Some(parent) = &p.parent
        && (!plain_id(parent) || parent == &p.id)
    {
        out.push(at(format!("{parent:?} cannot be its parent.")));
    }
    if let Some(f) = &p.family
        && !plain_id(f)
    {
        out.push(at(format!("{f:?} is not a valid family name.")));
    }
    if let Some(m) = &p.matching {
        for id in m.ids.iter().chain(&m.variants) {
            if !plain_id(id) {
                out.push(at(format!("{id:?} is not a valid os-release ID.")));
            }
        }
        if m.ids.is_empty() {
            out.push(at("match.ids must name at least one os-release ID.".into()));
        }
        for v in m.min_version.iter().chain(&m.max_version) {
            if !version_syntax(v) {
                out.push(at(format!(
                    "{v:?} is not a version (dotted numbers such as 22 or 24.04)."
                )));
            }
        }
    }
    for a in p.arch.iter().flatten() {
        if !ARCHES.contains(&a.as_str()) {
            out.push(at(format!(
                "{a:?} is not a known processor architecture ({}).",
                ARCHES.join(", ")
            )));
        }
    }
    for n in p.names.iter().flatten() {
        if !plain_id(n) {
            out.push(at(format!("{n:?} is not a valid registry key.")));
        }
    }
    for r in p.repositories.iter().flatten() {
        if r.id.is_empty() || !plain_text(&r.id, 200) || Glob::new(&r.id).is_err() {
            out.push(at(format!("{:?} is not a valid repository pattern.", r.id)));
        }
        if let Some(n) = &r.name
            && !plain_text(n, 100)
        {
            out.push(at("repository names must be plain text.".into()));
        }
    }
    for r in p.rules.iter().flatten() {
        if r.app != "*" && !plain_id(&r.app) {
            out.push(at(format!("{:?} is not a registry app ID or *.", r.app)));
        }
        if r.reason.trim().is_empty() || !plain_text(&r.reason, 300) {
            out.push(at(
                "every rule needs a reason of at most 300 characters.".into()
            ));
        }
        if r.action != RuleAction::Skip && r.method.is_none() {
            out.push(at(format!(
                "the {:?} rule for {} must name a method.",
                r.action, r.app
            )));
        }
    }
    match (&p.custom, p.package_manager) {
        (Some(_), _) if official => out.push(at(
            "official profiles may not contain custom commands.".into(),
        )),
        (Some(c), Some(Adapter::Custom) | None) => {
            if c.label.trim().is_empty() || !plain_text(&c.label, 40) {
                out.push(at("the custom package manager needs a short label.".into()));
            }
            let mut problems = Vec::new();
            check_argv("install", &c.install, "{packages}", &mut problems);
            check_argv("query", &c.query, "{package}", &mut problems);
            if !c.available.is_empty() {
                check_argv("available", &c.available, "{package}", &mut problems);
            }
            out.extend(problems.into_iter().map(at));
        }
        (Some(_), Some(_)) => out.push(at(
            "custom commands need \"package_manager\": \"custom\".".into()
        )),
        (None, Some(Adapter::Custom)) => out.push(at(
            "\"package_manager\": \"custom\" needs a \"custom\" section with its commands.".into(),
        )),
        (None, _) => {}
    }
}

/// Every problem with the profiles in `file` combined with the existing ones; empty when they
/// can be saved. `official`: an update from the project, which may not carry commands.
pub fn validate(file: &ProfileFile, base: &Distros, official: bool) -> Vec<String> {
    let mut out = Vec::new();
    if file.distros.is_empty() {
        out.push("The file holds no distribution profiles.".into());
    }
    if file.distros.len() > MAX_PROFILES {
        out.push(format!("The file holds more than {MAX_PROFILES} profiles."));
    }
    if let Some(v) = &file.version
        && !version_syntax(v)
    {
        out.push(format!("{v:?} is not a version (dotted numbers)."));
    }
    let mut ids = HashSet::new();
    for p in &file.distros {
        if !ids.insert(p.id.as_str()) {
            out.push(format!("{} is defined twice.", p.id));
        }
        check_profile(p, official, &mut out);
    }
    if !out.is_empty() {
        return out;
    }
    let combined = if official {
        let mut d = base.clone();
        d.official = Some(file.clone());
        d
    } else {
        base.with_user(file.distros.clone())
    };
    for p in &file.distros {
        match combined.resolve(&p.id) {
            Ok(r) if r.package_manager == Adapter::Custom && r.custom.is_none() => {
                out.push(format!(
                    "{}: the custom package manager has no commands.",
                    p.id
                ));
            }
            Ok(_) => {}
            Err(e) => out.push(e.to_string()),
        }
    }
    out
}

/// What a profile resolves to and how it relates to this computer, for "Validate and test".
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct TestReport {
    pub problems: Vec<String>,
    pub resolved: Option<Resolved>,
    /// It would be chosen for this computer's os-release.
    pub matches_here: bool,
    /// Commands the profile relies on, and whether this computer has them.
    pub tools: Vec<(String, bool)>,
}

/// Validates `file` and reports how its first profile behaves here (`here`: this computer's
/// os-release, if Linux).
pub fn test(file: &ProfileFile, base: &Distros, here: Option<&OsRelease>) -> TestReport {
    let problems = validate(file, base, false);
    let Some(first) = file.distros.first() else {
        return TestReport {
            problems,
            resolved: None,
            matches_here: false,
            tools: Vec::new(),
        };
    };
    let combined = base.with_user(file.distros.clone());
    let resolved = if problems.is_empty() {
        combined.resolve(&first.id).ok()
    } else {
        None
    };
    let matches_here = here.is_some_and(|os| {
        let (r, m) = combined.detect(os);
        r.id == first.id && m != Matched::Generic
    });
    let mut tools: Vec<String> = Vec::new();
    if let Some(r) = &resolved {
        if let Some(t) = r.package_manager.tool() {
            tools.push(t.into());
        }
        if let Some(c) = &r.custom {
            for argv in [&c.install, &c.query, &c.available] {
                if let Some(p) = argv.first()
                    && !tools.contains(p)
                {
                    tools.push(p.clone());
                }
            }
        }
    }
    let tools = tools
        .into_iter()
        .map(|t| {
            let have = if t.starts_with('/') {
                Path::new(&t).is_file()
            } else {
                crate::installers::which(&t).is_some()
            };
            (t, have)
        })
        .collect();
    TestReport {
        problems,
        resolved,
        matches_here,
        tools,
    }
}

// ---------------------------------------------------------------- saving the user's profiles

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir).ctx(|| format!("Creating {}", dir.display()))?;
    let tmp = path.with_extension("tmp");
    let json = serde_json::to_vec_pretty(value).map_err(|e| invalid(e.to_string()))?;
    fs::write(&tmp, json).ctx(|| format!("Writing {}", tmp.display()))?;
    fs::rename(&tmp, path).ctx(|| format!("Saving {}", path.display()))
}

/// The user's profile store under the Simple Setup configuration folder.
pub struct UserStore {
    dir: PathBuf,
}

impl UserStore {
    pub fn at(config_dir: &Path) -> Self {
        Self {
            dir: config_dir.to_path_buf(),
        }
    }

    pub fn open_default() -> Result<Self> {
        Ok(Self::at(crate::store::Store::open_default()?.root()))
    }

    pub fn distros(&self) -> Distros {
        Distros::load(&self.dir)
    }

    fn save(&self, u: &UserProfiles) -> Result<()> {
        write_json(&self.dir.join(USER_FILE), u)
    }

    /// Adds or replaces the profiles in `file` after validating them. Replacing a profile with
    /// different custom commands drops the trust in the old ones. Returns the IDs saved.
    pub fn import(&self, file: &ProfileFile) -> Result<Vec<String>> {
        let d = self.distros();
        let problems = validate(file, &d, false);
        if !problems.is_empty() {
            return Err(invalid(problems.join("\n")));
        }
        let mut u = d.user.clone();
        for p in &file.distros {
            u.profiles.retain(|q| q.id != p.id);
            u.profiles.push(p.clone());
            let keep = p.custom.as_ref().map(|c| command_digest(&p.id, c));
            if u.trusted.get(&p.id) != keep.as_ref() {
                u.trusted.remove(&p.id);
            }
        }
        self.save(&u)?;
        Ok(file.distros.iter().map(|p| p.id.clone()).collect())
    }

    /// Removes a user profile (built-in and official ones can only be switched off).
    pub fn remove(&self, id: &str) -> Result<()> {
        let mut u = self.distros().user;
        let before = u.profiles.len();
        u.profiles.retain(|p| p.id != id);
        if u.profiles.len() == before {
            return Err(invalid(format!(
                "{id} is not one of your profiles; built-in profiles can only be switched off."
            )));
        }
        u.trusted.remove(id);
        u.disabled.retain(|d| d != id);
        // A profile others inherit from cannot go (unless another layer still provides it).
        let left = Distros {
            user: u.clone(),
            ..self.distros()
        };
        if left.get(id).is_none()
            && let Some(child) = left
                .user
                .profiles
                .iter()
                .find(|p| p.parent.as_deref() == Some(id))
        {
            return Err(invalid(format!(
                "{} is based on {id}; remove or change it first.",
                child.id
            )));
        }
        self.save(&u)
    }

    pub fn set_enabled(&self, id: &str, on: bool) -> Result<()> {
        let d = self.distros();
        if d.get(id).is_none() {
            return Err(invalid(format!("No distribution profile {id:?}.")));
        }
        if id == ROOT && !on {
            return Err(invalid("The generic Linux profile cannot be switched off."));
        }
        let mut u = d.user;
        u.disabled.retain(|x| x != id);
        if !on {
            u.disabled.push(id.into());
        }
        self.save(&u)
    }

    /// Records that the user trusts the custom commands of profile `id` as they are now.
    pub fn trust(&self, id: &str, on: bool) -> Result<()> {
        let d = self.distros();
        let mut u = d.user.clone();
        if !on {
            u.trusted.remove(id);
            return self.save(&u);
        }
        let p = u
            .profiles
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| invalid(format!("{id} is not one of your profiles.")))?;
        let c = p
            .custom
            .as_ref()
            .ok_or_else(|| invalid(format!("{id} has no custom commands to trust.")))?;
        u.trusted.insert(id.into(), command_digest(id, c));
        self.save(&u)
    }

    /// The user's profile `id` (or, for others, a copy to start from) as a file to share.
    pub fn export(&self, ids: &[String]) -> Result<ProfileFile> {
        let d = self.distros();
        let distros = ids
            .iter()
            .map(|id| {
                d.get(id)
                    .map(|(p, _)| p.clone())
                    .ok_or_else(|| invalid(format!("No distribution profile {id:?}.")))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ProfileFile {
            schema_version: SCHEMA_VERSION,
            version: None,
            distros,
        })
    }

    /// Saves an official update after checking it (newer version, no commands, valid).
    pub fn save_official(&self, file: &ProfileFile) -> Result<()> {
        let d = self.distros();
        if !newer(file.version.as_deref(), Some(d.current_version())) {
            return Err(invalid(format!(
                "The downloaded profiles (version {}) are not newer than the ones in use ({}).",
                file.version.as_deref().unwrap_or("none"),
                d.current_version()
            )));
        }
        let problems = validate(file, &Distros::builtin(), true);
        if !problems.is_empty() {
            return Err(invalid(format!(
                "The downloaded profiles were refused: {}",
                problems.join("; ")
            )));
        }
        write_json(&self.dir.join(OFFICIAL_FILE), file)
    }
}

/// Result of looking for newer official profiles.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UpdateResult {
    pub updated: bool,
    pub version: String,
    pub message: String,
}

/// Downloads `distros.json` from this project's latest GitHub release, checks it against the
/// release's published SHA-256, validates it and saves it when it is newer.
pub fn update_official(
    store: &UserStore,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<UpdateResult> {
    let text = crate::update::release_text("distros.json", cancel)?;
    let file = parse(&text, false)?;
    let current = store.distros().current_version().to_string();
    if !newer(file.version.as_deref(), Some(&current)) {
        return Ok(UpdateResult {
            updated: false,
            message: format!("The distribution profiles are up to date (version {current})."),
            version: current,
        });
    }
    store.save_official(&file)?;
    let version = file.version.unwrap_or_default();
    Ok(UpdateResult {
        updated: true,
        message: format!("Distribution profiles updated to version {version}."),
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(text: &str) -> OsRelease {
        OsRelease::parse(text).unwrap()
    }

    fn file(json: &str) -> ProfileFile {
        parse(json, false).unwrap()
    }

    #[test]
    fn builtin_profiles_are_valid_and_resolve() {
        let d = Distros::builtin();
        let f = BUILTIN.clone();
        assert_eq!(
            validate(&f, &Distros::builtin(), true),
            Vec::<String>::new()
        );
        for id in d.ids() {
            let r = d.resolve(&id).unwrap();
            assert_eq!(r.chain.last().map(String::as_str), Some(ROOT), "{id}");
        }
        let mint = d.resolve("linuxmint").unwrap();
        assert_eq!(mint.chain, ["linuxmint", "ubuntu", "debian", "linux"]);
        assert_eq!(mint.package_manager, Adapter::Apt);
        assert_eq!(mint.formats, [Format::Deb]);
        assert_eq!(mint.names, ["linuxmint", "ubuntu", "debian", "dpkg"]);
        assert_eq!(mint.snap, Avail::Unsupported);
        assert_eq!(mint.flatpak, Avail::Default);
        // Mint's own Firefox rule replaces Ubuntu's.
        let ff: Vec<_> = mint.rules_for(Some("firefox")).collect();
        assert_eq!(ff[0].action, RuleAction::Prefer);
        assert_eq!(mint.official_repo("Ubuntu"), Some(true));
        assert_eq!(mint.official_repo("linuxmint"), Some(true));
        assert_eq!(mint.official_repo("LP-PPA-someone"), None);
        let suse = d.resolve("opensuse-tumbleweed").unwrap();
        // Fedora's RPM names are not openSUSE's.
        assert!(!suse.names.contains(&"rpm".to_string()));
        assert!(suse.rolling && suse.has_format(Format::Rpm));
        assert_eq!(suse.official_repo("openSUSE-Tumbleweed-Oss"), Some(true));
        let atomic = d.resolve("fedora-atomic").unwrap();
        assert!(atomic.immutable);
        assert_eq!(atomic.package_manager, Adapter::RpmOstree);
        assert_eq!(d.resolve("alpine").unwrap().appimage, Avail::Unsupported);
    }

    #[test]
    fn detection_by_id_variant_like_and_generic() {
        let d = Distros::builtin();
        let (r, m) = d.detect(&os(
            "ID=linuxmint\nID_LIKE=\"ubuntu debian\"\nVERSION_ID=\"22\"\nNAME=\"Linux Mint\"\n",
        ));
        assert_eq!((r.id.as_str(), m), ("linuxmint", Matched::Exact));
        let (r, m) = d.detect(&os("ID=fedora\nVERSION_ID=44\nVARIANT_ID=silverblue\n"));
        assert_eq!((r.id.as_str(), m), ("fedora-atomic", Matched::Exact));
        let (r, _) = d.detect(&os("ID=fedora\nVERSION_ID=44\nVARIANT_ID=workstation\n"));
        assert_eq!(r.id, "fedora");
        let (r, m) = d.detect(&os("ID=nimbus\nID_LIKE=\"ubuntu debian\"\nNAME=Nimbus\n"));
        assert_eq!(
            (r.id.as_str(), m),
            ("ubuntu", Matched::Like("ubuntu".into()))
        );
        let (r, m) = d.detect(&os(
            "ID=rocky\nID_LIKE=\"rhel centos fedora\"\nVERSION_ID=9.4\n",
        ));
        assert_eq!((r.id.as_str(), m), ("rocky", Matched::Exact));
        let (r, m) = d.detect(&os("ID=mystery\nNAME=\"Mystery OS\"\nVERSION_ID=1\n"));
        assert_eq!((r.id.as_str(), m), (ROOT, Matched::Generic));
        assert_eq!(r.name, "Mystery OS 1");
        assert_eq!(r.package_manager, Adapter::None);
    }

    #[test]
    fn version_ranges_pick_the_matching_profile() {
        let d = Distros::builtin().with_user(
            file(r#"{"schema_version":1,"distros":[
                {"id":"old-leap","parent":"opensuse-leap","name":"Old Leap","match":{"ids":["opensuse-leap"],"max_version":"15.4"}}
            ]}"#)
            .distros,
        );
        assert_eq!(
            d.detect(&os("ID=opensuse-leap\nVERSION_ID=15.3\n")).0.id,
            "old-leap"
        );
        assert_eq!(
            d.detect(&os("ID=opensuse-leap\nVERSION_ID=15.6\n")).0.id,
            "opensuse-leap"
        );
    }

    #[test]
    fn a_new_derivative_needs_only_three_fields() {
        let base = Distros::builtin();
        let f = file(
            r#"{"schema_version":1,"distros":[{"id":"nimbus","name":"Nimbus","parent":"ubuntu","match":{"ids":["nimbus"]}}]}"#,
        );
        assert!(validate(&f, &base, false).is_empty());
        let t = tempfile::tempdir().unwrap();
        let s = UserStore::at(t.path());
        assert_eq!(s.import(&f).unwrap(), ["nimbus"]);
        let d = s.distros();
        let (r, m) = d.detect(&os("ID=nimbus\nID_LIKE=ubuntu\nVERSION_ID=3\n"));
        assert_eq!((r.id.as_str(), m), ("nimbus", Matched::Exact));
        assert_eq!(r.package_manager, Adapter::Apt);
        assert_eq!(r.names, ["nimbus", "ubuntu", "debian", "dpkg"]);
        assert_eq!(r.layer, Layer::Custom);
        // Switched off: back to ID_LIKE.
        s.set_enabled("nimbus", false).unwrap();
        let (r, m) = s.distros().detect(&os("ID=nimbus\nID_LIKE=ubuntu\n"));
        assert_eq!(
            (r.id.as_str(), m),
            ("ubuntu", Matched::Like("ubuntu".into()))
        );
        s.remove("nimbus").unwrap();
        assert!(
            s.remove("ubuntu").is_err(),
            "built-in profiles are only switched off"
        );
    }

    #[test]
    fn yaml_profiles_are_read() {
        let y = "schema_version: 1\ndistros:\n  - id: nimbus\n    name: Nimbus\n    parent: ubuntu\n    match:\n      ids: [nimbus]\n";
        let f = parse(y, true).unwrap();
        assert_eq!(f.distros[0].parent.as_deref(), Some("ubuntu"));
        assert!(parse("schema_version: 1\ndistros: [ {id: x, bogus: 1} ]\n", true).is_err());
        // Alias bombs are cut off by the reader's limits.
        let mut bomb = String::from("a: &a [x,x,x,x,x,x,x,x,x,x]\n");
        for i in 0..20 {
            bomb += &format!(
                "b{i}: &b{i} [*{p},*{p},*{p},*{p},*{p},*{p},*{p},*{p},*{p},*{p}]\n",
                p = if i == 0 {
                    "a".to_string()
                } else {
                    format!("b{}", i - 1)
                }
            );
        }
        assert!(parse(&bomb, true).is_err());
    }

    #[test]
    fn malformed_and_hostile_profiles_are_refused() {
        let base = Distros::builtin();
        assert!(parse("{", false).is_err());
        assert!(parse(r#"{"schema_version":9,"distros":[]}"#, false).is_err());
        assert!(parse(&" ".repeat(MAX_BYTES + 1), false).is_err());
        assert!(
            parse(
                r#"{"schema_version":1,"distros":[{"id":"x","shell":"rm -rf /"}]}"#,
                false
            )
            .is_err()
        );
        let bad = |json: &str| -> Vec<String> { validate(&file(json), &base, false) };
        let p = |body: &str| format!(r#"{{"schema_version":1,"distros":[{body}]}}"#);
        assert!(!bad(&p(r#"{"id":"../etc"}"#)).is_empty());
        assert!(!bad(&p(r#"{"id":"x","parent":"x"}"#)).is_empty());
        assert!(
            !bad(&p(r#"{"id":"x","parent":"nope"}"#)).is_empty(),
            "unknown parent"
        );
        assert!(
            !bad(&p(r#"{"id":"a","parent":"b"},{"id":"b","parent":"a"}"#)).is_empty(),
            "cycle"
        );
        assert!(!bad(&p(r#"{"id":"x","arch":["m68k"]}"#)).is_empty());
        assert!(
            !bad(&p(
                r#"{"id":"x","match":{"ids":["x"],"min_version":"latest"}}"#
            ))
            .is_empty()
        );
        assert!(
            !bad(&p(
                r#"{"id":"x","rules":[{"app":"firefox","action":"prefer","reason":"r"}]}"#
            ))
            .is_empty(),
            "prefer needs a method"
        );
        assert!(
            !bad(&p(r#"{"id":"x","package_manager":"custom"}"#)).is_empty(),
            "custom needs commands"
        );
        let custom = |install: &str| {
            p(&format!(
                r#"{{"id":"x","package_manager":"custom","custom":{{"label":"xbps","install":{install},"query":["xbps-query","{{package}}"]}}}}"#
            ))
        };
        assert!(bad(&custom(r#"["xbps-install","-y","{packages}"]"#)).is_empty());
        assert!(
            !bad(&custom(r#"["sh","-c","xbps-install {packages}"]"#)).is_empty(),
            "no shells"
        );
        assert!(!bad(&custom(r#"["/usr/bin/../bin/sh","{packages}"]"#)).is_empty());
        assert!(!bad(&custom(r#"["sudo","xbps-install","{packages}"]"#)).is_empty());
        assert!(
            !bad(&custom(r#"["xbps-install","--x={packages}"]"#)).is_empty(),
            "placeholder must be whole"
        );
        assert!(
            !bad(&custom(r#"["xbps-install","-y"]"#)).is_empty(),
            "placeholder required"
        );
        // Official updates never carry commands.
        let f = file(&custom(r#"["xbps-install","{packages}"]"#));
        assert!(!validate(&f, &base, true).is_empty());
    }

    #[test]
    fn custom_commands_need_trust_and_edits_clear_it() {
        let t = tempfile::tempdir().unwrap();
        let s = UserStore::at(t.path());
        let f = |flag: &str| {
            file(&format!(
                r#"{{"schema_version":1,"distros":[{{"id":"void","name":"Void","match":{{"ids":["void"]}},"package_manager":"custom","formats":[],"custom":{{"label":"xbps","install":["xbps-install","{flag}","{{packages}}"],"query":["xbps-query","{{package}}"]}}}}]}}"#
            ))
        };
        s.import(&f("-y")).unwrap();
        let r = s.distros().resolve("void").unwrap();
        assert!(!r.trusted && r.usable_manager().is_none());
        s.trust("void", true).unwrap();
        let r = s.distros().resolve("void").unwrap();
        assert!(r.trusted);
        assert_eq!(r.usable_manager(), Some(Adapter::Custom));
        // Re-importing the same commands keeps the trust; different ones drop it.
        s.import(&f("-y")).unwrap();
        assert!(s.distros().resolve("void").unwrap().trusted);
        s.import(&f("-Sy")).unwrap();
        assert!(!s.distros().resolve("void").unwrap().trusted);
        // A hand-edited user file cannot fake trust for other commands.
        let mut u = s.distros().user.clone();
        u.trusted.insert("void".into(), "0".repeat(64));
        write_json(&t.path().join(USER_FILE), &u).unwrap();
        assert!(!s.distros().resolve("void").unwrap().trusted);
    }

    #[test]
    fn official_updates_must_be_newer_and_valid() {
        let t = tempfile::tempdir().unwrap();
        let s = UserStore::at(t.path());
        let mut f = BUILTIN.clone();
        assert!(s.save_official(&f).is_err(), "same version");
        f.version = Some("2000.1".into());
        assert!(s.save_official(&f).is_err(), "older version");
        f.version = Some("9999.1".into());
        f.distros[1].parent = Some("nowhere".into());
        assert!(s.save_official(&f).is_err(), "unknown parent");
        let mut f = BUILTIN.clone();
        f.version = Some("9999.1".into());
        f.distros.push(Profile {
            id: "newdistro".into(),
            name: Some("New".into()),
            parent: Some("debian".into()),
            matching: Some(Match {
                ids: vec!["newdistro".into()],
                ..Default::default()
            }),
            ..Default::default()
        });
        s.save_official(&f).unwrap();
        let d = s.distros();
        assert_eq!(d.official_version(), Some("9999.1"));
        assert_eq!(d.resolve("newdistro").unwrap().layer, Layer::Official);
    }

    #[test]
    fn test_report_shows_match_and_tools() {
        let f = file(
            r#"{"schema_version":1,"distros":[{"id":"nimbus","name":"Nimbus","parent":"ubuntu","match":{"ids":["nimbus"]}}]}"#,
        );
        let here = os("ID=nimbus\nID_LIKE=ubuntu\n");
        let r = test(&f, &Distros::builtin(), Some(&here));
        assert!(r.problems.is_empty() && r.matches_here);
        assert_eq!(r.tools[0].0, "apt-get");
        assert!(!test(&f, &Distros::builtin(), Some(&os("ID=fedora\n"))).matches_here);
    }
}
