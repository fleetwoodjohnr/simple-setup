//! Application discovery. Adapters only *report* what is installed; nothing is copied or
//! packaged until the user selects it in the Snapshot Builder.

pub mod git;
pub mod linux;
pub mod macos;
pub mod windows;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::platform::{Device, OsFamily};
use crate::store::UserApps;
use crate::{Result, registry};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Flatpak,
    Rpm,
    Dpkg,
    Pacman,
    Snap,
    AppImage,
    /// Unpackaged program found through a desktop shortcut (portable/custom install).
    Custom,
    /// Source checkout (usually a GitHub clone) in a folder the user approved for scanning.
    GitClone,
    /// Added by hand in the Applications screen.
    Manual,
    /// Windows installed-apps entry (registry uninstall key).
    Arp,
    /// Microsoft Store or sideloaded MSIX package.
    Msix,
    Scoop,
    Choco,
    /// Homebrew formula.
    Brew,
    /// Homebrew cask.
    BrewCask,
    /// macOS application bundle (`.app`) not installed through Homebrew.
    AppBundle,
    /// macOS installer-package receipt.
    Pkg,
    Macports,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Desktop,
    CommandLine,
    SourceBuild,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    Unknown,
    Probable,
    Confirmed,
}

/// Where a piece of information came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Read from a package manager, receipt or file the app owns.
    Detected,
    /// Guessed from naming conventions; may be wrong.
    Inferred,
    /// Entered or corrected by the user.
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LocKind {
    Config,
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Location {
    pub kind: LocKind,
    pub path: PathBuf,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitHub {
    /// `owner/name`.
    pub repo: String,
    /// Tag, branch or commit the install corresponds to, if known.
    pub reference: Option<String>,
    /// Release asset file name, if installed from a release download.
    pub asset: Option<String>,
    pub confidence: Confidence,
    pub origin: Origin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct App {
    /// Unique on this machine: `<source>:<native id>`.
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub source: Source,
    /// Package name, Flatpak ID, snap name, or file path.
    pub native_id: String,
    pub kind: Kind,
    /// Cross-platform identity from the application registry, e.g. `firefox`.
    pub canonical: Option<String>,
    /// Operating systems with a known way to install this app.
    #[serde(default)]
    pub available_on: Vec<OsFamily>,
    /// Repository or remote the package came from, e.g. `flathub`.
    pub repository: Option<String>,
    /// Came with the operating system rather than being added by the user.
    pub system: bool,
    pub install_size: Option<u64>,
    /// Program file, bundle or checkout directory for non-packaged installs.
    pub path: Option<PathBuf>,
    pub locations: Vec<Location>,
    pub github: Option<GitHub>,
    pub confidence: Confidence,
    /// Human-readable reasons behind the identification.
    pub evidence: Vec<String>,
    /// Fields the user corrected by hand.
    pub user_fields: Vec<String>,
    /// Installer downloads the user registered for other computers.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub downloads: Vec<Download>,
}

/// An installer download the user registered for one target platform. It is fetched only while
/// a snapshot is built, and refused unless it matches the SHA-256 given here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Download {
    pub target: crate::platform::Platform,
    /// HTTPS link to the installer, AppImage or disk image.
    pub url: String,
    pub sha256: String,
}

impl Download {
    pub fn check(&self) -> crate::Result<()> {
        if !self.url.starts_with("https://")
            || self
                .url
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(crate::invalid(format!(
                "Use an https:// link: {}",
                self.url
            )));
        }
        if self.sha256.len() != 64 || !self.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(crate::invalid(
                "The SHA-256 checksum must be 64 hexadecimal characters.",
            ));
        }
        Ok(())
    }
}

impl App {
    pub(crate) fn new(source: Source, native_id: &str, name: &str, kind: Kind) -> Self {
        Self {
            id: format!("{}:{native_id}", source_key(source)),
            name: name.to_string(),
            version: None,
            source,
            native_id: native_id.to_string(),
            kind,
            canonical: None,
            available_on: Vec::new(),
            repository: None,
            system: false,
            install_size: None,
            path: None,
            locations: Vec::new(),
            github: None,
            confidence: Confidence::Confirmed,
            evidence: Vec::new(),
            user_fields: Vec::new(),
            downloads: Vec::new(),
        }
    }

    pub(crate) fn add_location(&mut self, kind: LocKind, path: PathBuf, origin: Origin) {
        if path.exists() && !self.locations.iter().any(|l| l.path == path) {
            self.locations.push(Location { kind, path, origin });
        }
    }
}

