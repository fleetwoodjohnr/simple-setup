//! Installing apps the snapshot holds no installer for, on whatever system it is restored on.
//!
//! [`choose`] lists the ways this computer can install an app, best first, from what is known
//! about it: its own package ID, the application registry's IDs for each package manager, its
//! GitHub repository (release downloads) and, for source checkouts, its build tool. Nothing is
//! matched by name, so an unrelated app is never substituted. [`install`] tries them in order
//! until one works and the app is confirmed installed.
//!
//! Each [`Method`] variant is one provider with fixed command lines; package names are checked
//! with [`plain_name`] before they reach a command. Adding an installation method means adding
//! a variant and its arms here.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::discovery::{App, Build, BuildTool, Confidence, GitHub, Kind, Origin, Source};
use crate::installers::{
    self, Collect, Elevation, admin_cmd, cask_token, github_repo, release_tag, run, run_status,
    succeeds, winget_id,
};
use crate::manifest::{ManifestApp, Recipe, plain_name};
use crate::platform::{OsFamily, Platform};
use crate::probe::SystemInfo;
use crate::{Error, registry};

const FLATHUB: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";

/// The package manager behind a [`Method::System`] install: the destination profile's adapter.
pub use crate::distros::Adapter as Manager;

/// What the user allows a restore to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Prefs {
    /// Build apps from their source code when nothing else installs them.
    pub source: bool,
    pub snap: bool,
    /// Verified downloads (GitHub releases, vendor installers).
    pub downloads: bool,
    /// Flatpak before the distribution's own packages.
    pub prefer_flatpak: bool,
    /// Downloads whose source publishes no checksum may be installed (approved by the user).
    pub unverified: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            source: true,
            snap: true,
            downloads: true,
            prefer_flatpak: false,
            unverified: false,
        }
    }
}

impl Prefs {
    pub fn from_settings(s: &crate::store::Settings) -> Self {
        Self {
            source: s.allow_source_builds,
            snap: s.allow_snap,
            downloads: s.allow_downloads,
            prefer_flatpak: s.prefer_flatpak,
            unverified: s.allow_unverified_downloads,
        }
    }
}

/// Package sources whose names belong to a distribution's own package manager.
fn native_source(s: Source) -> bool {
    matches!(s, Source::Rpm | Source::Dpkg | Source::Pacman | Source::Apk)
}

/// Registry keys of the package names on the computer an app came from, for snapshots that do
/// not record the source distribution: the old one-key-per-format behaviour.
pub fn legacy_namespaces(s: Source) -> Vec<String> {
    if native_source(s) {
        vec![crate::discovery::source_key(s).to_string()]
    } else {
        Vec::new()
    }
}

/// How an app reaches this computer when the snapshot has no installer for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Method {
    /// `flatpak install --user flathub <id>` (no administrator rights).
    Flathub(String),
    Winget(String),
    Brew {
        name: String,
        cask: bool,
    },
    /// The distribution's package manager (administrator rights).
    System(Manager, String),
    Snap(String),
    /// Scoop (Windows; per user, no administrator rights).
    Scoop(String),
    /// Chocolatey (Windows; administrator rights through the consent prompt).
    Choco(String),
    /// A Microsoft Store app by its Store ID, through winget's `msstore` source.
    Store(String),
    /// The installer a catalog lists (winget manifest, Homebrew cask) or the app's GitHub
    /// release offers, downloaded and checked against its published SHA-256.
    Download,
    /// Built from the project's source code at the recorded commit or tag.
    Source {
        repo: String,
        build: Build,
        rev: Option<String>,
    },
    /// Built from a GitHub project's source when nothing else installs it; how (Cargo, Go,
    /// pipx) is read from the repository when it runs.
    SourceLookup {
        repo: String,
        rev: Option<String>,
    },
    /// The user's own command for this app and system, run without a shell.
    Custom {
        argv: Vec<String>,
        admin: bool,
    },
}

/// Marks the user's own commands in plans; approving the plan never approves these silently.
pub const CUSTOM_MARK: &str = "your own install command";

/// Where an install item comes from, for grouping in the restore preview.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Installer files in the snapshot (no internet needed).
    Snapshot,
    /// A package manager or a verified download.
    Online,
    /// Built from source code.
    Source,
}

impl Method {
    pub fn label(&self, os: OsFamily) -> String {
        match self {
            Self::Flathub(id) => format!("Flatpak from Flathub ({id})"),
            Self::Winget(id) => format!("winget ({id})"),
            Self::Brew { name, cask: true } => format!("Homebrew cask {name}"),
            Self::Brew { name, .. } => format!("Homebrew formula {name}"),
            Self::System(m, p) => format!("{} package {p}", m.label()),
            Self::Snap(n) => format!("snap {n}"),
            Self::Scoop(n) => format!("Scoop package {n}"),
            Self::Choco(n) => format!("Chocolatey package {n}"),
            Self::Store(n) => format!("Microsoft Store app {n}"),
            Self::Download => match os {
                OsFamily::Windows => {
                    "a verified download (winget catalog or GitHub release)".into()
                }
                OsFamily::Macos => {
                    "a verified download (Homebrew cask catalog or GitHub release)".into()
                }
                OsFamily::Linux => "a verified download from its GitHub release".into(),
            },
            Self::Source { repo, build, rev } => format!(
                "building github.com/{repo}{} from source with {}",
                rev.as_deref()
                    .map(|r| format!(" at {}", short(r)))
                    .unwrap_or_default(),
                tool_label(build.tool)
            ),
            Self::SourceLookup { repo, rev } => format!(
                "building github.com/{repo}{} from source (Cargo, Go or pipx, as the project uses)",
                rev.as_deref()
                    .map(|r| format!(" at {}", short(r)))
                    .unwrap_or_default()
            ),
            Self::Custom { argv, .. } => {
                format!("{CUSTOM_MARK}: {}", crate::discovery::show_argv(argv))
            }
        }
    }

    /// Asks for an administrator password (unless already running as administrator).
    pub fn admin(&self) -> bool {
        matches!(self, Self::System(..) | Self::Snap(_) | Self::Choco(_))
            || matches!(self, Self::Custom { admin: true, .. })
    }

    pub fn channel(&self) -> Channel {
        match self {
            Self::Source { .. } | Self::SourceLookup { .. } => Channel::Source,
            _ => Channel::Online,
        }
    }
}

fn short(rev: &str) -> &str {
    if rev.len() == 40 { &rev[..12] } else { rev }
}

pub fn tool_label(t: BuildTool) -> &'static str {
    match t {
        BuildTool::Cargo => "Cargo",
        BuildTool::Go => "Go",
        BuildTool::Pipx => "pipx",
    }
}

fn tool_command(t: BuildTool) -> &'static str {
    match t {
        BuildTool::Cargo => "cargo",
        BuildTool::Go => "go",
        BuildTool::Pipx => "pipx",
    }
}

/// The registry entry that installs a build tool (`rust`, `go`, `pipx`).
pub fn toolchain(t: BuildTool) -> &'static str {
    match t {
        BuildTool::Cargo => "rust",
        BuildTool::Go => "go",
        BuildTool::Pipx => "pipx",
    }
}

