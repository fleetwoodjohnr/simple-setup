//! Application discovery. Adapters only *report* what is installed; nothing is copied or
//! packaged until the user selects it in the Snapshot Builder.

pub mod devtools;
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

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Flatpak,
    Rpm,
    Dpkg,
    Pacman,
    /// Alpine's apk.
    Apk,
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
    /// A command-line tool installed with `cargo install` (any system).
    Cargo,
    /// A Python application installed with pipx (any system).
    Pipx,
    /// A global npm package (any system).
    Npm,
    /// A package of a distribution whose package manager its profile describes as commands
    /// (Portage, Nix, xbps…); the profile's `names` say which registry key it uses.
    Native,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Desktop,
    CommandLine,
    SourceBuild,
    /// Settings only, nothing to install (shell and command-line settings).
    Settings,
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
    /// Full commit hash of a source checkout, so a rebuild uses exactly the same code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// How a source checkout is built, when it uses a build tool Simple Setup can run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<Build>,
}

/// Build tools Simple Setup can rebuild a GitHub project with on the new computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildTool {
    /// `cargo install --git`
    Cargo,
    /// `go install <module>/...@<commit>`
    Go,
    /// `pipx install git+https://…@<commit>`
    Pipx,
    /// `npm install -g` (developer tools only; projects are not built with it).
    Npm,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub tool: BuildTool,
    /// Cargo package or Go module path, read from `Cargo.toml` / `go.mod`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
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
    /// Who makes it, as the installer or package says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publisher: Option<String>,
    /// The user's choices for this app (preferred ways to install it, release rules…).
    #[serde(default, skip_serializing_if = "AppPrefs::is_default")]
    pub prefs: AppPrefs,
    /// Its icon file on this computer (PNG, SVG, ICO, ICNS or a program holding one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
    /// Kind of app for grouping: Internet, Office, Development, Graphics, Multimedia, Games,
    /// Education, Science, Utilities, System.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Its AppStream ID (`org.kde.krita`), which Flathub uses for the same app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub appstream_id: Option<String>,
}

/// The app group a freedesktop main category (or macOS app category) stands for.
pub fn category_of(word: &str) -> Option<&'static str> {
    let w = word
        .trim()
        .trim_start_matches("public.app-category.")
        .to_ascii_lowercase();
    Some(match w.as_str() {
        "network" | "webbrowser" | "email" | "chat" | "instantmessaging" | "social-networking" => {
            "Internet"
        }
        "office" | "productivity" | "business" | "finance" | "wordprocessor" | "spreadsheet" => {
            "Office"
        }
        "development" | "developer-tools" | "ide" => "Development",
        "graphics" | "graphics-design" | "photography" | "2dgraphics" | "3dgraphics" => "Graphics",
        "audiovideo" | "audio" | "video" | "music" | "entertainment" => "Multimedia",
        "game" | "games" => "Games",
        "education" | "reference" => "Education",
        "science" | "medical" => "Science",
        "utility" | "utilities" => "Utilities",
        "system" | "settings" => "System",
        _ => return None,
    })
}

/// The user's per-app migration rules. They travel in the snapshot so the new computer honours
/// them; `hidden` only affects this computer's lists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct AppPrefs {
    /// Ways to install it to try first, in this order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub prefer: Vec<crate::distros::MethodKind>,
    /// Ways never to use for it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub avoid: Vec<crate::distros::MethodKind>,
    /// Left out of the app lists (an app the user does not want to migrate).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Prereleases of its GitHub project may be installed.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub prerelease: bool,
    /// Which release file to use per platform, when the automatic choice is wrong.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub asset_rules: Vec<AssetRule>,
    /// The user's own install commands per platform. Never run without approval that shows
    /// the exact command, on every computer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub custom_install: Vec<CustomInstall>,
}