pub fn source_key(s: Source) -> &'static str {
    match s {
        Source::Flatpak => "flatpak",
        Source::Rpm => "rpm",
        Source::Dpkg => "dpkg",
        Source::Pacman => "pacman",
        Source::Snap => "snap",
        Source::AppImage => "appimage",
        Source::Custom => "custom",
        Source::GitClone => "git",
        Source::Manual => "manual",
        Source::Arp => "arp",
        Source::Msix => "msix",
        Source::Scoop => "scoop",
        Source::Choco => "choco",
        Source::Brew => "brew",
        Source::BrewCask => "brew_cask",
        Source::AppBundle => "app_bundle",
        Source::Pkg => "pkg",
        Source::Macports => "macports",
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Inventory {
    pub device: Device,
    pub scanned_at: String,
    pub apps: Vec<App>,
    /// Libraries and dependencies that are not shown as applications.
    pub hidden_packages: usize,
    /// Adapters that were unavailable or failed, in plain language.
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ScanOptions {
    /// Folders the user approved for finding source checkouts and portable apps.
    /// Nothing outside the standard locations is scanned unless listed here.
    pub source_dirs: Vec<PathBuf>,
}

pub struct Ctx<'a> {
    pub home: &'a Path,
    pub apps: Vec<App>,
    pub hidden: usize,
    pub notes: Vec<String>,
}

pub fn scan(opts: &ScanOptions, user: &UserApps) -> Result<Inventory> {
    let device = Device::detect()?;
    let home = dirs::home_dir().unwrap_or_default();
    let mut cx = Ctx {
        home: &home,
        apps: Vec::new(),
        hidden: 0,
        notes: Vec::new(),
    };
    let os = device.platform.os;
    match os {
        OsFamily::Linux => linux::scan(&mut cx, opts),
        OsFamily::Windows => windows::scan(&mut cx),
        OsFamily::Macos => macos::scan(&mut cx),
    }
    git::scan(&mut cx, &opts.source_dirs);
    for app in &mut cx.apps {
        registry::annotate(app, &home, os);
    }
    merge_raw_duplicates(&mut cx.apps);
    user.apply(&mut cx.apps);

    let mut seen = HashSet::new();
    cx.apps.retain(|a| seen.insert(a.id.clone()));
    cx.apps
        .sort_by_cached_key(|a| (a.name.to_lowercase(), a.id.clone()));
    Ok(Inventory {
        device,
        scanned_at: crate::now(),
        apps: cx.apps,
        hidden_packages: cx.hidden,
        notes: cx.notes,
    })
}

/// Stdout of a command that ran (whatever its exit status), or None if it could not start.
// ponytail: no timeout; listing commands return in seconds. Add one if a manager hangs on a lock.
pub(crate) fn output(cmd: &mut Command) -> Option<(bool, String)> {
    let out = cmd.env("LC_ALL", "C").output().ok()?;
    Some((
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    ))
}

/// Stdout of a command that succeeded.
pub(crate) fn run(cmd: &str, args: &[&str]) -> Option<String> {
    output(Command::new(cmd).args(args))
        .filter(|(ok, _)| *ok)
        .map(|(_, s)| s)
}

/// A program seen both as a raw install (Windows installed-apps entry, macOS app bundle) and
/// through a package manager is shown once, as the package-manager install, which is the one
/// that can be reinstalled.
pub fn merge_raw_duplicates(apps: &mut Vec<App>) {
    let raw = |s: Source| matches!(s, Source::Arp | Source::AppBundle);
    let mut i = 0;
    while i < apps.len() {
        let twin = apps[i]
            .canonical
            .as_deref()
            .filter(|_| raw(apps[i].source))
            .and_then(|c| {
                apps.iter()
                    .position(|a| !raw(a.source) && a.canonical.as_deref() == Some(c))
            });
        let Some(j) = twin else {
            i += 1;
            continue;
        };
        let r = apps.remove(i);
        let m = &mut apps[if j > i { j - 1 } else { j }];
        m.path = m.path.take().or(r.path);
        m.install_size = m.install_size.or(r.install_size);
        m.version = m.version.take().or(r.version);
        m.evidence
            .push(format!("Also listed as \"{}\" ({})", r.name, r.native_id));
        for l in r.locations {
            if !m.locations.iter().any(|x| x.path == l.path) {
                m.locations.push(l);
            }
        }
    }
}