/// The app as discovery describes it, with the registry's GitHub repository filled in when the
/// app has none of its own; the catalog code works on this.
pub fn as_app(a: &ManifestApp) -> App {
    let mut app = App::new(a.source, &a.native_id, &a.name, Kind::Desktop);
    app.id = a.id.clone();
    app.version = a.version.clone();
    app.canonical = a.canonical.clone();
    app.repository = a.repository.clone();
    // The links the user registered travel in version 4 snapshots, so a restore can use them.
    app.downloads = a.downloads.clone();
    app.prefs = a.prefs.clone();
    app.publisher = a.publisher.clone();
    app.available_on = a.available_on.clone();
    app.github = a.github.clone().or_else(|| {
        let repo = a
            .canonical
            .as_deref()
            .and_then(registry::get)?
            .github
            .clone()?;
        Some(GitHub {
            repo,
            reference: None,
            asset: None,
            confidence: Confidence::Probable,
            origin: Origin::Inferred,
            commit: None,
            build: None,
        })
    });
    app
}

/// A stand-in app for a build tool, so it is installed like any other app.
pub fn toolchain_app(t: BuildTool) -> ManifestApp {
    let id = toolchain(t);
    let name = registry::get(id).map_or(tool_label(t), |e| e.name.as_str());
    ManifestApp {
        id: format!("toolchain:{id}"),
        name: name.to_string(),
        version: None,
        source: Source::Manual,
        native_id: id.to_string(),
        canonical: Some(id.to_string()),
        repository: None,
        github: None,
        confidence: Confidence::Confirmed,
        evidence: Vec::new(),
        installer: true,
        config: false,
        data: false,
        ..ManifestApp::default()
    }
}

/// The ways the computer described by `info` can install `a`, best first; empty if there is
/// none. `from`: the registry keys of the app's package names where it came from (see
/// [`crate::distros::Resolved::namespaces`]); its own package name is reused only on a
/// distribution sharing one. Linux installs follow the distribution profile: only its package
/// manager, its names from the registry, and the portable formats it supports. Nothing is
/// matched by name, so an unrelated app is never substituted.
pub fn choose(a: &ManifestApp, from: &[String], info: &SystemInfo, prefs: &Prefs) -> Vec<Method> {
    let app = as_app(a);
    let entry = a.canonical.as_deref().and_then(registry::get);
    // The registry's names, and those the snapshot carries (version 4), so an app this
    // computer's Simple Setup does not know is still found by its exact identifiers.
    let ids = |key: &str| -> Vec<String> {
        let mut v: Vec<String> = entry
            .and_then(|e| e.packages.get(key))
            .cloned()
            .unwrap_or_default();
        for i in a.identifiers.get(key).into_iter().flatten() {
            if !v.contains(i) {
                v.push(i.clone());
            }
        }
        v
    };
    // The app's own ID first (same packaging elsewhere), then the registry's.
    let named = |key: &str, src: Source| -> Vec<String> {
        let mut v: Vec<String> = (a.source == src)
            .then(|| a.native_id.clone())
            .into_iter()
            .collect();
        v.extend(ids(key));
        v.retain(|n| plain_name(n));
        v.dedup();
        v
    };
    let mut out = Vec::new();
    let os = info.device.platform.os;
    let profile = info.profile();
    let manager = profile
        .and_then(|p| p.usable_manager())
        .filter(|m| m.tool().is_none_or(|t| info.has(t)));
    // Downloads go through curl; on Linux it is installed first when missing (see `needs`).
    let can_download =
        prefs.downloads && (info.has("curl") || (os == OsFamily::Linux && manager.is_some()));
    let store = matches!(a.source, Source::Msix | Source::AppBundle) && a.repository.is_some();
    match os {
        OsFamily::Linux => {
            let mut native: Vec<String> = Vec::new();
            if let (Some(p), Some(_)) = (profile, manager) {
                if native_source(a.source) && from.iter().any(|k| p.namespaces.contains(k)) {
                    native.push(a.native_id.clone());
                }
                match entry {
                    Some(e) => native.extend(p.package_names(&e.packages)),
                    None => native.extend(p.package_names(&a.identifiers)),
                }
            }
            native.retain(|n| plain_name(n));
            let mut seen = HashSet::new();
            native.retain(|n| seen.insert(n.clone()));
            let flatpaks: Vec<Method> = named("flatpak", Source::Flatpak)
                .into_iter()
                .map(Method::Flathub)
                .collect();
            let snaps: Vec<Method> = named("snap", Source::Snap)
                .into_iter()
                .map(Method::Snap)
                .collect();
            // Flatpak can come from the distribution's own packages first (installed in the
            // same batch, see `needs`) where the profile says it is available but not set up.
            let flatpak = info.has("flatpak") || flatpak_installable(info);
            let snap = info.has("snap") && prefs.snap;
            // The app's own packaging first: a Flatpak's or snap's settings live inside its
            // sandbox folders, which only that packaging reads.
            if a.source == Source::Flatpak && flatpak {
                out.extend(flatpaks.iter().take(1).cloned());
            }
            if a.source == Source::Snap && snap {
                out.extend(snaps.iter().take(1).cloned());
            }
            if prefs.prefer_flatpak && flatpak {
                out.extend(flatpaks.iter().cloned());
            }
            if let Some(m) = manager {
                out.extend(native.into_iter().map(|p| Method::System(m, p)));
            }
            // A Flatpak that needs Flatpak installed first comes after a snap that does not.
            let ready = info.has("flatpak");
            if flatpak && ready {
                out.extend(flatpaks.iter().cloned());
            }
            if snap {
                out.extend(snaps);
            }
            if flatpak && !ready {
                out.extend(flatpaks);
            }
            // Downloaded Linux programs are built for glibc; a profile says when they cannot run.
            let portable = profile.is_none_or(|p| p.appimage != crate::distros::Avail::Unsupported);
            if github_repo(&app).is_some() && can_download && portable {
                out.push(Method::Download);
            }
        }
        OsFamily::Windows if store && a.source == Source::Msix => {}
        OsFamily::Windows => {
            let winget = winget_id(&app).filter(|w| plain_name(w));
            let scoop = named("scoop", Source::Scoop);
            let choco = named("choco", Source::Choco);
            // The app's own package manager first, as it was installed on the old computer.
            if a.source == Source::Scoop && info.has("scoop") {
                out.extend(scoop.iter().take(1).cloned().map(Method::Scoop));
            }
            if a.source == Source::Choco && info.has("choco") {
                out.extend(choco.iter().take(1).cloned().map(Method::Choco));
            }
            if let Some(w) = &winget
                && info.has("winget")
            {
                out.push(Method::Winget(w.clone()));
            }
            if info.has("winget") {
                out.extend(
                    ids("msstore")
                        .into_iter()
                        .filter(|i| plain_name(i))
                        .map(Method::Store),
                );
            }
            if info.has("scoop") {
                out.extend(scoop.into_iter().map(Method::Scoop));
            }
            if info.has("choco") {
                out.extend(choco.into_iter().map(Method::Choco));
            }
            if (winget.is_some() || github_repo(&app).is_some()) && can_download {
                out.push(Method::Download);
            }
        }
        OsFamily::Macos if store && a.source == Source::AppBundle => {}
        OsFamily::Macos => {
            let cask = cask_token(&app).filter(|c| plain_name(c));
            let formulae = named("brew", Source::Brew);
            if info.has("brew") {
                if let Some(c) = &cask {
                    out.push(Method::Brew {
                        name: c.clone(),
                        cask: true,
                    });
                }
                out.extend(
                    formulae
                        .into_iter()
                        .map(|name| Method::Brew { name, cask: false }),
                );
            }
            if (cask.is_some() || github_repo(&app).is_some()) && can_download {
                out.push(Method::Download);
            }
        }
    }
    if prefs.source
        && let Some(gh) = &app.github
        && let Some(build) = &gh.build
        && github_repo(&app).is_some()
        && (build.tool != BuildTool::Go || build.name.is_some())
    {
        out.push(Method::Source {
            repo: gh.repo.clone(),
            build: build.clone(),
            rev: gh
                .commit
                .clone()
                .or_else(|| release_tag(gh.reference.as_deref()).map(String::from)),
        });
    }
    // A GitHub project with no fitting release and no checkout here: built from its source
    // as the last way, when the user allows source builds (always with approval).
    // Only for command-line style installs: desktop app formats (AppImage, Flatpak, snap, app
    // bundles, Windows installers) are rarely built with Cargo, Go or pipx.
    let buildable = !matches!(
        a.source,
        Source::AppImage
            | Source::Flatpak
            | Source::Snap
            | Source::AppBundle
            | Source::BrewCask
            | Source::Msix
            | Source::Arp
            | Source::Pkg
    );
    // Only when no package manager offers it: a download alone may find no fitting release.
    let only_downloads = out.iter().all(|m| matches!(m, Method::Download));
    if prefs.source
        && buildable
        && only_downloads
        && let Some(repo) = github_repo(&app)
        && !out.iter().any(|m| matches!(m, Method::Source { .. }))
        && app.github.as_ref().is_some_and(|g| g.build.is_none())
    {
        out.push(Method::SourceLookup {
            repo: repo.to_string(),
            rev: app
                .github
                .as_ref()
                .and_then(|g| release_tag(g.reference.as_deref()))
                .map(String::from),
        });
    }
    // The user's own commands for this system (and distribution, when they name one).
    for c in &a.prefs.custom_install {
        let fits = c.target == info.device.platform
            && c.profile
                .as_deref()
                .is_none_or(|p| profile.is_some_and(|r| r.chain.iter().any(|x| x == p)))
            && c.check().is_ok();
        if fits {
            out.push(Method::Custom {
                argv: c.argv.clone(),
                admin: c.admin,
            });
        }
    }
    let mut seen = HashSet::new();
    out.retain(|m| seen.insert(m.clone()));
    out
}

