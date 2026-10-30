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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Manager {
    Apt,
    Dnf,
    Zypper,
    Pacman,
}

impl Manager {
    pub const ALL: [Self; 4] = [Self::Apt, Self::Dnf, Self::Zypper, Self::Pacman];

    fn tool(self) -> &'static str {
        match self {
            Self::Apt => "apt-get",
            Self::Dnf => "dnf",
            Self::Zypper => "zypper",
            Self::Pacman => "pacman",
        }
    }

    /// Registry key and discovery source of the package names this manager installs.
    fn key(self) -> (&'static str, Source) {
        match self {
            Self::Apt => ("dpkg", Source::Dpkg),
            Self::Dnf | Self::Zypper => ("rpm", Source::Rpm),
            Self::Pacman => ("pacman", Source::Pacman),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Apt => "APT",
            Self::Dnf => "DNF",
            Self::Zypper => "Zypper",
            Self::Pacman => "Pacman",
        }
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
    /// The installer a catalog lists (winget manifest, Homebrew cask) or the app's GitHub
    /// release offers, downloaded and checked against its published SHA-256.
    Download,
    /// Built from the project's source code at the recorded commit or tag.
    Source {
        repo: String,
        build: Build,
        rev: Option<String>,
    },
}

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
        }
    }

    /// Asks for an administrator password (unless already running as administrator).
    pub fn admin(&self) -> bool {
        matches!(self, Self::System(..) | Self::Snap(_))
    }

    pub fn channel(&self) -> Channel {
        match self {
            Self::Source { .. } => Channel::Source,
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
    }
}