impl AppPrefs {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Checks everything a snapshot or a settings file could carry here.
    pub fn check(&self) -> crate::Result<()> {
        for r in &self.asset_rules {
            if r.pattern.is_empty()
                || r.pattern.len() > 200
                || r.pattern
                    .chars()
                    .any(|c| c.is_control() || c == '/' || c == '\\')
                || globset::Glob::new(&r.pattern).is_err()
            {
                return Err(crate::invalid(format!(
                    "Unusable release file pattern {:?}.",
                    r.pattern
                )));
            }
        }
        for c in &self.custom_install {
            c.check()?;
        }
        Ok(())
    }
}

/// A file-name pattern (`*-linux-x86_64.tar.gz`) choosing the GitHub release file for `target`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssetRule {
    pub target: crate::platform::Platform,
    pub pattern: String,
}

/// An install command the user wrote: a program and its arguments, run without a shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CustomInstall {
    pub target: crate::platform::Platform,
    /// Distribution profile it is for (Linux), when it only fits one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    pub argv: Vec<String>,
    /// Run with administrator rights.
    #[serde(default)]
    pub admin: bool,
}

impl CustomInstall {
    pub fn check(&self) -> crate::Result<()> {
        let ok = !self.argv.is_empty()
            && self.argv.len() <= 64
            && !self.argv[0].trim().is_empty()
            && self
                .argv
                .iter()
                .all(|a| a.len() <= 1000 && !a.chars().any(char::is_control))
            && self
                .profile
                .as_deref()
                .is_none_or(crate::manifest::plain_name);
        if ok {
            Ok(())
        } else {
            Err(crate::invalid("Unusable install command."))
        }
    }

    /// The command as shown for approval.
    pub fn display(&self) -> String {
        show_argv(&self.argv)
    }
}

/// A command as shown to people: arguments with spaces or quotes are quoted.
pub fn show_argv(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if a.is_empty() || a.contains([' ', '"', '\'', '\\', '$', '`']) {
                format!("{a:?}")
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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
            publisher: None,
            prefs: AppPrefs::default(),
            icon: None,
            category: None,
            appstream_id: None,
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
        Source::Apk => "apk",
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
        Source::Cargo => "cargo",
        Source::Pipx => "pipx",
        Source::Npm => "npm",
        Source::Native => "native",
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
    /// This computer's distribution profile (Linux).
    pub profile: Option<crate::distros::Resolved>,
}

pub fn scan(opts: &ScanOptions, user: &UserApps) -> Result<Inventory> {
    let device = Device::detect()?;
    let home = dirs::home_dir().unwrap_or_default();
    let profile = (device.platform.os == OsFamily::Linux)
        .then(|| crate::probe::local_linux(&crate::distros::Distros::load_default()))
        .flatten()
        .map(|l| l.profile);
    let mut cx = Ctx {
        home: &home,
        apps: Vec::new(),
        hidden: 0,
        notes: Vec::new(),
        profile,
    };
    let os = device.platform.os;
    match os {
        OsFamily::Linux => linux::scan(&mut cx, opts),
        OsFamily::Windows => windows::scan(&mut cx),
        OsFamily::Macos => macos::scan(&mut cx),
    }
    git::scan(&mut cx, &opts.source_dirs);
    devtools::scan(&mut cx);
    if let Some(d) = devtools::dotfiles(&home, os) {
        cx.apps.push(d);
    }
    user.apply_identity(&mut cx.apps);
    let names = cx.profile.as_ref().map(|p| p.names.clone());
    for app in &mut cx.apps {
        registry::annotate(app, &home, os, names.as_deref());
    }
    merge_raw_duplicates(&mut cx.apps);
    user.apply_locations(&mut cx.apps);

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

/// Stdout of a command that ran (whatever its exit status), or None if it could not start or
/// took longer than 90 seconds.
pub(crate) fn output(cmd: &mut Command) -> Option<(bool, String)> {
    // A listing that waits on a lock (dnf while automatic updates run) is stopped after a
    // while and that part of the scan is skipped, instead of the scan never finishing.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let timer = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(90));
        timer.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let (code, out, _) = crate::installers::run_status(cmd, &stop).ok()?;
    Some((code == Some(0), out))
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