/// System packages the chosen methods depend on that this computer lacks: `curl` for
/// downloads (Linux), installed in the same batch as the apps' own packages.
pub fn needs(methods: &[&Method], info: &SystemInfo) -> Vec<(Manager, String)> {
    let download = methods
        .iter()
        .any(|m| matches!(m, Method::Download | Method::Source { .. }));
    let mgr = info
        .profile()
        .and_then(|p| p.usable_manager())
        .filter(|m| m.tool().is_some_and(|t| info.has(t)) && !m.needs_restart());
    let flathub = methods.iter().any(|m| matches!(m, Method::Flathub(_)));
    let mut out = Vec::new();
    if let Some(m) = mgr
        && info.device.platform.os == OsFamily::Linux
    {
        if download && !info.has("curl") {
            out.push((m, "curl".to_string()));
        }
        if flathub && !info.has("flatpak") {
            out.push((m, "flatpak".to_string()));
        }
    }
    out
}

/// Flatpak is not installed, but the distribution's own packages offer it.
pub fn flatpak_installable(info: &SystemInfo) -> bool {
    !info.has("flatpak")
        && info
            .profile()
            .is_some_and(|p| p.flatpak == crate::distros::Avail::Available)
        && info
            .profile()
            .and_then(|p| p.usable_manager())
            .is_some_and(|m| m.tool().is_some_and(|t| info.has(t)) && !m.needs_restart())
}

/// Methods for a stand-in toolchain app: never built from source.
pub fn choose_toolchain(t: BuildTool, info: &SystemInfo) -> Vec<Method> {
    let prefs = Prefs {
        source: false,
        ..Prefs::default()
    };
    choose(&toolchain_app(t), &[], info, &prefs)
}

/// Where a build tool is, including the usual install folders not yet on this process's PATH
/// (a toolchain installed moments ago by this restore).
pub fn tool_path(t: BuildTool, home: &Path) -> Option<PathBuf> {
    let cmd = tool_command(t);
    installers::which(cmd).or_else(|| {
        let exe = if cfg!(windows) {
            format!("{cmd}.exe")
        } else {
            cmd.to_string()
        };
        [
            home.join(".cargo/bin"),
            home.join(".local/bin"),
            home.join("go/bin"),
            PathBuf::from("/usr/local/go/bin"),
            PathBuf::from(r"C:\Program Files\Go\bin"),
        ]
        .into_iter()
        .map(|d| d.join(&exe))
        .find(|p| p.is_file())
    })
}

/// Read-only check whether a method's package is already installed (no network, quick).
/// `info` supplies a custom adapter's query command.
pub fn present(m: &Method, info: &SystemInfo) -> bool {
    match m {
        Method::Flathub(id) => succeeds("flatpak", &["info", id]),
        Method::System(Manager::Apt, p) => installers::deb_installed(p),
        Method::System(
            Manager::Dnf | Manager::Zypper | Manager::RpmOstree | Manager::TransactionalUpdate,
            p,
        ) => succeeds("rpm", &["-q", p]),
        Method::System(Manager::Pacman, p) => succeeds("pacman", &["-Q", p]),
        Method::System(Manager::Apk, p) => succeeds("apk", &["info", "-e", p]),
        Method::System(Manager::Custom, p) => custom(info)
            .and_then(|c| {
                Some(
                    Command::new(&c.query[0])
                        .args(expand(&c.query[1..], "{package}", &[p.as_str()]))
                        .output()
                        .ok()?
                        .status
                        .success(),
                )
            })
            .unwrap_or(false),
        Method::System(Manager::None, _) => false,
        Method::Snap(n) => succeeds("snap", &["list", n]),
        Method::Brew { name, cask } => succeeds(
            "brew",
            &["list", if *cask { "--cask" } else { "--formula" }, name],
        ),
        Method::Winget(id) => succeeds(
            "winget",
            &[
                "list",
                "--id",
                id,
                "--exact",
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        ),
        Method::Scoop(n) => succeeds("cmd", &["/c", "scoop", "prefix", n]),
        Method::Choco(n) => Command::new("choco")
            .args(["list", "--exact", n, "--limit-output"])
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| {
                String::from_utf8_lossy(&o.stdout).lines().any(|l| {
                    l.split('|')
                        .next()
                        .is_some_and(|x| x.eq_ignore_ascii_case(n))
                })
            }),
        Method::Store(id) => succeeds(
            "winget",
            &[
                "list",
                "--id",
                id,
                "--exact",
                "--source",
                "msstore",
                "--accept-source-agreements",
                "--disable-interactivity",
            ],
        ),
        Method::Download
        | Method::Source { .. }
        | Method::SourceLookup { .. }
        | Method::Custom { .. } => false,
    }
}