/// The ways this computer can install `a`, best first. Empty if there is none.
pub fn choose(a: &ManifestApp, info: &SystemInfo, allow_source: bool) -> Vec<Method> {
    let app = as_app(a);
    let entry = a.canonical.as_deref().and_then(registry::get);
    let ids = |key: &str| -> Vec<String> {
        entry
            .and_then(|e| e.packages.get(key))
            .cloned()
            .unwrap_or_default()
    };
    // The app's own package name first (same packaging on another distribution), then the
    // registry's names for this manager.
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
    // Downloads go through curl; on Linux it is installed first when missing (see `needs`).
    let can_download = info.has("curl")
        || (os == OsFamily::Linux && Manager::ALL.iter().any(|m| info.has(m.tool())));
    let store = matches!(a.source, Source::Msix | Source::AppBundle) && a.repository.is_some();
    match os {
        OsFamily::Linux => {
            let own_system = Manager::ALL
                .into_iter()
                .filter(|m| info.has(m.tool()) && a.source == m.key().1 && plain_name(&a.native_id))
                .map(|m| Method::System(m, a.native_id.clone()));
            let flatpaks = named("flatpak", Source::Flatpak)
                .into_iter()
                .map(Method::Flathub);
            let systems = Manager::ALL
                .into_iter()
                .filter(|m| info.has(m.tool()))
                .flat_map(|m| {
                    named(m.key().0, m.key().1)
                        .into_iter()
                        .map(move |p| Method::System(m, p))
                });
            let snaps = named("snap", Source::Snap).into_iter().map(Method::Snap);
            if a.source == Source::Flatpak && info.has("flatpak") {
                out.extend(flatpaks.clone().take(1));
            }
            out.extend(own_system);
            if info.has("flatpak") {
                out.extend(flatpaks);
            }
            out.extend(systems);
            if info.has("snap") {
                out.extend(snaps);
            }
            if github_repo(&app).is_some() && can_download {
                out.push(Method::Download);
            }
        }
        OsFamily::Windows if store && a.source == Source::Msix => {}
        OsFamily::Windows => {
            let winget = winget_id(&app).filter(|w| plain_name(w));
            if let Some(w) = &winget
                && info.has("winget")
            {
                out.push(Method::Winget(w.clone()));
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
    if allow_source
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
    let first = Manager::ALL.into_iter().find(|m| info.has(m.tool()));
    match first {
        Some(m) if download && !info.has("curl") && info.device.platform.os == OsFamily::Linux => {
            vec![(m, "curl".to_string())]
        }
        _ => Vec::new(),
    }
}

/// Methods for a stand-in toolchain app: never built from source.
pub fn choose_toolchain(t: BuildTool, info: &SystemInfo) -> Vec<Method> {
    choose(&toolchain_app(t), info, false)
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
pub fn present(m: &Method) -> bool {
    match m {
        Method::Flathub(id) => succeeds("flatpak", &["info", id]),
        Method::System(Manager::Apt, p) => Command::new("dpkg-query")
            .args(["-W", "-f=${Status}", p])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("install ok installed")),
        Method::System(Manager::Dnf | Manager::Zypper, p) => succeeds("rpm", &["-q", p]),
        Method::System(Manager::Pacman, p) => succeeds("pacman", &["-Q", p]),
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
        Method::Download | Method::Source { .. } => false,
    }
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
        if present(m) {
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
    run(cmd, cx.cancel)
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
        Method::Winget(id) => {
            let (code, _, why) = run_status(
                Command::new("winget").args([
                    "install",
                    "--id",
                    id,
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
            batch(&[(*mgr, p.clone())], cx, status);
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
            match installers::check(&recipe, cx.info.distro.as_deref(), cx.home) {
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
    }
    if !present(m) {
        return Err(not_found());
    }
    Ok((details, restart))
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
    let mut c = Collect {
        staging: &dir,
        home: cx.home,
        cancel: cx.cancel,
        shared: &mut shared,
        status,
    };
    let (recipe, evidence) = installers::collect_remote(&as_app(a), cx.platform, &mut c)?;
    if let Ok(json) = serde_json::to_string(&(&recipe, &evidence)) {
        let _ = fs::write(&record, json);
    }
    Ok((recipe, evidence))
}

/// Installs system packages with one administrator prompt per package manager: packages the
/// manager does not offer are left out (so one unknown name cannot stop the others). Returns
/// the packages it installed and confirmed; packages that were already there are not touched
/// or returned.
pub fn batch(
    wanted: &[(Manager, String)],
    cx: &Ctx,
    status: &mut dyn FnMut(String),
) -> HashSet<(Manager, String)> {
    let mut done = HashSet::new();
    for mgr in Manager::ALL {
        let mut pkgs: Vec<&String> = wanted
            .iter()
            .filter(|(m, p)| *m == mgr && plain_name(p))
            .map(|(_, p)| p)
            .collect();
        pkgs.sort();
        pkgs.dedup();
        let need: Vec<&String> = pkgs
            .into_iter()
            .filter(|p| !present(&Method::System(mgr, (*p).clone())))
            .collect();
        let need: Vec<&String> = match mgr {
            // apt-get checks availability itself after refreshing its lists (see below).
            Manager::Apt => need,
            _ => need.into_iter().filter(|p| available(mgr, p)).collect(),
        };
        if need.is_empty() {
            continue;
        }
        status(format!(
            "Installing {} with {} (administrator password needed once)",
            need.iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            mgr.label()
        ));
        let names: Vec<OsString> = need.iter().map(OsString::from).collect();
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
                c.args(["-S", "--needed", "--noconfirm"]);
                c
            }
        };
        cmd.args(&names);
        if let Err(e) = run(&mut cmd, cx.cancel) {
            status(format!("{} reported: {e}", mgr.label()));
        }
        done.extend(
            need.into_iter()
                .filter(|p| present(&Method::System(mgr, (*p).clone())))
                .map(|p| (mgr, p.clone())),
        );
    }
    done
}

/// The package manager offers `p` (read-only query of its package lists).
fn available(mgr: Manager, p: &str) -> bool {
    match mgr {
        Manager::Apt => Command::new("apt-cache")
            .args(["policy", p])
            .output()
            .is_ok_and(|o| {
                let t = String::from_utf8_lossy(&o.stdout);
                t.contains("Candidate:") && !t.contains("Candidate: (none)")
            }),
        Manager::Dnf => Command::new("dnf")
            .args(["-q", "repoquery", "--qf", "%{name}\\n", p])
            .output()
            .is_ok_and(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .any(|l| l.trim() == p)
            }),
        Manager::Zypper => Command::new("zypper")
            .args(["--non-interactive", "--no-refresh", "info", p])
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("Information for package")),
        Manager::Pacman => succeeds("pacman", &["-Si", p]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Arch, Device};

    fn info(os: OsFamily, tools: &[&str]) -> SystemInfo {
        SystemInfo {
            device: Device {
                hostname: "new".into(),
                os_name: "test".into(),
                platform: Platform {
                    os,
                    arch: Arch::X86_64,
                },
            },
            distro: None,
            cpu: String::new(),
            cores: 1,
            memory_bytes: 0,
            free_bytes: None,
            tools: tools
                .iter()
                .chain(&["curl"])
                .map(|t| t.to_string())
                .collect(),
            flathub: false,
            elevated: false,
            online: true,
        }
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
            let lin = choose(a, &info(Linux, &["flatpak", "dnf"]), true);
            assert_eq!(
                lin[0],
                if *from == Linux {
                    Method::System(Manager::Dnf, "firefox".into())
                } else {
                    Method::Flathub("org.mozilla.firefox".into())
                },
                "{from} → Linux"
            );
            assert!(lin.contains(&Method::System(Manager::Dnf, "firefox".into())));
            assert!(
                !lin.iter()
                    .any(|m| matches!(m, Method::System(Manager::Apt, _)))
            );

            let win = choose(a, &info(Windows, &["winget"]), true);
            assert_eq!(
                win,
                vec![Method::Winget("Mozilla.Firefox".into()), Method::Download],
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
        let go = choose_toolchain(BuildTool::Go, &info(OsFamily::Windows, &["winget"]));
        assert_eq!(go[0], Method::Winget("GoLang.Go".into()));
        assert!(Method::System(Manager::Dnf, "x".into()).admin());
        assert!(!Method::Flathub("x".into()).admin());
    }
}