/// The destination's trusted custom package manager, if it has one.
fn custom(info: &SystemInfo) -> Option<&crate::distros::CustomAdapter> {
    let p = info.profile()?;
    (p.trusted && p.package_manager == Manager::Custom)
        .then_some(p.custom.as_ref())
        .flatten()
        .filter(|c| !c.install.is_empty() && !c.query.is_empty())
}

/// A custom adapter's arguments with `placeholder` replaced by `values` (several arguments).
pub fn expand(args: &[String], placeholder: &str, values: &[&str]) -> Vec<OsString> {
    let mut out = Vec::new();
    for a in args {
        if a == placeholder {
            out.extend(values.iter().map(OsString::from));
        } else {
            out.push(OsString::from(a));
        }
    }
    out
}

/// The exact command a custom adapter runs, for showing before it is approved.
pub fn custom_command(info: &SystemInfo, packages: &[&str]) -> Option<String> {
    let c = custom(info)?;
    let args = expand(&c.install, "{packages}", packages);
    Some(
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// What a restore needs to run installs.
pub struct Ctx<'a> {
    pub home: &'a Path,
    pub platform: Platform,
    pub info: &'a SystemInfo,
    pub elev: Elevation,
    /// Folder for downloaded installers (kept, so a second restore does not download again).
    pub downloads: &'a Path,
    pub cancel: &'a AtomicBool,
    /// See [`Prefs::unverified`].
    pub unverified: bool,
}

/// The result of trying an app's methods.
#[derive(Debug, Clone, Default)]
pub struct Attempt {
    pub ok: bool,
    pub message: String,
    /// What was used: method, release, file and checksum, commit.
    pub details: Vec<String>,
    pub restart: bool,
    pub cancelled: bool,
}

/// Tries `methods` in order until one installs `a` and it is confirmed. Methods in `skip`
/// (already tried, e.g. in a batch) are passed over.
pub fn install(
    a: &ManifestApp,
    methods: &[Method],
    skip: &HashSet<Method>,
    cx: &Ctx,
    status: &mut dyn FnMut(String),
) -> Attempt {
    let os = cx.platform.os;
    let mut failed = Vec::new();
    for m in methods.iter().filter(|m| !skip.contains(m)) {
        let label = m.label(os);
        if present(m, cx.info) {
            return Attempt {
                ok: true,
                message: format!("{} is already installed ({label}).", a.name),
                details: vec![format!("Found with {label}.")],
                ..Attempt::default()
            };
        }
        status(format!("{}: installing with {label}", a.name));
        match run_method(a, m, cx, status) {
            Ok((details, restart)) => {
                return Attempt {
                    ok: true,
                    message: format!(
                        "Installed with {label}.{}",
                        if restart {
                            " Restart the computer to finish."
                        } else {
                            ""
                        }
                    ),
                    details,
                    restart,
                    cancelled: false,
                };
            }
            Err(Error::Cancelled) => {
                return Attempt {
                    message: "Cancelled.".into(),
                    cancelled: true,
                    ..Attempt::default()
                };
            }
            Err(e) => failed.push(format!("{label}: {e}")),
        }
    }
    Attempt {
        message: if failed.is_empty() {
            format!("No way to install {} was left to try.", a.name)
        } else {
            format!("Could not install {}. Tried {}", a.name, failed.join("; "))
        },
        details: failed,
        ..Attempt::default()
    }
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn go(cmd: &mut Command, cx: &Ctx) -> crate::Result<String> {
    run_waiting(cmd, cx.cancel, &mut |_| {}, LOCK_PAUSE)
}

/// The package manager's lock is held by another program (automatic updates, a software
/// centre, another terminal): worth waiting for, not a failure.
fn lock_held(error: &str) -> bool {
    const SIGNS: [&str; 10] = [
        "Could not get lock",
        "Unable to acquire the dpkg frontend lock",
        "unable to lock database",
        "Unable to lock database",
        "System management is locked",
        "Failed to obtain rpm transaction lock",
        "is currently holding the yum lock",
        "change in progress",
        "Another installation is already in progress",
        "code 1618",
    ];
    SIGNS.iter().any(|s| error.contains(s))
}

const LOCK_PAUSE: Duration = Duration::from_secs(15);
/// How long another installer may hold the lock before the install is given up.
const LOCK_WAIT: Duration = Duration::from_secs(600);

/// Runs a package-manager command, waiting (and saying so) while another program holds its
/// lock, then trying again.
fn run_waiting(
    cmd: &mut Command,
    cancel: &AtomicBool,
    status: &mut dyn FnMut(String),
    pause: Duration,
) -> crate::Result<String> {
    let started = std::time::Instant::now();
    loop {
        match run(cmd, cancel) {
            Err(e) if lock_held(&e.to_string()) && started.elapsed() < LOCK_WAIT => {
                status(format!(
                    "Another program is installing software right now; waiting for it to finish ({e})"
                ));
                crate::diag::log(&format!("waiting for a package manager lock: {e}"));
                let until = std::time::Instant::now() + pause;
                while std::time::Instant::now() < until {
                    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                        return Err(crate::Error::Cancelled);
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
            r => return r,
        }
    }
}

/// Runs one method; `Ok((details, restart needed))` only once the app is confirmed.
fn run_method(
    a: &ManifestApp,
    m: &Method,
    cx: &Ctx,
    status: &mut dyn FnMut(String),
) -> crate::Result<(Vec<String>, bool)> {
    let not_found = || crate::invalid("It finished, but the app is not installed afterwards.");
    let mut restart = false;
    let mut details = vec![format!("Installed with {}.", m.label(cx.platform.os))];
    match m {
        Method::Flathub(id) => {
            if !cx.info.flathub {
                go(
                    Command::new("flatpak").args([
                        "remote-add",
                        "--user",
                        "--if-not-exists",
                        "flathub",
                        FLATHUB,
                    ]),
                    cx,
                )?;
            }
            go(
                Command::new("flatpak").args([
                    "install",
                    "--user",
                    "-y",
                    "--noninteractive",
                    "flathub",
                    id,
                ]),
                cx,
            )?;
        }
        Method::Winget(_) | Method::Store(_) => {
            let mut c = Command::new("winget");
            match m {
                Method::Store(id) => c.args(["install", "--source", "msstore", "--id", id]),
                Method::Winget(id) => c.args(["install", "--id", id]),
                _ => &mut c,
            };
            let (code, _, why) = run_status(
                c.args([
                    "--exact",
                    "--silent",
                    "--accept-package-agreements",
                    "--accept-source-agreements",
                    "--disable-interactivity",
                ]),
                cx.cancel,
            )?;
            // winget's own codes: 0x8A15002B already installed, 0x8A150109 restart to finish.
            match code.map(|c| c as u32) {
                Some(0 | 0x8A15_002B) => {}
                Some(0x8A15_0109 | 3010 | 1641) => restart = true,
                Some(c) => return Err(crate::invalid(format!("{why} (code {c:#x})"))),
                None => return Err(crate::invalid(why)),
            }
        }
        // Scoop's command is a script shim (scoop.cmd); the name is a checked plain name.
        Method::Scoop(n) => {
            go(Command::new("cmd").args(["/c", "scoop", "install", n]), cx)?;
        }
        Method::Choco(n) => {
            let args = ["install", n.as_str(), "-y", "--no-progress"];
            if cx.info.elevated {
                go(Command::new("choco").args(args), cx)?;
            } else {
                // Chocolatey needs administrator rights: Windows' consent prompt grants them.
                restart |= installers::run_as_admin("choco", &args).map_err(crate::invalid)?;
            }
        }
        Method::Brew { name, cask } => {
            let mut c = Command::new("brew");
            c.arg("install");
            if *cask {
                c.arg("--cask");
            }
            c.arg(name)
                .env("NONINTERACTIVE", "1")
                .env("HOMEBREW_NO_ANALYTICS", "1")
                .env("HOMEBREW_NO_ENV_HINTS", "1");
            go(&mut c, cx)?;
        }
        Method::System(mgr, p) => {
            let done = batch(&[(*mgr, p.clone())], cx, status);
            if mgr.needs_restart() {
                // Layered onto the system image: there from the next start.
                if done.is_empty() {
                    return Err(not_found());
                }
                details.push("Takes effect after a restart.".into());
                return Ok((details, true));
            }
        }
        Method::Snap(n) => {
            let snap = |classic: bool| {
                let mut a = os(&["install"]);
                if classic {
                    a.push("--classic".into());
                }
                a.push(n.into());
                run(admin_cmd("snap", true, cx.elev).args(a), cx.cancel)
            };
            match snap(false) {
                Err(e) if e.to_string().contains("classic") => {
                    snap(true)?;
                }
                r => {
                    r?;
                }
            }
        }
        Method::Download => {
            let (recipe, evidence) = download(a, cx, status)?;
            details = evidence;
            match installers::check(&recipe, cx.info, cx.home) {
                Ok(true) => {}
                Ok(false) => {
                    let done =
                        installers::install(&app_dir(a, cx), &recipe, cx.home, cx.elev, status)
                            .map_err(crate::invalid)?;
                    restart = done.restart;
                    details.push(done.message);
                }
                Err(why) => return Err(crate::invalid(why)),
            }
            if !installers::installed(&recipe, cx.home) {
                return Err(not_found());
            }
            return Ok((details, restart));
        }
        Method::Source { repo, build, rev } => {
            // The build tool comes first, from this computer's package managers, when missing.
            if tool_path(build.tool, cx.home).is_none() {
                let tc = toolchain_app(build.tool);
                let methods = choose_toolchain(build.tool, cx.info);
                let at = install(&tc, &methods, &HashSet::new(), cx, status);
                if at.cancelled {
                    return Err(Error::Cancelled);
                }
                if !at.ok {
                    return Err(crate::invalid(format!(
                        "{} is not installed, and installing {} did not work ({}).",
                        tool_label(build.tool),
                        tc.name,
                        at.message
                    )));
                }
                details.push(format!("Installed {} first: {}", tc.name, at.message));
            }
            let tool = tool_path(build.tool, cx.home).ok_or_else(|| {
                crate::invalid(format!(
                    "{} is not installed on this computer.",
                    tool_label(build.tool)
                ))
            })?;
            status(format!(
                "{}: building from source with {} (this can take several minutes)",
                a.name,
                tool_label(build.tool)
            ));
            details.extend(build_from_source(repo, build, rev.as_deref(), &tool, cx)?);
            return Ok((details, false));
        }
        Method::SourceLookup { repo, rev } => {
            status(format!(
                "{}: finding how github.com/{repo} is built",
                a.name
            ));
            let build = repo_build(repo, rev.as_deref(), cx.cancel)?;
            let m = Method::Source {
                repo: repo.clone(),
                build,
                rev: rev.clone(),
            };
            return run_method(a, &m, cx, status);
        }
        Method::Custom { argv, admin } => {
            status(format!("{}: running your own install command", a.name));
            let (prog, args) = argv
                .split_first()
                .ok_or_else(|| crate::invalid("The command is empty."))?;
            if *admin && cfg!(windows) && !cx.info.elevated {
                let args: Vec<&str> = args.iter().map(String::as_str).collect();
                restart |= installers::run_as_admin(prog, &args).map_err(crate::invalid)?;
            } else {
                go(admin_cmd(prog, *admin, cx.elev).args(args), cx)?;
            }
            // Nothing tells what an arbitrary command installed.
            details.push("Your command finished successfully; Simple Setup cannot confirm what it installed.".into());
            return Ok((details, restart));
        }
    }
    if !present(m, cx.info) {
        return Err(not_found());
    }
    Ok((details, restart))
}

/// How a GitHub project is built, from the files at the top of its repository (at `rev`, else
/// its default branch): Cargo, Go or a Python package for pipx, as for a checkout.
fn repo_build(repo: &str, rev: Option<&str>, cancel: &AtomicBool) -> crate::Result<Build> {
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
    }
    let at = rev.map(|r| format!("?ref={r}")).unwrap_or_default();
    let top: Vec<Entry> = crate::fetch::json(
        &format!("https://api.github.com/repos/{repo}/contents/{at}"),
        Some("application/vnd.github+json"),
        cancel,
    )?;
    let has = |n: &str| top.iter().any(|e| e.name == n);
    if has("Cargo.toml") {
        return Ok(Build {
            tool: BuildTool::Cargo,
            name: None,
        });
    }
    if has("go.mod") {
        let text = crate::fetch::text(
            &format!(
                "https://raw.githubusercontent.com/{repo}/{}/go.mod",
                rev.unwrap_or("HEAD")
            ),
            None,
            cancel,
        )?;
        let module = text
            .lines()
            .find_map(|l| l.trim().strip_prefix("module "))
            .map(|m| m.trim().trim_matches('"').to_string())
            .filter(|m| crate::manifest::identifier_ok(m))
            .ok_or_else(|| crate::invalid("The project's go.mod names no usable module."))?;
        return Ok(Build {
            tool: BuildTool::Go,
            name: Some(module),
        });
    }
    if has("pyproject.toml") || has("setup.py") {
        return Ok(Build {
            tool: BuildTool::Pipx,
            name: None,
        });
    }
    Err(crate::invalid(format!(
        "github.com/{repo} is not built with Cargo, Go or pipx, which Simple Setup can use."
    )))
}

/// `cargo install --git`, `go install` or `pipx install` at the recorded revision.
fn build_from_source(
    repo: &str,
    build: &Build,
    rev: Option<&str>,
    tool: &Path,
    cx: &Ctx,
) -> crate::Result<Vec<String>> {
    let url = format!("https://github.com/{repo}");
    let local_bin = cx.home.join(".local/bin");
    let commit_like =
        rev.is_some_and(|r| r.len() == 40 && r.bytes().all(|b| b.is_ascii_hexdigit()));
    let mut details = vec![match rev {
        Some(r) => format!("Source: {url} at {r}."),
        None => format!("Source: {url} (default branch; no commit was recorded)."),
    }];
    match build.tool {
        BuildTool::Cargo => {
            let cargo = |locked: bool| {
                let mut c = Command::new(tool);
                c.args(["install", "--git", &url]);
                if let Some(r) = rev {
                    c.args([if commit_like { "--rev" } else { "--tag" }, r]);
                }
                if locked {
                    c.arg("--locked");
                }
                if !cfg!(windows) {
                    c.arg("--root").arg(cx.home.join(".local"));
                }
                if let Some(n) = &build.name {
                    c.arg(n);
                }
                go(&mut c, cx)
            };
            // --locked builds exactly the dependency versions the project records, when it does.
            if let Err(e) = cargo(true) {
                if matches!(e, Error::Cancelled) || !e.to_string().contains("lock") {
                    return Err(e);
                }
                cargo(false)?;
                details.push(
                    "The project records no dependency lock file; current versions were used."
                        .into(),
                );
            }
        }
        BuildTool::Go => {
            let module = build
                .name
                .as_deref()
                .ok_or_else(|| crate::invalid("The Go module name is not known."))?;
            let mut c = Command::new(tool);
            c.args([
                "install",
                &format!("{module}/...@{}", rev.unwrap_or("latest")),
            ]);
            if !cfg!(windows) {
                c.env("GOBIN", &local_bin);
            }
            go(&mut c, cx)?;
        }
        BuildTool::Pipx => {
            let spec = match rev {
                Some(r) => format!("git+{url}.git@{r}"),
                None => format!("git+{url}.git"),
            };
            go(Command::new(tool).args(["install", &spec]), cx)?;
        }
    }
    details.push(if cfg!(windows) {
        "Installed into the build tool's own program folder.".into()
    } else {
        format!("Installed into {}.", local_bin.display())
    });
    Ok(details)
}

fn app_dir(a: &ManifestApp, cx: &Ctx) -> PathBuf {
    download_dir(a, cx.downloads)
}

fn download_dir(a: &ManifestApp, downloads: &Path) -> PathBuf {
    downloads.join(installers::portable_name(
        &a.id.replace([':', '/', '\\'], "-"),
    ))
}

/// An installer for `a` downloaded by an earlier restore is still there (sizes match; the
/// checksum is checked again before it is used), so it installs without internet access.
pub fn downloaded_earlier(a: &ManifestApp, downloads: &Path) -> bool {
    let dir = download_dir(a, downloads);
    fs::read_to_string(dir.join("recipe.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<(Recipe, Vec<String>)>(&t).ok())
        .is_some_and(|(r, _)| {
            r.files().iter().all(|f| {
                crate::paths::join_rel(&dir, &f.stored, true)
                    .is_ok_and(|p| fs::metadata(p).is_ok_and(|m| m.len() == f.size))
            })
        })
}

/// Downloads (or reuses an earlier verified download of) the installer for `a`.
fn download(
    a: &ManifestApp,
    cx: &Ctx,
    status: &mut dyn FnMut(String),
) -> crate::Result<(Recipe, Vec<String>)> {
    let dir = app_dir(a, cx);
    let record = dir.join("recipe.json");
    if let Ok(text) = fs::read_to_string(&record)
        && let Ok((recipe, evidence)) = serde_json::from_str::<(Recipe, Vec<String>)>(&text)
        && crate::manifest::check_recipe(&recipe).is_ok()
        && crate::compat::bundle_fits(&recipe, cx.info).is_ok()
        && recipe.files().iter().all(|f| {
            crate::paths::join_rel(&dir, &f.stored, true).is_ok_and(|p| {
                crate::build::hash_file(&p, cx.cancel, |_| {}).is_ok_and(|(_, h)| h == f.sha256)
            })
        })
    {
        status(format!(
            "{}: using the installer downloaded earlier",
            a.name
        ));
        return Ok((recipe, evidence));
    }
    fs::create_dir_all(&dir)
        .map_err(|e| crate::invalid(format!("Creating {}: {e}", dir.display())))?;
    let mut shared = HashMap::new();
    // Use the actual destination capabilities for vendor package selection.
    let portable = cx.info.clone();
    let mut c = Collect {
        staging: &dir,
        home: cx.home,
        cancel: cx.cancel,
        shared: &mut shared,
        status,
        to: &portable,
        from: None,
        allow_unverified: cx.unverified,
    };
    let (recipe, evidence) = installers::collect_remote(&as_app(a), cx.platform, &mut c)?;
    if let Ok(json) = serde_json::to_string(&(&recipe, &evidence)) {
        let _ = fs::write(&record, json);
    }
    Ok((recipe, evidence))
}

/// Installs system packages with one elevated command per package manager (one password
/// prompt). Packages that are already installed are left alone, and names the package manager
/// does not offer are left out (so one unknown name cannot stop the others). Returns the
/// packages it installed and confirmed; on image-based systems (rpm-ostree,
/// transactional-update), the ones it layered, which appear after a restart.
pub fn batch(
    wanted: &[(Manager, String)],
    cx: &Ctx,
    status: &mut dyn FnMut(String),
) -> HashSet<(Manager, String)> {
    let mut done = HashSet::new();
    let mut managers: Vec<Manager> = wanted.iter().map(|(m, _)| *m).collect();
    managers.dedup();
    let mut seen = HashSet::new();
    managers.retain(|m| seen.insert(*m));
    for mgr in managers {
        let mut pkgs: Vec<&String> = wanted
            .iter()
            .filter(|(m, p)| *m == mgr && plain_name(p))
            .map(|(_, p)| p)
            .collect();
        pkgs.sort();
        pkgs.dedup();
        let need: Vec<&String> = pkgs
            .into_iter()
            .filter(|p| !present(&Method::System(mgr, (*p).clone()), cx.info))
            .collect();
        let need: Vec<&String> = match mgr {
            // apt-get checks availability itself after refreshing its lists (see below).
            Manager::Apt => need,
            _ => need
                .into_iter()
                .filter(|p| available(mgr, p, cx.info) != Some(false))
                .collect(),
        };
        if need.is_empty() {
            continue;
        }
        let names: Vec<OsString> = need.iter().map(OsString::from).collect();
        let joined = need
            .iter()
            .map(|p| p.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let mut cmd = match mgr {
            // One prompt for refreshing the package lists and installing. The script is fixed;
            // package names arrive as its arguments and were checked by `plain_name`.
            Manager::Apt => {
                let mut c = admin_cmd("sh", true, cx.elev);
                c.args([
                    "-c",
                    "apt-get update -q >/dev/null 2>&1; ok=; for p in \"$@\"; do apt-cache show -- \"$p\" >/dev/null 2>&1 && ok=\"$ok $p\"; done; [ -z \"$ok\" ] || DEBIAN_FRONTEND=noninteractive apt-get install -y -q $ok",
                    "simple-setup",
                ]);
                c
            }
            Manager::Dnf => {
                let mut c = admin_cmd("dnf", true, cx.elev);
                c.args(["install", "-y"]);
                c
            }
            Manager::Zypper => {
                let mut c = admin_cmd("zypper", true, cx.elev);
                c.args(["--non-interactive", "install"]);
                c
            }
            Manager::Pacman => {
                let mut c = admin_cmd("pacman", true, cx.elev);
                // Without package lists pacman can install nothing; Arch supports refreshing
                // them only together with a full upgrade (no partial upgrades).
                c.args([
                    if crate::compat::lists_present(mgr) {
                        "-S"
                    } else {
                        "-Syu"
                    },
                    "--needed",
                    "--noconfirm",
                ]);
                c
            }
            Manager::Apk => {
                let mut c = admin_cmd("apk", true, cx.elev);
                // -U: fetch the current package index first (fresh systems have none).
                c.args(["add", "-U"]);
                c
            }
            Manager::RpmOstree => {
                let mut c = admin_cmd("rpm-ostree", true, cx.elev);
                c.args(["install", "--idempotent", "--allow-inactive"]);
                c
            }
            Manager::TransactionalUpdate => {
                let mut c = admin_cmd("transactional-update", true, cx.elev);
                c.args(["--non-interactive", "pkg", "install"]);
                c
            }
            Manager::Custom => {
                let Some(ad) = custom(cx.info) else { continue };
                let mut c = admin_cmd(&ad.install[0], ad.admin, cx.elev);
                let names: Vec<&str> = need.iter().map(|p| p.as_str()).collect();
                c.args(expand(&ad.install[1..], "{packages}", &names));
                status(format!(
                    "Installing {joined} with {} (your trusted commands)",
                    ad.label
                ));
                if let Err(e) = run_waiting(&mut c, cx.cancel, status, LOCK_PAUSE) {
                    status(format!("{} reported: {e}", ad.label));
                }
                done.extend(
                    need.into_iter()
                        .filter(|p| present(&Method::System(mgr, (*p).clone()), cx.info))
                        .map(|p| (mgr, p.clone())),
                );
                continue;
            }
            Manager::None => continue,
        };
        status(format!(
            "Installing {joined} with {} (administrator password needed once)",
            mgr.label()
        ));
        cmd.args(&names);
        let ran = run_waiting(&mut cmd, cx.cancel, status, LOCK_PAUSE);
        if let Err(e) = &ran {
            status(format!("{} reported: {e}", mgr.label()));
        }
        if mgr.needs_restart() {
            if ran.is_ok() {
                done.extend(need.into_iter().map(|p| (mgr, p.clone())));
            }
            continue;
        }
        done.extend(
            need.into_iter()
                .filter(|p| present(&Method::System(mgr, (*p).clone()), cx.info))
                .map(|p| (mgr, p.clone())),
        );
    }
    done
}

/// Whether the package manager offers `p` (read-only query of its package lists); `None` when
/// it cannot be asked.
fn available(mgr: Manager, p: &str, info: &SystemInfo) -> Option<bool> {
    if !crate::compat::lists_present(mgr) {
        return None;
    }
    let out = |prog: &str, args: &[&str]| Command::new(prog).args(args).output().ok();
    match mgr {
        Manager::Apt => out("apt-cache", &["policy", p]).map(|o| {
            let t = String::from_utf8_lossy(&o.stdout);
            t.contains("Candidate:") && !t.contains("Candidate: (none)")
        }),
        Manager::Dnf => out("dnf", &["-q", "repoquery", "--qf", "%{name}\\n", p]).map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .any(|l| l.trim() == p)
        }),
        Manager::Zypper | Manager::TransactionalUpdate => {
            out("zypper", &["--non-interactive", "--no-refresh", "info", p])
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("Information for package"))
        }
        Manager::Pacman => Some(succeeds("pacman", &["-Si", p])),
        Manager::Apk => out("apk", &["search", "-e", p])
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty()),
        Manager::Custom => {
            let c = custom(info)?;
            let argv = c.available.split_first()?;
            Command::new(argv.0)
                .args(expand(argv.1, "{package}", &[p]))
                .output()
                .ok()
                .map(|o| o.status.success())
        }
        Manager::RpmOstree | Manager::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::Arch;

    /// A computer of `os` with exactly these tools (plus curl); on Linux its distribution is
    /// the one whose package manager is among them.
    fn info(os: OsFamily, tools: &[&str]) -> SystemInfo {
        let platform = Platform {
            os,
            arch: Arch::X86_64,
        };
        let distro = tools
            .iter()
            .find_map(|t| match *t {
                "dnf" => Some("fedora"),
                "apt-get" => Some("ubuntu"),
                "zypper" => Some("opensuse-tumbleweed"),
                "pacman" => Some("arch"),
                "apk" => Some("alpine"),
                _ => None,
            })
            .unwrap_or(crate::distros::ROOT);
        let profile = crate::distros::Distros::builtin().resolve(distro).ok();
        let mut i = SystemInfo::assumed(platform, profile);
        i.tools = tools
            .iter()
            .chain(&["curl"])
            .map(|t| t.to_string())
            .collect();
        i
    }

    fn choose(a: &ManifestApp, info: &SystemInfo, source: bool) -> Vec<Method> {
        let prefs = Prefs {
            source,
            ..Prefs::default()
        };
        super::choose(a, &legacy_namespaces(a.source), info, &prefs)
    }

    fn app(source: Source, native: &str, canonical: Option<&str>) -> ManifestApp {
        ManifestApp {
            id: format!("x:{native}"),
            name: native.into(),
            version: None,
            source,
            native_id: native.into(),
            canonical: canonical.map(String::from),
            repository: None,
            github: None,
            confidence: Confidence::Confirmed,
            evidence: Vec::new(),
            installer: true,
            config: false,
            data: false,
            ..ManifestApp::default()
        }
    }

    #[test]
    fn every_os_pair_gets_native_methods() {
        use OsFamily::*;
        // Firefox discovered on each system, restored on each system.
        let sources = [
            (Linux, app(Source::Rpm, "firefox", Some("firefox"))),
            (
                Windows,
                app(Source::Arp, "Mozilla Firefox", Some("firefox")),
            ),
            (Macos, app(Source::BrewCask, "firefox", Some("firefox"))),
        ];
        for (from, a) in &sources {
            // The distribution's own package first, then Flathub.
            let lin = choose(a, &info(Linux, &["flatpak", "dnf"]), true);
            assert_eq!(
                lin[..2],
                [
                    Method::System(Manager::Dnf, "firefox".into()),
                    Method::Flathub("org.mozilla.firefox".into())
                ],
                "{from} → Linux"
            );
            assert!(
                !lin.iter()
                    .any(|m| matches!(m, Method::System(Manager::Apt, _)))
            );
            // Debian family: its own names, never DNF even when dnf is on PATH.
            let deb = choose(a, &info(Linux, &["apt-get", "dnf", "flatpak"]), true);
            assert_eq!(
                deb[0],
                Method::System(Manager::Apt, "firefox".into()),
                "{from} → Ubuntu"
            );
            assert!(
                !deb.iter()
                    .any(|m| matches!(m, Method::System(Manager::Dnf, _)))
            );

            let win = choose(a, &info(Windows, &["winget"]), true);
            // winget first, then the Microsoft Store listing (same winget), then a download.
            assert_eq!(
                win,
                vec![
                    Method::Winget("Mozilla.Firefox".into()),
                    Method::Store("9NZVDKPMR9RD".into()),
                    Method::Download
                ],
                "{from} → Windows"
            );
            // Without winget the catalog download still works.
            assert_eq!(choose(a, &info(Windows, &[]), true), vec![Method::Download]);

            let mac = choose(a, &info(Macos, &["brew"]), true);
            assert_eq!(
                mac[0],
                Method::Brew {
                    name: "firefox".into(),
                    cask: true
                },
                "{from} → macOS"
            );
            assert_eq!(choose(a, &info(Macos, &[]), true), vec![Method::Download]);
        }
    }

    #[test]
    fn unknown_apps_are_not_guessed() {
        let a = app(Source::Arp, "Tiny Tool", None);
        assert!(
            choose(
                &a,
                &info(OsFamily::Linux, &["flatpak", "dnf", "apt-get"]),
                true
            )
            .is_empty()
        );
        assert!(choose(&a, &info(OsFamily::Macos, &["brew"]), true).is_empty());
        // The same app with a GitHub repository can come from its releases.
        let mut gh = a.clone();
        gh.github = Some(GitHub {
            repo: "me/tiny".into(),
            reference: Some("v1.2".into()),
            asset: None,
            confidence: Confidence::Confirmed,
            origin: Origin::Detected,
            commit: None,
            build: None,
        });
        assert_eq!(
            choose(&gh, &info(OsFamily::Linux, &[]), true),
            vec![Method::Download]
        );
        // A command-line tool can also be built from its source as the last way.
        let mut cli = gh.clone();
        cli.source = Source::Manual;
        assert_eq!(
            choose(&cli, &info(OsFamily::Linux, &[]), true),
            vec![
                Method::Download,
                Method::SourceLookup {
                    repo: "me/tiny".into(),
                    rev: Some("v1.2".into())
                }
            ]
        );
        assert_eq!(
            choose(&cli, &info(OsFamily::Linux, &[]), false),
            vec![Method::Download]
        );
        // Odd package names never reach a command line.
        let bad = app(Source::Rpm, "--oops", None);
        assert!(choose(&bad, &info(OsFamily::Linux, &["dnf"]), true).is_empty());
    }

    #[test]
    fn source_checkouts_build_at_their_commit() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let mut a = app(Source::GitClone, "/home/me/src/rg", None);
        a.github = Some(GitHub {
            repo: "BurntSushi/ripgrep".into(),
            reference: Some(format!("master@{}", &sha[..12])),
            asset: None,
            confidence: Confidence::Confirmed,
            origin: Origin::Detected,
            commit: Some(sha.into()),
            build: Some(Build {
                tool: BuildTool::Cargo,
                name: Some("ripgrep".into()),
            }),
        });
        let m = choose(&a, &info(OsFamily::Windows, &["cargo"]), true);
        assert_eq!(m[0], Method::Download, "a release build is preferred");
        assert_eq!(
            m[1],
            Method::Source {
                repo: "BurntSushi/ripgrep".into(),
                build: Build {
                    tool: BuildTool::Cargo,
                    name: Some("ripgrep".into())
                },
                rev: Some(sha.into()),
            }
        );
        assert!(
            m[1].label(OsFamily::Windows)
                .contains("ripgrep at 0123456789ab from source with Cargo")
        );
        assert_eq!(
            choose(&a, &info(OsFamily::Windows, &[]), false),
            vec![Method::Download]
        );
    }

    #[test]
    fn store_apps_and_toolchains() {
        let mut store = app(Source::Msix, "Microsoft.Todos", None);
        store.repository = Some("Microsoft Store".into());
        assert!(choose(&store, &info(OsFamily::Windows, &["winget"]), true).is_empty());
        let rust = choose_toolchain(BuildTool::Cargo, &info(OsFamily::Linux, &["dnf"]));
        assert_eq!(rust, vec![Method::System(Manager::Dnf, "cargo".into())]);
        assert!(Manager::RpmOstree.needs_restart() && !Manager::Dnf.needs_restart());
        let go = choose_toolchain(BuildTool::Go, &info(OsFamily::Windows, &["winget"]));
        assert_eq!(go[0], Method::Winget("GoLang.Go".into()));
        assert!(Method::System(Manager::Dnf, "x".into()).admin());
        assert!(!Method::Flathub("x".into()).admin());
    }

    #[test]
    fn package_names_follow_the_destination_family() {
        // An RPM from Fedora on openSUSE: Fedora's name is not reused (only the registry's
        // openSUSE names would be), and Flathub covers it.
        let ff = app(Source::Rpm, "firefox", Some("firefox"));
        let suse = choose(&ff, &info(OsFamily::Linux, &["zypper", "flatpak"]), true);
        assert!(
            !suse.contains(&Method::System(Manager::Zypper, "firefox".into())),
            "{suse:?}"
        );
        assert!(suse.contains(&Method::Flathub("org.mozilla.firefox".into())));
        // An unknown RPM from Fedora is still tried by name on another Fedora-family system.
        let tool = app(Source::Rpm, "fedora-only-tool", None);
        assert_eq!(
            choose(&tool, &info(OsFamily::Linux, &["dnf"]), true),
            vec![Method::System(Manager::Dnf, "fedora-only-tool".into())]
        );
        // …but never on Debian, Arch or Alpine.
        for t in ["apt-get", "pacman", "apk"] {
            assert!(
                choose(&tool, &info(OsFamily::Linux, &[t]), true).is_empty(),
                "{t}"
            );
        }
        // Alpine (musl) gets no downloaded Linux programs, and an AppImage is not built.
        let mut gh = app(Source::AppImage, "Tool", None);
        gh.github = Some(GitHub {
            repo: "me/tool".into(),
            reference: None,
            asset: None,
            confidence: Confidence::Confirmed,
            origin: Origin::Detected,
            commit: None,
            build: None,
        });
        assert!(choose(&gh, &info(OsFamily::Linux, &["apk"]), true).is_empty());
        assert_eq!(
            choose(&gh, &info(OsFamily::Linux, &["apt-get"]), false),
            vec![Method::Download]
        );
    }

    /// Regression guard: a package manager whose lock is held is waited for, not failed.
    #[cfg(unix)]
    #[test]
    fn waits_for_a_held_lock() {
        let t = tempfile::tempdir().unwrap();
        let flag = t.path().join("second");
        let script = format!(
            "if [ -f {f} ]; then echo installed; else touch {f}; echo 'E: Could not get lock /var/lib/dpkg/lock-frontend' >&2; exit 100; fi",
            f = flag.display()
        );
        let mut c = Command::new("sh");
        c.args(["-c", &script]);
        let mut notes = Vec::new();
        let out = run_waiting(
            &mut c,
            &AtomicBool::new(false),
            &mut |n| notes.push(n),
            Duration::from_millis(10),
        )
        .unwrap();
        assert_eq!(out.trim(), "installed");
        assert_eq!(notes.len(), 1, "{notes:?}");
        // Other failures are not retried.
        let mut c = Command::new("sh");
        c.args([
            "-c",
            "echo 'E: Unable to locate package nope' >&2; exit 100",
        ]);
        assert!(
            run_waiting(
                &mut c,
                &AtomicBool::new(false),
                &mut |_| {},
                Duration::from_millis(10)
            )
            .is_err()
        );
        assert!(lock_held(
            "error: failed to init transaction (unable to lock database)"
        ));
        assert!(lock_held(
            "System management is locked by the application with pid 1234 (zypper)."
        ));
        assert!(!lock_held("No match for argument: nope"));
    }

    /// How real projects are built, read from their repositories. Run with `--ignored live`.
    #[test]
    #[ignore]
    fn live_repository_build_tools() {
        let stop = AtomicBool::new(false);
        assert_eq!(
            repo_build("BurntSushi/ripgrep", None, &stop).unwrap().tool,
            BuildTool::Cargo
        );
        let fzf = repo_build("junegunn/fzf", None, &stop).unwrap();
        assert_eq!(fzf.tool, BuildTool::Go);
        assert_eq!(fzf.name.as_deref(), Some("github.com/junegunn/fzf"));
        assert_eq!(
            repo_build("pypa/pipx", None, &stop).unwrap().tool,
            BuildTool::Pipx
        );
        assert!(repo_build("fleetwoodjohnr/simple-cell", None, &stop).is_err());
    }
}
