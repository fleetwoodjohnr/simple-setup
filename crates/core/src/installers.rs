//! Offline installers. While a snapshot is built, each selected app's installer is collected
//! into it: Flatpak bundles are made from this computer's Flatpak repository, RPM dependency
//! sets and snaps are downloaded from their usual sources, and self-contained program files are
//! copied. On the new computer they are installed from the snapshot alone, with fixed argument
//! lists per recipe type; nothing in the manifest is ever run as a command.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::build::{CopyError, copy_hashed, hash_file};
use crate::catalog::{self, Pick};
use crate::discovery::linux::parse_human_size;
use crate::discovery::{App, Confidence, Source, git, source_key};
use crate::fetch;
use crate::manifest::{
    FileEntry, MacContainer, Package, Recipe, SnapFile, StoredFile, WinInstaller, plain_name,
};
use crate::paths::{join_rel, portable_component, sanitize_component, tilde, untilde};
use crate::platform::{Arch, OsFamily, Platform, os_label};
use crate::probe::SystemInfo;
use crate::registry;
use crate::restore::{ensure_dirs, restore_file};
use crate::{Error, IoContext, Result, invalid};

static NEVER: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------- running tools

fn drain<R: Read + Send + 'static>(r: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut r) = r {
            let _ = r.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

fn last_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join(" / ")
}

/// Runs a tool to completion (killing it if `cancel` is set) and returns its output.
pub(crate) fn run(cmd: &mut Command, cancel: &AtomicBool) -> Result<String> {
    let (code, out, why) = run_status(cmd, cancel)?;
    match code {
        Some(0) => Ok(out),
        _ => Err(invalid(why)),
    }
}

/// Runs a tool to completion and returns its exit code (`None` if killed by a signal), its
/// output, and a one-line reason for a failure (the tool's last lines).
pub(crate) fn run_status(
    cmd: &mut Command,
    cancel: &AtomicBool,
) -> Result<(Option<i32>, String, String)> {
    let name = cmd.get_program().to_string_lossy().into_owned();
    let started = std::time::Instant::now();
    let mut child = cmd
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| invalid(format!("Could not start {name}: {e}")))?;
    let (out, err) = (drain(child.stdout.take()), drain(child.stderr.take()));
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            crate::diag::command(cmd, None, started.elapsed(), "cancelled");
            return Err(Error::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(e).ctx(|| format!("Waiting for {name}")),
        }
    };
    let (out, err) = (
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    );
    let why = last_lines(if err.trim().is_empty() { &out } else { &err }, 3);
    crate::diag::command(cmd, status.code(), started.elapsed(), &why);
    Ok((status.code(), out, format!("{name} failed: {why}")))
}

pub(crate) fn succeeds(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn which(prog: &str) -> Option<PathBuf> {
    // Windows programs end in .exe; winget is an app alias, which only exists as a link.
    // Scoop installs script shims (`scoop.cmd`).
    let names: &[String] = &if cfg!(windows) {
        vec![
            format!("{prog}.exe"),
            format!("{prog}.cmd"),
            prog.to_string(),
        ]
    } else {
        vec![prog.to_string()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .find(|p| p.is_file() || (cfg!(windows) && fs::symlink_metadata(p).is_ok()))
}

// ---------------------------------------------------------------- what can be bundled

fn is_elf(p: &Path) -> bool {
    let mut head = [0u8; 4];
    File::open(p)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head == b"\x7fELF"
}

fn at_ref(reference: Option<&str>) -> String {
    reference.map(|r| format!(" at {r}")).unwrap_or_default()
}

/// A build's view of where an installer is going: from this computer (its platform and, on
/// Linux, its distribution profile) to the computer a target stands for (see
/// [`crate::probe::SystemInfo::assumed`]).
#[derive(Clone, Copy)]
pub struct Route<'a> {
    pub source: Platform,
    pub from: Option<&'a crate::distros::Resolved>,
    pub to: &'a SystemInfo,
}

impl Route<'_> {
    pub fn target(&self) -> Platform {
        self.to.device.platform
    }

    /// Downloaded Linux programs can run there (not on musl systems such as Alpine).
    fn portable(&self) -> bool {
        self.to
            .profile()
            .is_none_or(|p| p.appimage != crate::distros::Avail::Unsupported)
    }
}

/// Whether this computer's own copy of `app` can be bundled for the destination: same system
/// and processor, and for distribution packages the same distribution profile (they are built
/// against it). Flatpak bundles, snaps and program files are not tied to a distribution.
fn local_fits(app: &App, r: &Route) -> bool {
    r.target() == r.source
        && (!matches!(
            app.source,
            Source::Rpm | Source::Dpkg | Source::Pacman | Source::Apk
        ) || match (r.from, r.to.profile()) {
            (Some(f), Some(t)) => {
                f.id == t.id
                    && !r.to.linux.as_ref().is_some_and(|l| l.immutable)
                    && r.to
                        .distro
                        .as_ref()
                        .is_none_or(|d| Some(d.clone()) == crate::platform::distro())
            }
            // This computer's distribution is not known: nothing says its packages fit.
            _ => false,
        })
}

/// What would be bundled for `app` on the destination when the snapshot is built here, and the
/// Linux release it is limited to (RPM packages install only on the one they came from); or
/// why nothing can be (and what to do instead). Reads only metadata; never uses the network.
pub fn plan(
    app: &App,
    home: &Path,
    r: &Route,
) -> std::result::Result<(String, Option<String>), String> {
    let (source, target) = (r.source, r.target());
    if let Some(d) = app.downloads.iter().find(|d| d.target == target) {
        let host = d
            .url
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or_default();
        return Ok((
            format!(
                "the download you registered (from {host}), checked against the SHA-256 you entered"
            ),
            None,
        ));
    }
    if local_fits(app, r) {
        return match local_plan(app, home) {
            Ok(what) => Ok((
                what,
                matches!(
                    app.source,
                    Source::Rpm | Source::Dpkg | Source::Pacman | Source::Apk
                )
                .then(crate::platform::distro)
                .flatten(),
            )),
            Err(why) => remote_plan(app, r)
                .map(|what| (what, None))
                .map_err(|_| why),
        };
    }
    if target == source && app.source == Source::Rpm {
        let there = r.to.profile().map_or("the new computer".to_string(), |p| {
            if p.id == crate::distros::ROOT {
                "other Linux distributions".to_string()
            } else {
                p.name.clone()
            }
        });
        return remote_plan(app, r).map(|what| (what, None)).map_err(|_| {
            format!(
                "{} is installed here as an RPM package, which does not install on {there}; RPM packages are never converted.",
                app.name
            )
        });
    }
    remote_plan(app, r).map(|what| (what, None))
}

/// The Linux distribution a recipe installs on, when it is tied to one.
pub fn only_on(r: &Recipe) -> Option<&str> {
    match r {
        Recipe::Rpm { distro, .. } | Recipe::NativeSet { distro, .. } => Some(distro),
        _ => None,
    }
}

/// What can be collected from this computer itself (Linux packages, program files).
fn local_plan(app: &App, home: &Path) -> std::result::Result<String, String> {
    let name = &app.name;
    match app.source {
        Source::Flatpak => Ok(format!(
            "a Flatpak bundle of {} and its runtime, made from this computer's Flatpak repository",
            app.native_id
        )),
        Source::Rpm => match app.repository.as_deref() {
            Some(r) if r.starts_with('@') || r == "<unknown>" => Err(format!(
                "{name} was installed from a local .rpm file, not from a repository, so it cannot be collected again. Keep that file and install it by hand."
            )),
            r => Ok(format!(
                "RPM package {} and all its dependencies, downloaded from {} during the build",
                app.native_id,
                r.unwrap_or("its repository")
            )),
        },
        Source::Snap => Ok(format!(
            "snap {} and its base, downloaded from the Snap Store during the build",
            app.native_id
        )),
        Source::AppImage | Source::Custom => {
            let Some(p) = app.path.as_deref().filter(|p| p.is_file()) else {
                return Err(format!("The program file for {name} was not found."));
            };
            if !p.starts_with(home) {
                return Err(format!(
                    "{name} is installed outside your home folder ({}); reinstall it by hand.",
                    p.display()
                ));
            }
            if app.source == Source::Custom {
                if let Some(co) = git::checkout_containing(p, home) {
                    return Err(match co.github(Confidence::Probable) {
                        Some(gh) => format!(
                            "{name} runs from a source checkout; rebuild it from github.com/{}{}.",
                            gh.repo,
                            at_ref(gh.reference.as_deref())
                        ),
                        None => format!(
                            "{name} runs from a source checkout ({}); rebuild it by hand.",
                            tilde(&co.dir, home)
                        ),
                    });
                }
                if !is_elf(p) {
                    return Err(format!(
                        "{name} is a script that needs other files; reinstall it by hand."
                    ));
                }
            }
            Ok(format!("the program file itself ({})", tilde(p, home)))
        }
        Source::GitClone => Err(match &app.github {
            Some(gh) => format!(
                "Rebuild {name} from source: clone github.com/{}{}. Select its data to copy the checkout itself.",
                gh.repo,
                at_ref(gh.reference.as_deref())
            ),
            None => format!(
                "Rebuild {name} from source by hand. Select its data to copy the checkout itself."
            ),
        }),
        Source::Dpkg | Source::Pacman | Source::Apk => Ok(format!(
            "native package {} and its complete dependency set, restricted to this distribution and release",
            app.native_id
        )),
        Source::Manual => Err(format!(
            "{name} was added by hand and has no known installer."
        )),
        Source::Arp
        | Source::Msix
        | Source::Scoop
        | Source::Choco
        | Source::Brew
        | Source::BrewCask
        | Source::AppBundle
        | Source::Pkg
        | Source::Macports
        | Source::Cargo
        | Source::Pipx
        | Source::Npm
        | Source::Native => Err(format!(
            "No offline installer is known for {name}. On the new computer, {}.",
            reinstall(app)
        )),
    }
}

pub(crate) fn winget_id(app: &App) -> Option<String> {
    let e = app.canonical.as_deref().and_then(registry::get)?;
    e.packages.get("winget")?.first().cloned()
}

pub(crate) fn cask_token(app: &App) -> Option<String> {
    if app.source == Source::BrewCask {
        return Some(app.native_id.clone());
    }
    let e = app.canonical.as_deref().and_then(registry::get)?;
    e.packages.get("brew_cask")?.first().cloned()
}

/// `owner/name` with only the characters GitHub allows.
pub(crate) fn github_repo(app: &App) -> Option<&str> {
    let r = app.github.as_ref().map(|g| g.repo.as_str()).or_else(|| {
        app.canonical
            .as_deref()
            .and_then(registry::get)
            .and_then(|e| e.github.as_deref())
    })?;
    let (o, n) = r.split_once('/')?;
    [o, n]
        .iter()
        .all(|p| {
            !p.is_empty()
                && !p.starts_with('.')
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
        .then_some(r)
}

/// Installers downloaded from a vetted catalog during the build: winget for Windows, Homebrew
/// casks for macOS, the app's GitHub releases otherwise.
fn remote_plan(app: &App, r: &Route) -> std::result::Result<String, String> {
    let (source, target) = (r.source, r.target());
    let deb = crate::compat::wants_deb(r.to);
    let name = &app.name;
    if target.os == OsFamily::Linux && !r.portable() {
        let there = r.to.profile().map_or("that system", |p| p.name.as_str());
        return Err(format!(
            "Downloaded Linux programs and AppImages do not run on {there}; the restore installs {name} with its own package manager or Flatpak where one is known."
        ));
    }
    let store = matches!(app.source, Source::Msix | Source::AppBundle) && app.repository.is_some();
    if store && registry::method_os(source_key(app.source)) == Some(target.os) {
        return Err(format!(
            "{name} came from the {}; install it from there on the new computer (it is tied to your account).",
            app.repository.as_deref().unwrap_or_default()
        ));
    }
    let github = |what: &str| {
        github_repo(app)
            .map(|r| format!("{what} from a release of github.com/{r}, checked by its SHA-256"))
    };
    let found = match target.os {
        OsFamily::Windows => winget_id(app)
            .map(|id| format!("the Windows installer listed in the winget catalog as {id}, checked against its published SHA-256"))
            .or_else(|| github("a Windows installer")),
        OsFamily::Macos => cask_token(app)
            .map(|t| format!("the macOS download of Homebrew cask {t}, checked by its SHA-256"))
            .or_else(|| github("a macOS disk image, app archive or program")),
        OsFamily::Linux if wants_rpm(r.to) => github("an official .rpm package, AppImage or program archive"),
        OsFamily::Linux if deb => github("a .deb package, AppImage or program archive"),
        OsFamily::Linux => github("an AppImage or program archive")
            .map(|s| s + "; it runs on any Linux distribution"),
    };
    found.ok_or_else(|| {
        if target.os == source.os && source.os != OsFamily::Linux {
            format!(
                "No offline installer is known for {name}. On the new computer, {}.",
                reinstall(app)
            )
        } else if target.os == source.os {
            let how = match app.source {
                Source::Flatpak => format!(
                    "Flatpak {} from {}",
                    app.native_id,
                    app.repository.as_deref().unwrap_or("its remote")
                ),
                Source::Rpm | Source::Dpkg | Source::Pacman | Source::Apk => {
                    format!(
                        "{} package {}",
                        source_key(app.source).to_uppercase(),
                        app.native_id
                    )
                }
                Source::Snap => format!("snap {}", app.native_id),
                _ => "its original installer".into(),
            };
            format!(
                "Installers are bundled for {source} only. On {target}, install {name} from {how}."
            )
        } else {
            format!(
                "No downloadable {} installer is known for {name}.",
                target.os
            )
        }
    })
}

/// How to reinstall a Windows or macOS app by hand.
fn reinstall(app: &App) -> String {
    let id = &app.native_id;
    let winget = app
        .canonical
        .as_deref()
        .and_then(crate::registry::get)
        .and_then(|e| e.packages.get("winget")?.first());
    match (app.source, &app.repository, winget) {
        (Source::Scoop, ..) => format!("run  scoop install {id}"),
        (Source::Choco, ..) => format!("run  choco install {id}"),
        (Source::Brew, ..) => format!("run  brew install {id}"),
        (Source::BrewCask, ..) => format!("run  brew install --cask {id}"),
        (Source::Macports, ..) => format!("run  sudo port install {id}"),
        (Source::Msix | Source::AppBundle, Some(store), _) => {
            format!("install {} from the {store}", app.name)
        }
        (Source::Arp | Source::Msix, _, Some(w)) => format!("run  winget install --id {w}"),
        _ => match &app.github {
            Some(gh) => format!("download {} from github.com/{}", app.name, gh.repo),
            None => format!("run the original installer for {} again", app.name),
        },
    }
}

// ---------------------------------------------------------------- Flatpak

/// Installed runtimes (`name/arch/branch`) that a runtime's or app's metadata declares as
/// extensions — GL drivers, codecs — except debug, locale and source extensions.
pub fn extension_refs(meta: &str, arch: &str, branch: &str, installed: &[String]) -> Vec<String> {
    struct Point {
        name: String,
        version: Option<String>,
        versions: Option<Vec<String>>,
        subdirs: bool,
    }
    let skip = |n: &str| {
        n.split('.')
            .any(|c| matches!(c, "Debug" | "Locale" | "Sources"))
    };
    let mut points: Vec<Point> = Vec::new();
    let mut in_ext = false;
    for line in meta.lines().map(str::trim) {
        if line.starts_with('[') {
            let name = line
                .trim_matches(['[', ']'])
                .strip_prefix("Extension ")
                .map(str::trim);
            in_ext = name.is_some();
            if let Some(n) = name {
                points.push(Point {
                    name: n.into(),
                    version: None,
                    versions: None,
                    subdirs: false,
                });
            }
        } else if in_ext && let (Some(p), Some((k, v))) = (points.last_mut(), line.split_once('='))
        {
            let v = v.trim();
            match k.trim() {
                "version" => p.version = Some(v.into()),
                "versions" => {
                    p.versions = Some(
                        v.split(';')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(String::from)
                            .collect(),
                    );
                }
                "subdirectories" => p.subdirs = v == "true",
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    for p in points.iter().filter(|p| !skip(&p.name)) {
        let versions = p
            .versions
            .clone()
            .unwrap_or_else(|| vec![p.version.clone().unwrap_or_else(|| branch.into())]);
        for r in installed {
            let mut it = r.splitn(3, '/');
            let (Some(n), Some(a), Some(b)) = (it.next(), it.next(), it.next()) else {
                continue;
            };
            let named = n == p.name
                || p.subdirs
                    && n.strip_prefix(p.name.as_str())
                        .is_some_and(|rest| rest.starts_with('.'));
            if named && a == arch && !skip(n) && versions.iter().any(|v| v == b) && !out.contains(r)
            {
                out.push(r.clone());
            }
        }
    }
    out
}

fn flatpak(args: &[&str], cancel: &AtomicBool) -> Result<String> {
    run(Command::new("flatpak").args(args), cancel).map(|s| s.trim().to_string())
}

/// The refs to bundle for a Flatpak app, in install order: runtime, extensions, app.
fn flatpak_refs(id: &str, cancel: &AtomicBool) -> Result<Vec<String>> {
    let app_ref = flatpak(&["info", "--show-ref", id], cancel)?;
    let runtime = format!(
        "runtime/{}",
        flatpak(&["info", "--show-runtime", id], cancel)?
    );
    let installed: Vec<String> = flatpak(&["list", "--runtime", "--columns=ref"], cancel)?
        .lines()
        .map(|l| l.trim().to_string())
        .collect();
    let mut refs = vec![runtime.clone()];
    for parent in [&runtime, &app_ref] {
        let meta = flatpak(&["info", "-m", parent], cancel)?;
        let p: Vec<&str> = parent.split('/').collect();
        let (arch, branch) = (
            p.get(2).copied().unwrap_or(""),
            p.get(3).copied().unwrap_or(""),
        );
        for e in extension_refs(&meta, arch, branch, &installed) {
            let r = format!("runtime/{e}");
            if !refs.contains(&r) {
                refs.push(r);
            }
        }
    }
    refs.push(app_ref);
    Ok(refs)
}

/// Rough size of the bundles for these Flatpak apps (installed size of each distinct ref,
/// halved for compression). Used only for the free-space estimate.
pub fn flatpak_estimate(ids: &[&str]) -> u64 {
    let Ok(list) = flatpak(&["list", "--columns=ref,size"], &NEVER) else {
        return 0;
    };
    let sizes: HashMap<&str, u64> = list
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .filter_map(|(r, s)| Some((r.trim(), parse_human_size(s)?)))
        .collect();
    let mut seen = HashSet::new();
    let mut total = 0;
    for id in ids {
        for r in flatpak_refs(id, &NEVER).unwrap_or_default() {
            let short = r.split_once('/').map_or(r.as_str(), |(_, s)| s).to_string();
            if seen.insert(short.clone()) {
                total += sizes.get(short.as_str()).copied().unwrap_or(0) / 2;
            }
        }
    }
    total
}

// ---------------------------------------------------------------- collecting

/// Installer files collected so far in this build, by Flatpak ref (shared runtimes).
pub type Shared = HashMap<String, StoredFile>;

pub struct Collect<'a> {
    pub staging: &'a Path,
    pub home: &'a Path,
    pub cancel: &'a AtomicBool,
    pub shared: &'a mut Shared,
    pub status: &'a mut dyn FnMut(String),
    /// The computer the installers are for (see [`Route`]).
    pub to: &'a SystemInfo,
    /// This computer's distribution profile.
    pub from: Option<&'a crate::distros::Resolved>,
    /// Downloads whose source publishes no checksum may be used (their own SHA-256 is then
    /// pinned). Building does so and says so in the evidence; a restore only when approved.
    pub allow_unverified: bool,
}

fn local(staging: &Path, rel: &str) -> PathBuf {
    staging.join(rel.split('/').collect::<PathBuf>())
}

pub(crate) fn portable_name(s: &str) -> String {
    if portable_component(s) {
        s.into()
    } else {
        sanitize_component(s)
    }
}

fn app_dir(app: &App) -> String {
    format!(
        "installers/{}",
        portable_name(&format!("{}-{}", source_key(app.source), app.name))
    )
}

pub(crate) fn stored(c: &Collect, rel: String) -> Result<StoredFile> {
    let (size, sha256) = hash_file(&local(c.staging, &rel), c.cancel, |_| {})?;
    Ok(StoredFile {
        stored: rel,
        size,
        sha256,
    })
}

pub(crate) fn fresh_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        fs::remove_dir_all(dir).ctx(|| format!("Clearing {}", dir.display()))?;
    }
    fs::create_dir_all(dir).ctx(|| format!("Creating {}", dir.display()))
}

/// Collects `app`'s installer for `target` into `installers/` inside the staging folder: from
/// this computer when it is the same platform and that works, otherwise from a catalog download.
/// Errors other than cancellation mean "no installer"; the build continues without it.
pub fn collect(
    app: &App,
    source: Platform,
    target: Platform,
    c: &mut Collect,
) -> Result<(Recipe, Vec<String>)> {
    let registered = app.downloads.iter().any(|d| d.target == target);
    let route = Route {
        source,
        from: c.from,
        to: c.to,
    };
    if registered || !local_fits(app, &route) || local_plan(app, c.home).is_err() {
        return collect_remote(app, target, c);
    }
    match collect_local(app, c) {
        Err(e) if !matches!(e, Error::Cancelled) && remote_plan(app, &route).is_ok() => {
            // e.g. a package installed from a local file, or a repository that is unreachable.
            (c.status)(format!("{e} Trying a download instead."));
            let (recipe, mut evidence) = collect_remote(app, target, c)?;
            evidence.insert(
                0,
                format!("Collecting it from this computer failed ({e}), so it was downloaded."),
            );
            Ok((recipe, evidence))
        }
        r => r,
    }
}

fn collect_local(app: &App, c: &mut Collect) -> Result<(Recipe, Vec<String>)> {
    if !plain_name(&app.native_id)
        && matches!(
            app.source,
            Source::Flatpak
                | Source::Rpm
                | Source::Dpkg
                | Source::Pacman
                | Source::Apk
                | Source::Snap
        )
    {
        return Err(invalid(format!(
            "Unexpected package name {:?}.",
            app.native_id
        )));
    }
    match app.source {
        Source::Flatpak => collect_flatpak(app, c),
        Source::Rpm => collect_rpm(app, c),
        Source::Dpkg | Source::Pacman | Source::Apk => crate::native_sets::collect(app, c),
        Source::Snap => collect_snap(app, c),
        Source::AppImage | Source::Custom => collect_program(app, c),
        _ => Err(invalid("No installer can be collected for this app.")),
    }
}

fn collect_flatpak(app: &App, c: &mut Collect) -> Result<(Recipe, Vec<String>)> {
    let refs = flatpak_refs(&app.native_id, c.cancel)?;
    let mut bundles = Vec::new();
    let mut made = Vec::new();
    for r in &refs {
        if let Some(f) = c.shared.get(r) {
            bundles.push(Package {
                name: r.clone(),
                file: f.clone(),
            });
            continue;
        }
        let p: Vec<&str> = r.split('/').collect();
        let [kind, name, arch, branch] = p[..] else {
            return Err(invalid(format!("Unexpected Flatpak ref {r}")));
        };
        let rel = format!(
            "installers/flatpak/{}",
            portable_name(&format!("{name}-{branch}-{arch}.flatpak"))
        );
        let dst = local(c.staging, &rel);
        if let Some(d) = dst.parent() {
            fs::create_dir_all(d).ctx(|| format!("Creating {}", d.display()))?;
        }
        let _ = fs::remove_file(&dst);
        (c.status)(format!(
            "Bundling {name} {branch} from the Flatpak repository (large runtimes take several minutes)"
        ));
        // `<installation>/<kind>/<name>/<arch>/<branch>/<commit>` → `<installation>/repo`
        let location = PathBuf::from(flatpak(&["info", "--show-location", r], c.cancel)?);
        let repo = location
            .ancestors()
            .nth(5)
            .map(|i| i.join("repo"))
            .filter(|p| p.is_dir())
            .ok_or_else(|| invalid(format!("No Flatpak repository found for {r}.")))?;
        let mut cmd = Command::new("flatpak");
        cmd.arg("build-bundle");
        if kind == "runtime" {
            cmd.arg("--runtime");
        }
        cmd.arg(format!("--arch={arch}"))
            .arg(&repo)
            .arg(&dst)
            .arg(name)
            .arg(branch);
        run(&mut cmd, c.cancel)?;
        let f = stored(c, rel)?;
        c.shared.insert(r.clone(), f.clone());
        made.push(r.clone());
        bundles.push(Package {
            name: r.clone(),
            file: f,
        });
    }
    let app_ref = refs.last().cloned().unwrap_or_default();
    let mut evidence = vec![format!(
        "Bundled with flatpak build-bundle from this computer's repository: {}",
        refs.join(", ")
    )];
    if made.len() < refs.len() {
        evidence.push("Runtime bundles are shared with other apps in this snapshot.".into());
    }
    Ok((Recipe::Flatpak { app_ref, bundles }, evidence))
}

fn collect_rpm(app: &App, c: &mut Collect) -> Result<(Recipe, Vec<String>)> {
    let distro = crate::platform::distro()
        .ok_or_else(|| invalid("Could not identify this Linux distribution (/etc/os-release)."))?;
    let rel_dir = format!("{}/rpms", app_dir(app));
    let dir = local(c.staging, &rel_dir);
    fresh_dir(&dir)?;
    (c.status)(format!(
        "Downloading {} and its dependencies with dnf",
        app.native_id
    ));
    run(
        Command::new("dnf")
            .args([
                "download",
                "--resolve",
                "--alldeps",
                "--setopt=install_weak_deps=False",
                "--destdir",
            ])
            .arg(&dir)
            .arg(&app.native_id),
        c.cancel,
    )?;
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .ctx(|| format!("Reading {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "rpm"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Err(invalid("dnf did not download any packages."));
    }
    let names = run(
        Command::new("rpm")
            .args(["-qp", "--nosignature", "--qf", "%{NAME}\\n"])
            .args(&files),
        c.cancel,
    )?;
    let names: Vec<&str> = names.lines().map(str::trim).collect();
    if names.len() != files.len() {
        return Err(invalid("Could not read the downloaded packages."));
    }
    let signed = match run(
        Command::new("rpmkeys").arg("--checksig").args(&files),
        c.cancel,
    ) {
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        r => r.is_ok(),
    };
    let mut rpms = Vec::new();
    for (f, n) in files.iter().zip(names) {
        let file = f
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let safe = portable_name(&file);
        if safe != file {
            fs::rename(f, dir.join(&safe)).ctx(|| format!("Renaming {file}"))?;
        }
        rpms.push(Package {
            name: n.into(),
            file: stored(c, format!("{rel_dir}/{safe}"))?,
        });
    }
    if !rpms.iter().any(|p| p.name == app.native_id) {
        return Err(invalid(format!(
            "The download did not include {}.",
            app.native_id
        )));
    }
    let bytes: u64 = rpms.iter().map(|p| p.file.size).sum();
    let evidence = vec![
        format!(
            "{} packages ({} MB) downloaded with dnf for {distro}",
            rpms.len(),
            bytes / 1_000_000
        ),
        if signed {
            "Package signatures verified with rpmkeys on this computer.".into()
        } else {
            "Some package signatures could not be verified on this computer (rpmkeys --checksig); from here on the files are protected by checksums only.".into()
        },
    ];
    Ok((
        Recipe::Rpm {
            distro,
            packages: vec![app.native_id.clone()],
            rpms,
        },
        evidence,
    ))
}

/// `base`, content-provider snaps and confinement from a `meta/snap.yaml`.
pub fn parse_snap_yaml(text: &str) -> (Option<String>, Vec<String>, bool) {
    let unquote = |v: &str| v.trim().trim_matches(['"', '\'']).to_string();
    let (mut base, mut providers, mut classic) = (None, Vec::new(), false);
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("base:") {
            base = Some(unquote(v)).filter(|b| !b.is_empty());
        } else if let Some(v) = line.strip_prefix("confinement:") {
            classic = v.trim() == "classic";
        } else if let Some(v) = line.trim().strip_prefix("default-provider:") {
            let v = unquote(v);
            let v = v.split(':').next().unwrap_or_default().to_string();
            if !v.is_empty() && !providers.contains(&v) {
                providers.push(v);
            }
        }
    }
    (base, providers, classic)
}

fn collect_snap(app: &App, c: &mut Collect) -> Result<(Recipe, Vec<String>)> {
    let name = &app.native_id;
    let yaml =
        fs::read_to_string(format!("/snap/{name}/current/meta/snap.yaml")).unwrap_or_default();
    let (base, providers, classic) = parse_snap_yaml(&yaml);
    let mut order: Vec<String> = base.into_iter().chain(providers).collect();
    order.retain(|n| plain_name(n) && n != name);
    order.push(name.clone());
    let rel_dir = format!("{}/snaps", app_dir(app));
    let dir = local(c.staging, &rel_dir);
    fresh_dir(&dir)?;
    let mut snaps = Vec::new();
    for n in &order {
        (c.status)(format!("Downloading snap {n} from the Snap Store"));
        run(
            Command::new("snap")
                .arg("download")
                .arg(format!("--target-directory={}", dir.display()))
                .arg(format!("--basename={n}"))
                .arg(n),
            c.cancel,
        )?;
        snaps.push(SnapFile {
            name: n.clone(),
            snap: stored(c, format!("{rel_dir}/{n}.snap"))?,
            assertion: stored(c, format!("{rel_dir}/{n}.assert"))?,
        });
    }
    Ok((
        Recipe::Snap { classic, snaps },
        vec![format!(
            "Downloaded from the Snap Store with signed assertions: {}",
            order.join(", ")
        )],
    ))
}

fn collect_program(app: &App, c: &mut Collect) -> Result<(Recipe, Vec<String>)> {
    let src = app
        .path
        .as_deref()
        .ok_or_else(|| invalid("The program file is unknown."))?;
    let original = tilde(src, c.home);
    if !original.starts_with("~/") {
        return Err(invalid("The program is outside the home folder."));
    }
    let file = src
        .file_name()
        .map(|n| portable_name(&n.to_string_lossy()))
        .unwrap_or_else(|| "program".into());
    let rel = format!("{}/{file}", app_dir(app));
    let dst = local(c.staging, &rel);
    if let Some(d) = dst.parent() {
        fs::create_dir_all(d).ctx(|| format!("Creating {}", d.display()))?;
    }
    (c.status)(format!("Copying {original}"));
    let mut input = File::open(src).ctx(|| format!("Reading {}", src.display()))?;
    let mut out = File::create(&dst).ctx(|| format!("Writing {}", dst.display()))?;
    let (size, sha256) = match copy_hashed(&mut input, &mut out, c.cancel, |_| {}) {
        Ok(v) => v,
        Err(CopyError::Cancelled) => return Err(Error::Cancelled),
        Err(CopyError::Read(e) | CopyError::Write(e)) => {
            return Err(e).ctx(|| format!("Copying {original}"));
        }
    };
    Ok((
        Recipe::Program {
            original: original.clone(),
            launcher: None,
            file: StoredFile {
                stored: rel,
                size,
                sha256,
            },
        },
        vec![format!("Program file copied from {original}")],
    ))
}

// ---------------------------------------------------------------- downloads

const GITHUB_API: &str = "application/vnd.github+json";

/// Evidence for a download whose source publishes no checksum (also read by RESTORE.md).
pub(crate) const UNCHECKED: &str = "No checksum is published for this download.";

fn winget_pick(
    id: &str,
    have: Option<&str>,
    arch: Arch,
    c: &mut Collect,
) -> Result<(Pick, String)> {
    let dir = catalog::winget_dir(id).ok_or_else(|| invalid(format!("Unusual winget ID {id}.")))?;
    (c.status)(format!("Looking up {id} in the winget catalog"));
    let listing: Vec<catalog::DirEntry> = fetch::json(
        &format!("https://api.github.com/repos/microsoft/winget-pkgs/contents/{dir}"),
        Some(GITHUB_API),
        c.cancel,
    )?;
    let versions = catalog::winget_versions(&listing);
    // The version the old computer has, if the catalog still lists it; else the newest.
    let v = have
        .and_then(|h| versions.iter().find(|v| *v == h))
        .or(versions.first())
        .ok_or_else(|| invalid(format!("The winget catalog lists no versions of {id}.")))?;
    let yaml = fetch::text(
        &format!(
            "https://raw.githubusercontent.com/microsoft/winget-pkgs/master/{dir}/{v}/{id}.installer.yaml"
        ),
        None,
        c.cancel,
    )?;
    let pick = catalog::winget_installer(&yaml, arch).map_err(invalid)?;
    Ok((pick, format!("winget manifest {id} {v}")))
}

fn cask_pick(token: &str, arch: Arch, c: &mut Collect) -> Result<(catalog::CaskPick, String)> {
    if !plain_name(token) {
        return Err(invalid(format!("Unusual Homebrew cask name {token:?}.")));
    }
    (c.status)(format!("Looking up Homebrew cask {token}"));
    let json = fetch::text(
        &format!("https://formulae.brew.sh/api/cask/{token}.json"),
        None,
        c.cancel,
    )?;
    let pick = catalog::cask_pick(&json, arch).map_err(invalid)?;
    Ok((pick, format!("Homebrew cask {token}")))
}

/// A tag name worth asking GitHub about (not a branch@commit, a bare commit or "latest").
pub(crate) fn release_tag(reference: Option<&str>) -> Option<&str> {
    reference.filter(|r| {
        !r.is_empty()
            && *r != "latest"
            && !r.contains(['@', '/', ' ', '?', '#', '%'])
            && !(r.len() >= 7
                && r.bytes().all(|b| b.is_ascii_hexdigit())
                && r.bytes().any(|b| b.is_ascii_alphabetic()))
    })
}

/// The release asset for `target`: from the release the old computer had (its tag), else from
/// the newest release that has a file the destination can use (stable releases first unless
/// the user allowed prereleases for this app; drafts are never used). The user's file-name
/// patterns for this app and target narrow the choice. GitHub's digest and the release's
/// checksum list are compared when both exist.
fn github_pick(
    app: &App,
    repo: &str,
    reference: Option<&str>,
    target: Platform,
    c: &mut Collect,
) -> Result<(Pick, String)> {
    let api = format!("https://api.github.com/repos/{repo}/releases");
    let mut want = catalog::Want::for_system(c.to, target);
    want.rules = app
        .prefs
        .asset_rules
        .iter()
        .filter(|r| r.target == target)
        .map(|r| r.pattern.clone())
        .collect();
    let mut tried = Vec::new();
    let mut found = None;
    if let Some(t) = release_tag(reference) {
        (c.status)(format!("Looking up release {t} of github.com/{repo}"));
        match fetch::json::<catalog::Release>(
            &format!("{api}/tags/{t}"),
            Some(GITHUB_API),
            c.cancel,
        )
        .and_then(|rel| {
            let pick = catalog::pick_asset(&rel, &want).map_err(invalid)?;
            Ok((pick, rel.tag_name, rel.prerelease))
        }) {
            Ok(f) => found = Some(f),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(e) => tried.push(e.to_string()),
        }
    }
    if found.is_none() {
        (c.status)(format!("Looking up the releases of github.com/{repo}"));
        let mut rels: Vec<catalog::Release> =
            fetch::json(&format!("{api}?per_page=100"), Some(GITHUB_API), c.cancel)?;
        rels.retain(|r| !r.draft);
        if !app.prefs.prerelease {
            // A stable sort keeps the newest first within stable and within prereleases.
            rels.sort_by_key(|r| r.prerelease);
        }
        if rels.is_empty() {
            tried.push(format!("github.com/{repo} has no published releases."));
        }
        for rel in rels {
            match catalog::pick_asset(&rel, &want) {
                Ok(pick) => {
                    found = Some((pick, rel.tag_name, rel.prerelease));
                    break;
                }
                // The newest release's reason is the one worth showing.
                Err(e) if tried.len() < 2 => tried.push(e),
                Err(_) => {}
            }
        }
    }
    let Some((mut pick, tag, pre)) = found else {
        return Err(invalid(tried.join(" ")));
    };
    if let Some(sums) = pick.sums_url.clone() {
        let list = fetch::text(&sums, None, c.cancel)?;
        let digest = (!pick.sha256.is_empty()).then(|| pick.sha256.clone());
        match catalog::verified_sha(&pick.name, digest.as_deref(), Some(&list)) {
            Ok((sha, by)) => {
                pick.sha256 = sha;
                pick.detail += &format!("; checked against {by}");
            }
            Err(e) if digest.is_some() || list.contains(&pick.name) => return Err(invalid(e)),
            Err(_) => {
                return Err(invalid(format!(
                    "The checksum list of {repo} does not cover {}.",
                    pick.name
                )));
            }
        }
    }
    let pre = if pre { " (a prerelease)" } else { "" };
    Ok((pick, format!("GitHub release {tag}{pre} of {repo}")))
}

/// A link the user registered, described by its file name (links carry no catalog data).
fn registered_pick(
    d: &crate::discovery::Download,
    target: Platform,
    version: Option<&str>,
) -> Pick {
    let path = d.url.split(['?', '#']).next().unwrap_or_default();
    let name = path.rsplit('/').next().unwrap_or_default().to_string();
    let lower = name.to_ascii_lowercase();
    let archive = [".tar.gz", ".tgz", ".tar.xz", ".zip"]
        .iter()
        .any(|e| lower.ends_with(e))
        // A macOS zip usually holds an app, installed like a disk image.
        && !(target.os == OsFamily::Macos && lower.ends_with(".zip"));
    let win = (target.os == OsFamily::Windows && !archive).then(|| {
        if lower.ends_with(".msi") {
            WinInstaller::Msi
        } else if lower.ends_with(".msix") || lower.ends_with(".msixbundle") {
            WinInstaller::Msix
        } else {
            WinInstaller::Exe
        }
    });
    Pick {
        url: d.url.clone(),
        sha256: d.sha256.to_ascii_lowercase(),
        version: version.unwrap_or("download").to_string(),
        win,
        detail: "link you registered".into(),
        name,
        archive,
        ..Pick::default()
    }
}

/// An `.exe` named like a program rather than an installer (`jq-windows-amd64.exe`, not
/// `App-Setup-1.2.exe`).
fn portable_exe(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".exe")
        && (lower.contains("portable")
            || ![
                "setup",
                "install",
                "installer",
                "update",
                "updater",
                "uninstall",
            ]
            .iter()
            .any(|w| {
                lower
                    .split(|c: char| !c.is_ascii_alphanumeric())
                    .any(|t| t == *w)
            }))
}

/// Disk image, zip archive or installer package, by content (download links often have no
/// file extension).
fn mac_container(p: &Path) -> Option<MacContainer> {
    let mut f = File::open(p).ok()?;
    let mut head = [0u8; 4];
    f.read_exact(&mut head).ok()?;
    match &head {
        b"PK\x03\x04" => return Some(MacContainer::Zip),
        b"xar!" => return Some(MacContainer::Pkg),
        _ => {}
    }
    // Disk images end with a 512-byte "koly" trailer.
    let len = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(len.checked_sub(512)?)).ok()?;
    f.read_exact(&mut head).ok()?;
    (&head == b"koly").then_some(MacContainer::Dmg)
}

/// Collects the installer for `target` from a vetted catalog download (registered link,
/// winget, Homebrew cask, else the app's GitHub release) into `c.staging`. Used by the build
/// for other platforms, and by restore for apps the snapshot has no installer for.
pub(crate) fn collect_remote(
    app: &App,
    target: Platform,
    c: &mut Collect,
) -> Result<(Recipe, Vec<String>)> {
    let registered = app.downloads.iter().any(|d| d.target == target);
    if target.os == OsFamily::Linux
        && !registered
        && c.to
            .profile()
            .is_some_and(|p| p.appimage == crate::distros::Avail::Unsupported)
    {
        return Err(invalid(format!(
            "Downloaded Linux programs do not run on {}.",
            c.to.profile().map_or("that system", |p| p.name.as_str())
        )));
    }
    let entry = app.canonical.as_deref().and_then(registry::get);
    let catalog_id = match target.os {
        OsFamily::Windows => winget_id(app),
        OsFamily::Macos => cask_token(app),
        OsFamily::Linux => None,
    };
    let reference = app.github.as_ref().and_then(|g| g.reference.as_deref());
    let registered = app.downloads.iter().find(|d| d.target == target);
    let mut fallback = None;
    let (pick, app_bundle, origin) = match (target.os, catalog_id) {
        _ if let Some(d) = registered => {
            d.check()?;
            let pick = registered_pick(d, target, app.version.as_deref());
            (pick, None, "your download".to_string())
        }
        (os, Some(id)) => {
            let found = if os == OsFamily::Windows {
                winget_pick(&id, app.version.as_deref(), target.arch, c).map(|(p, o)| (p, None, o))
            } else {
                cask_pick(&id, target.arch, c).map(|(p, o)| (p.pick, p.app, o))
            };
            match (found, github_repo(app)) {
                (Ok(f), _) => f,
                (Err(Error::Cancelled), _) => return Err(Error::Cancelled),
                (Err(e), Some(repo)) => {
                    fallback = Some(format!(
                        "The catalog lookup failed ({e}); the app's GitHub release was used instead."
                    ));
                    let (p, o) = github_pick(app, repo, reference, target, c)?;
                    (p, None, o)
                }
                (Err(e), None) => return Err(e),
            }
        }
        _ => {
            let repo = github_repo(app)
                .ok_or_else(|| invalid(format!("No catalog lists {} for {target}.", app.name)))?;
            let (p, o) = github_pick(app, repo, reference, target, c)?;
            (p, None, o)
        }
    };
    let lower = pick.name.to_ascii_lowercase();
    let ext = match (target.os, pick.win) {
        _ if pick.program => "program",
        _ if pick.archive && lower.ends_with(".zip") => "zip",
        _ if pick.archive && lower.ends_with(".tar.xz") => "tar.xz",
        _ if pick.archive => "tar.gz",
        (OsFamily::Windows, Some(WinInstaller::Msi)) => "msi",
        (OsFamily::Windows, Some(WinInstaller::Msix)) => "msix",
        (OsFamily::Windows, _) => "exe",
        (OsFamily::Macos, _) => "download",
        (OsFamily::Linux, _) if lower.ends_with(".deb") => "deb",
        (OsFamily::Linux, _) if lower.ends_with(".rpm") => "rpm",
        (OsFamily::Linux, _) => "AppImage",
    };
    let dir = format!("installers/{target}/{}", portable_name(&app.name));
    let stem = portable_name(&format!("{}-{}", app.name, pick.version));
    let mut rel = format!("{dir}/{stem}.{ext}");
    let dst = local(c.staging, &rel);
    fresh_dir(dst.parent().unwrap_or(c.staging))?;
    (c.status)(format!(
        "Downloading the {target} installer ({})",
        pick.detail
    ));
    // Only Homebrew `no_check` casks and GitHub assets without any checksum get here without
    // one (winget entries and registered links always carry one).
    if ext == "rpm" && pick.sha256.is_empty() {
        return Err(invalid("Vendor RPM requires a published SHA-256."));
    }
    let published = (!pick.sha256.is_empty()).then_some(pick.sha256.as_str());
    if published.is_none() && !c.allow_unverified {
        return Err(invalid(format!(
            "{UNCHECKED} Installing {} from {} needs your approval (allow unverified downloads).",
            app.name, pick.url
        )));
    }
    match fetch::download(&pick.url, &dst, published, c.cancel) {
        Ok(_) => {}
        Err(Error::Cancelled) => return Err(Error::Cancelled),
        // Files of private repositories are only served through the API, with a token.
        Err(e) => match pick
            .api_url
            .as_deref()
            .filter(|_| fetch::has_github_token())
        {
            Some(api) => {
                fetch::download_asset(api, &dst, published, c.cancel)?;
            }
            None => return Err(e),
        },
    }
    let mut evidence: Vec<String> = fallback.into_iter().collect();
    evidence.push(format!(
        "Downloaded from {} ({origin}, {}).",
        pick.url, pick.detail
    ));
    evidence.push(if registered.is_some() {
        "The file matched the SHA-256 checksum you entered.".into()
    } else if published.is_none() {
        let host = pick.url.trim_start_matches("https://").split('/').next().unwrap_or_default();
        let (_, sha) = hash_file(&dst, c.cancel, |_| {})?;
        format!(
            "{UNCHECKED} It was fetched over HTTPS from {host}, and its SHA-256 {sha} is pinned in this snapshot."
        )
    } else if pick.sums_url.is_some() {
        "The file matched the SHA-256 checksum in the release's checksum list.".into()
    } else {
        "The file matched the SHA-256 checksum the catalog publishes.".into()
    });
    // A macOS zip holding an app is installed like a disk image; other archives hold a program.
    let zipped_app = (target.os == OsFamily::Macos && ext == "zip")
        .then(|| zip_listing(&dst).and_then(|n| catalog::zip_app(&n)))
        .flatten();
    let recipe = match target.os {
        // A bare program (`jq-linux-amd64`): checked to be one for this processor, then
        // installed under the command's own name.
        _ if pick.program => {
            if !is_program(&dst, target) {
                let _ = fs::remove_file(&dst);
                return Err(invalid(format!(
                    "{} is not a {} program for {}.",
                    pick.name,
                    os_label(target.os),
                    target.arch.label()
                )));
            }
            let command = app
                .github
                .as_ref()
                .and_then(|g| g.repo.split('/').nth(1))
                .map_or_else(|| app.name.to_lowercase(), str::to_lowercase);
            let command = portable_name(&command);
            let original = match target.os {
                OsFamily::Windows => format!("~/AppData/Local/Programs/{command}/{command}.exe"),
                _ => format!("~/.local/bin/{command}"),
            };
            evidence.push(format!(
                "{} is the program itself; it goes to {original}.",
                pick.name
            ));
            Recipe::Program {
                original,
                file: stored(c, rel.clone())?,
                launcher: None,
            }
        }
        _ if pick.archive && zipped_app.is_none() => {
            let (recipe, what, keep) = program_from_archive(app, target, &dst, &rel, &dir, c)?;
            if !keep {
                let _ = fs::remove_file(&dst);
            }
            evidence.push(what);
            recipe
        }
        OsFamily::Windows => {
            let mut installer = pick.win.unwrap_or(WinInstaller::Exe);
            if installer == WinInstaller::Exe
                && pick.silent.is_none()
                && let Some(k) = file_head(&dst, 8 << 20)
                    .as_deref()
                    .and_then(catalog::sniff_win_installer)
            {
                installer = k;
                evidence.push(format!(
                    "Recognized as a {k:?} installer, which installs unattended."
                ));
            } else if installer == WinInstaller::Exe
                && pick.silent.is_none()
                && portable_exe(&pick.name)
                && is_program(&dst, target)
            {
                // No installer inside and not named like one: the program itself.
                let file = format!("{}.exe", portable_name(&app.name));
                let original = format!(
                    "~/AppData/Local/Programs/{}/{file}",
                    portable_name(&app.name)
                );
                evidence.push(format!(
                    "{} is the program itself (no installer inside); it goes to {original}.",
                    pick.name
                ));
                let renamed = format!("{dir}/{file}");
                fs::rename(&dst, local(c.staging, &renamed)).ctx(|| format!("Renaming {rel}"))?;
                return Ok((
                    Recipe::Program {
                        original,
                        file: stored(c, renamed)?,
                        launcher: None,
                    },
                    evidence,
                ));
            }
            // Names to find it under in the installed-programs list afterwards.
            let mut product: Vec<String> = entry
                .and_then(|e| e.packages.get("arp").cloned())
                .unwrap_or_default();
            for n in entry
                .map(|e| e.name.clone())
                .into_iter()
                .chain([app.name.clone()])
            {
                if !product.contains(&n) {
                    product.push(n);
                }
            }
            Recipe::Windows {
                silent: pick.silent.filter(|_| installer == WinInstaller::Exe),
                installer,
                file: stored(c, rel)?,
                product,
            }
        }
        OsFamily::Macos => {
            let container = mac_container(&dst).ok_or_else(|| {
                invalid("The macOS download is not a disk image, zip archive or installer package.")
            })?;
            let ext = format!("{container:?}").to_lowercase();
            let renamed = format!("{dir}/{stem}.{ext}");
            fs::rename(&dst, local(c.staging, &renamed)).ctx(|| format!("Renaming {rel}"))?;
            rel = renamed;
            let bundle_id = match app.source {
                Source::AppBundle => Some(app.native_id.clone()),
                _ => entry.and_then(|e| e.packages.get("app_bundle")?.first().cloned()),
            };
            let inside = app_bundle.or(zipped_app).or_else(|| {
                (container == MacContainer::Dmg && cfg!(target_os = "macos"))
                    .then(|| dmg_app(&local(c.staging, &rel)))
                    .flatten()
            });
            Recipe::MacApp {
                container,
                file: stored(c, rel)?,
                app: inside.filter(|_| container != MacContainer::Pkg),
                bundle_id,
            }
        }
        OsFamily::Linux if ext == "rpm" => {
            if !wants_rpm(c.to) {
                return Err(invalid("This destination does not accept vendor RPMs."));
            }
            let (package, arch) = crate::native_sets::rpm_metadata(&dst)?;
            let want = target.arch.info().rpm.as_deref().unwrap_or_default();
            if !plain_name(&package) || (arch != want && arch != "noarch") {
                return Err(invalid("Vendor RPM architecture mismatch"));
            }
            evidence.push(format!(
                "Upstream RPM {package} for {arch}; destination dependencies must be resolved."
            ));
            Recipe::VendorRpm {
                profile: c
                    .to
                    .profile()
                    .ok_or_else(|| invalid("Unknown RPM destination"))?
                    .id
                    .clone(),
                package,
                file: stored(c, rel)?,
            }
        }
        OsFamily::Linux if file_head(&dst, 8).as_deref() == Some(&b"!<arch>\n"[..]) => {
            let (package, arch) = deb_control(&dst, c.staging)?;
            let want = target.arch.info().deb.as_deref().unwrap_or_default();
            if arch != want && arch != "all" {
                let _ = fs::remove_file(&dst);
                return Err(invalid(format!(
                    "The .deb package is for {arch}, not {}.",
                    target.arch
                )));
            }
            evidence.push(format!("A Debian package ({package}) for {arch}."));
            Recipe::Deb {
                package,
                file: stored(c, rel)?,
            }
        }
        OsFamily::Linux => {
            if elf_arch(&dst) != Some(target.arch) {
                let _ = fs::remove_file(&dst);
                return Err(invalid(format!(
                    "The Linux download is not a self-contained program (AppImage) for {}.",
                    target.arch
                )));
            }
            Recipe::Program {
                original: format!("~/Applications/{stem}.AppImage"),
                file: stored(c, rel)?,
                launcher: Some(app.name.clone()).filter(|n| crate::manifest::launcher_name_ok(n)),
            }
        }
    };
    crate::manifest::check_recipe(&recipe)?;
    Ok((recipe, evidence))
}

/// The first `max` bytes of a file.
fn file_head(p: &Path, max: u64) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    File::open(p).ok()?.take(max).read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Entry names of a zip file, reading only its central directory.
fn zip_listing(p: &Path) -> Option<Vec<String>> {
    Some(zip_entries(p)?.into_iter().map(|(n, _)| n).collect())
}

/// Names and Unix modes of a zip file's entries, reading only its central directory.
fn zip_entries(p: &Path) -> Option<Vec<(String, u32)>> {
    let mut f = File::open(p).ok()?;
    let len = f.metadata().ok()?.len();
    let tail_len = len.min(65_557 + 22);
    f.seek(SeekFrom::Start(len - tail_len)).ok()?;
    let mut tail = Vec::new();
    f.take(tail_len).read_to_end(&mut tail).ok()?;
    let (count, offset, size) = catalog::zip_eocd(&tail)?;
    let mut f = File::open(p).ok()?;
    f.seek(SeekFrom::Start(offset)).ok()?;
    let mut cd = Vec::new();
    f.take(size as u64).read_to_end(&mut cd).ok()?;
    catalog::zip_central_entries(&cd, count)
}

/// Shared libraries travel with the program they belong to; they are not programs themselves.
fn is_library(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.ends_with(".dll")
        || name.ends_with(".dylib")
        || name.ends_with(".so")
        || name.contains(".so.")
}

/// A link inside `root` whose target is absolute or climbs out of `root`.
fn escaping_link(root: &Path) -> Option<PathBuf> {
    walkdir::WalkDir::new(root)
        .into_iter()
        .flatten()
        .filter(|e| e.path_is_symlink())
        .find(|e| {
            let Ok(t) = fs::read_link(e.path()) else {
                return true;
            };
            let parent = e.path().parent().unwrap_or(root);
            let rel = parent.strip_prefix(root).unwrap_or(Path::new(""));
            let mut depth = rel.components().count() as i64;
            for c in t.components() {
                match c {
                    std::path::Component::Normal(_) => depth += 1,
                    std::path::Component::CurDir => {}
                    std::path::Component::ParentDir => depth -= 1,
                    _ => return true,
                }
                if depth < 0 {
                    return true;
                }
            }
            false
        })
        .map(walkdir::DirEntry::into_path)
}

/// The single app at the top of a disk image (macOS only: mounts it read-only briefly).
fn dmg_app(file: &Path) -> Option<String> {
    let mnt = std::env::temp_dir().join(format!("simple-setup-probe-{}", std::process::id()));
    fs::create_dir_all(&mnt).ok()?;
    let attached = run(
        Command::new("hdiutil")
            .args([
                "attach",
                "-nobrowse",
                "-readonly",
                "-noautoopen",
                "-mountpoint",
            ])
            .arg(&mnt)
            .arg(file),
        &NEVER,
    )
    .is_ok();
    let apps: Vec<String> = fs::read_dir(&mnt)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.ends_with(".app"))
                .collect()
        })
        .unwrap_or_default();
    if attached {
        let _ = run(
            Command::new("hdiutil")
                .arg("detach")
                .arg(&mnt)
                .arg("-force"),
            &NEVER,
        );
    }
    let _ = fs::remove_dir(&mnt);
    (apps.len() == 1).then(|| apps[0].clone())
}

/// The processor an ELF program is built for (`None`: not ELF, or a processor Simple Setup
/// has no name for).
fn elf_arch(p: &Path) -> Option<Arch> {
    let head = file_head(p, 20)?;
    if head.get(..4)? != b"\x7fELF" {
        return None;
    }
    let (class, data) = (*head.get(4)?, *head.get(5)?);
    let m = [*head.get(18)?, *head.get(19)?];
    let machine = if data == 2 {
        u16::from_be_bytes(m)
    } else {
        u16::from_le_bytes(m)
    };
    Arch::from_elf(class, data, machine)
}

/// `Package` and `Architecture` from a `.deb`'s control file. A `.deb` is an `ar` archive whose
/// `control.tar.*` member holds it; that member is unpacked into a scratch folder under `scratch`.
// ponytail: unpacks with the system `tar` (gzip/xz/zstd detected by it); a build machine whose
// tar cannot read the compression gets no installer for that app, like any failed download.
fn deb_control(deb: &Path, scratch: &Path) -> Result<(String, String)> {
    let bad = || invalid(format!("{} is not a Debian package.", deb.display()));
    let mut f = File::open(deb).ctx(|| format!("Opening {}", deb.display()))?;
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic).map_err(|_| bad())?;
    if &magic != b"!<arch>\n" {
        return Err(bad());
    }
    loop {
        let mut h = [0u8; 60];
        f.read_exact(&mut h).map_err(|_| bad())?;
        let name = String::from_utf8_lossy(&h[..16]);
        let name = name.trim_end().trim_end_matches('/');
        let size: u64 = String::from_utf8_lossy(&h[48..58])
            .trim()
            .parse()
            .map_err(|_| bad())?;
        let ok_name = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.');
        if !(ok_name && name.starts_with("control.tar")) {
            // Members are padded to an even length.
            f.seek(SeekFrom::Current((size + size % 2) as i64))
                .map_err(|_| bad())?;
            continue;
        }
        let dir = scratch.join(".deb-control");
        fresh_dir(&dir)?;
        let tarball = dir.join(name);
        let mut out = File::create(&tarball).ctx(|| format!("Creating {}", tarball.display()))?;
        std::io::copy(&mut (&mut f).take(size), &mut out).map_err(|_| bad())?;
        drop(out);
        let unpacked = dir.join("x");
        fs::create_dir(&unpacked).ctx(|| format!("Creating {}", unpacked.display()))?;
        let read = run(
            Command::new("tar")
                .arg("-xf")
                .arg(&tarball)
                .arg("-C")
                .arg(&unpacked),
            &NEVER,
        )
        .and_then(|_| {
            let control = unpacked.join("control");
            // Only a regular file: a link could point anywhere on this computer.
            if !fs::symlink_metadata(&control).is_ok_and(|m| m.is_file()) {
                return Err(bad());
            }
            fs::read_to_string(&control).map_err(|_| bad())
        });
        let _ = fs::remove_dir_all(&dir);
        let text = read?;
        let field = |k: &str| {
            text.lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix(':'))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        return Ok((
            field("Package").ok_or_else(bad)?,
            field("Architecture").ok_or_else(bad)?,
        ));
    }
}

/// Whether APT lists a package as installed.
pub(crate) fn deb_installed(package: &str) -> bool {
    Command::new("dpkg-query")
        .args(["-W", "-f=${Status}", package])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("install ok installed"))
}

/// Whether a file is a program for `target` (by its first bytes; Linux programs must also
/// match the processor, since Linux cannot run other architectures).
fn is_program(p: &Path, target: Platform) -> bool {
    let head = file_head(p, 4).unwrap_or_default();
    match target.os {
        OsFamily::Linux => elf_arch(p) == Some(target.arch),
        OsFamily::Macos => matches!(
            head.as_slice(),
            [0xfe, 0xed, 0xfa, 0xcf | 0xce]
                | [0xcf | 0xce, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
        ),
        OsFamily::Windows => {
            head.starts_with(b"MZ") && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))
        }
    }
}

/// Unpacks a release archive into the fresh folder `out` with the system `tar` (bsdtar on
/// Windows and macOS also reads zip; `unzip` where GNU tar cannot). Zip entries are checked
/// before anything is written; links that point out of `out` are refused afterwards (GNU tar
/// and bsdtar already refuse absolute and `..` names and never write through links they made).
pub(crate) fn unpack(archive: &Path, out: &Path, what: &str, cancel: &AtomicBool) -> Result<()> {
    fresh_dir(out)?;
    let zip = archive
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
    if zip {
        let entries = zip_entries(archive)
            .ok_or_else(|| invalid(format!("The {what} download is not a readable zip file.")))?;
        if let Some((name, why)) = entries
            .iter()
            .find_map(|(n, m)| catalog::zip_entry_problem(n, *m).map(|w| (n, w)))
        {
            return Err(invalid(format!(
                "The {what} download was refused: its entry {name:?} {why}."
            )));
        }
    }
    let mut cmd = if zip && which("unzip").is_some() {
        let mut u = Command::new("unzip");
        u.args(["-q", "-o"]).arg(archive).arg("-d").arg(out);
        u
    } else {
        let mut t = Command::new("tar");
        t.arg("-xf").arg(archive).arg("-C").arg(out);
        t
    };
    run(&mut cmd, cancel)?;
    if let Some(l) = escaping_link(out) {
        let _ = fs::remove_dir_all(out);
        return Err(invalid(format!(
            "The {what} download was refused: {} links outside the archive.",
            l.strip_prefix(out).unwrap_or(&l).display()
        )));
    }
    Ok(())
}

/// The archive's single top-level folder, if everything is inside one.
fn single_top(out: &Path) -> Option<String> {
    let entries: Vec<_> = fs::read_dir(out).ok()?.flatten().collect();
    match entries.as_slice() {
        [one] if one.file_type().ok()?.is_dir() => {
            Some(one.file_name().to_string_lossy().into_owned())
        }
        _ => None,
    }
}

/// From a release archive: the one program in it, installed alone into `~/.local/bin`
/// (Windows: `%LOCALAPPDATA%\\Programs\\<app>`); or, when the program comes with its own
/// libraries, the whole archive, unpacked into its own folder at restore ([`Recipe::Archive`]).
/// Archives with several programs are refused unless one is named after the app or its
/// repository. `keep` in the result: the archive file itself is the installer.
fn program_from_archive(
    app: &App,
    target: Platform,
    archive: &Path,
    archive_rel: &str,
    dir: &str,
    c: &mut Collect,
) -> Result<(Recipe, String, bool)> {
    let out = local(c.staging, &format!("{dir}/unpacked"));
    (c.status)(format!("Unpacking the {} download", app.name));
    unpack(archive, &out, &app.name, c.cancel)?;
    // walkdir does not follow links, so only regular files inside the archive are candidates.
    let files: Vec<PathBuf> = walkdir::WalkDir::new(&out)
        .into_iter()
        .flatten()
        .filter(|e| e.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .collect();
    let with_libraries = files.iter().any(|p| is_library(p));
    let progs: Vec<PathBuf> = files
        .into_iter()
        .filter(|p| !is_library(p) && is_program(p, target))
        .collect();
    let stem = |p: &PathBuf| {
        p.file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    let names: Vec<String> = [
        Some(app.name.as_str()),
        app.github.as_ref().and_then(|g| g.repo.split('/').nth(1)),
    ]
    .into_iter()
    .flatten()
    .map(str::to_lowercase)
    .collect();
    let prog = match progs.as_slice() {
        [one] => one.clone(),
        [] => {
            let _ = fs::remove_dir_all(&out);
            return Err(invalid(format!(
                "The {} download holds no {} program.",
                app.name, target.os
            )));
        }
        many => match many.iter().find(|p| names.contains(&stem(p))) {
            Some(p) => p.clone(),
            None => {
                let _ = fs::remove_dir_all(&out);
                return Err(invalid(format!(
                    "The {} download holds several programs; install it by hand.",
                    app.name
                )));
            }
        },
    };
    let file = prog
        .file_name()
        .map(|n| portable_name(&n.to_string_lossy()))
        .unwrap_or_default();
    if with_libraries {
        let top = single_top(&out);
        let base = top.as_ref().map_or(out.clone(), |t| out.join(t));
        let program = crate::paths::rel_string(&prog, &base).unwrap_or_default();
        let _ = fs::remove_dir_all(&out);
        let folder = portable_name(&app.name);
        let (dir, link) = match target.os {
            OsFamily::Windows => (format!("~/AppData/Local/Programs/{folder}"), None),
            _ => (
                format!("~/.local/opt/{folder}"),
                Some(format!("~/.local/bin/{file}")),
            ),
        };
        let what = format!(
            "The archive holds {file} with its own libraries; it is unpacked into {dir}{}.",
            link.as_deref()
                .map(|l| format!(" and {file} is linked as {l}"))
                .unwrap_or_default()
        );
        return Ok((
            Recipe::Archive {
                file: stored(c, archive_rel.to_string())?,
                dir,
                program,
                link,
                top,
            },
            what,
            true,
        ));
    }
    let rel = format!("{dir}/{file}");
    fs::rename(&prog, local(c.staging, &rel)).ctx(|| format!("Moving {file}"))?;
    let _ = fs::remove_dir_all(&out);
    let original = match target.os {
        OsFamily::Windows => format!(
            "~/AppData/Local/Programs/{}/{file}",
            portable_name(&app.name)
        ),
        _ => format!("~/.local/bin/{file}"),
    };
    let what = format!("Unpacked the program {file} from the archive; it goes to {original}.");
    Ok((
        Recipe::Program {
            original,
            file: stored(c, rel)?,
            launcher: None,
        },
        what,
        false,
    ))
}

/// A bundled installer file as an absolute path. Package managers read a relative path as a
/// package name (`apt-get install snap/installers/…` looks for a package of that name), and
/// the snapshot is often given relative to the current folder.
fn installer_path(bundle: &Path, f: &StoredFile) -> std::result::Result<PathBuf, String> {
    let p = join_rel(bundle, &f.stored, true).map_err(|e| e.to_string())?;
    std::path::absolute(&p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Marks a folder made from a release archive, with the archive's SHA-256.
const ARCHIVE_MARK: &str = ".simple-setup-archive";

/// The folder holds a release archive Simple Setup unpacked (any version of it).
fn own_archive_dir(d: &Path) -> bool {
    fs::symlink_metadata(d.join(ARCHIVE_MARK)).is_ok_and(|m| m.is_file())
}

/// This exact archive is unpacked at its folder and its program is there.
fn archive_installed(r: &Recipe, home: &Path) -> bool {
    let Recipe::Archive {
        dir, program, file, ..
    } = r
    else {
        return false;
    };
    untilde(dir, home).is_ok_and(|d| {
        fs::read_to_string(d.join(ARCHIVE_MARK)).is_ok_and(|s| s.trim() == file.sha256)
            && join_rel(&d, program, false).is_ok_and(|p| p.is_file())
    })
}

/// Links an unpacked archive's program into the user's program folder (Unix); never replaces
/// a file that is not already this link.
fn link_program(home: &Path, dir: &str, program: &str, link: &str) -> String {
    let (Ok(d), Ok(l)) = (untilde(dir, home), untilde(link, home)) else {
        return "no link (unusable location)".into();
    };
    let Ok(target) = join_rel(&d, program, false) else {
        return "no link (unusable program path)".into();
    };
    if fs::read_link(&l).is_ok_and(|t| t == target) {
        return format!("{link} (already linked)");
    }
    if fs::symlink_metadata(&l).is_ok() {
        return format!("no link: something else is already at {link}");
    }
    #[cfg(unix)]
    {
        if let Err(e) = ensure_dirs(home, l.parent().unwrap_or(home)) {
            return format!("no link ({e})");
        }
        match std::os::unix::fs::symlink(&target, &l) {
            Ok(()) => format!("{link} → {}", tilde(&target, home)),
            Err(e) => format!("no link ({e})"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = target;
        "no link".into()
    }
}

/// True when restore can install the recipe by itself; otherwise the bundled file waits for the
/// user (see [`check`] for the instructions).
pub fn unattended(r: &Recipe) -> bool {
    match r {
        Recipe::Windows {
            installer: WinInstaller::Exe,
            silent,
            ..
        } => silent.is_some(),
        // A disk image that does not name its app is opened on the Mac to find it.
        Recipe::MacApp { container, app, .. } => match container {
            MacContainer::Pkg => true,
            MacContainer::Zip => app.is_some(),
            MacContainer::Dmg => true,
        },
        _ => true,
    }
}

/// What the user does with a bundled installer restore cannot run by itself.
pub fn by_hand(r: &Recipe) -> Option<String> {
    let f = &r.files().first()?.stored;
    match r {
        Recipe::Windows {
            installer: WinInstaller::Exe,
            silent: None,
            ..
        } => Some(format!(
            "Run {f} from the snapshot (no internet needed); it has no known unattended mode, so it is not started for you."
        )),
        Recipe::MacApp {
            app: None,
            container: MacContainer::Zip,
            ..
        } => Some(format!(
            "Open {f} from the snapshot and drag the app into Applications (no internet needed)."
        )),
        _ => None,
    }
}

// ---------------------------------------------------------------- installing

/// How administrator rights are obtained for package installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Elevation {
    /// Already running as administrator.
    None,
    /// `sudo` (asks for the password in the terminal).
    Sudo,
    /// `pkexec` (asks for the password in a system dialog).
    Pkexec,
}

impl Elevation {
    pub fn detect(terminal: bool) -> Self {
        let root = fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .and_then(|v| v.split_whitespace().nth(1).map(|u| u == "0"))
            })
            .unwrap_or(false);
        match (root, terminal) {
            (true, _) => Self::None,
            (false, true) => Self::Sudo,
            (false, false) => Self::Pkexec,
        }
    }
}

/// Package installs need administrator rights; Flatpak (per user) and program files do not.
pub fn needs_admin(r: &Recipe) -> bool {
    matches!(
        r,
        Recipe::NativeSet { .. }
            | Recipe::VendorRpm { .. }
            | Recipe::Rpm { .. }
            | Recipe::Deb { .. }
            | Recipe::Snap { .. }
            | Recipe::MacApp {
                container: MacContainer::Pkg,
                ..
            }
    )
}

pub(crate) fn admin_cmd(prog: &str, admin: bool, elev: Elevation) -> Command {
    match (admin, elev) {
        (true, Elevation::Sudo) => {
            let mut c = Command::new("sudo");
            c.arg("--").arg(prog);
            c
        }
        (true, Elevation::Pkexec) => {
            let mut c = Command::new("pkexec");
            c.arg(which(prog).unwrap_or_else(|| prog.into()));
            c
        }
        _ => Command::new(prog),
    }
}

/// Installs an app package (MSIX/MSIXBundle) for this user, without administrator rights;
/// Windows refuses packages whose signature it does not trust. The path travels in an
/// environment variable, so nothing from the snapshot becomes PowerShell code.
fn msix_install(file: &Path) -> std::result::Result<(), String> {
    run(
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Add-AppxPackage -Path $env:SS_FILE",
            ])
            .env("SS_FILE", file),
        &NEVER,
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}

/// An app package (MSIX) whose display name is one of `product`'s names is installed for this
/// user (app packages are not in the installed-programs list).
fn msix_installed(product: &[String]) -> bool {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-AppxPackage | ForEach-Object { (Get-AppxPackageManifest $_).Package.Properties.DisplayName }",
        ])
        .stdin(Stdio::null())
        .output();
    out.is_ok_and(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .any(|n| product.iter().any(|p| registry::arp_match(n.trim(), p)))
    })
}

/// The installed-programs list has an entry for one of `product`'s names.
fn win_installed(product: &[String]) -> bool {
    crate::discovery::windows::read_uninstall_entries().is_ok_and(|es| {
        es.iter()
            .any(|e| product.iter().any(|p| registry::arp_match(&e.name, p)))
    })
}

/// The fixed program and arguments for each unattended installer type; an `.exe` gets the
/// catalog's silent switches.
fn win_command(
    kind: WinInstaller,
    silent: Option<&[String]>,
    file: &Path,
) -> Option<(OsString, Vec<OsString>)> {
    let f = OsString::from(file);
    let args = |a: &[&str]| a.iter().map(OsString::from).collect::<Vec<_>>();
    Some(match kind {
        WinInstaller::Msi => {
            let mut a = args(&["/i"]);
            a.push(f);
            a.extend(args(&["/passive", "/norestart"]));
            ("msiexec".into(), a)
        }
        WinInstaller::Nullsoft => (f, args(&["/S"])),
        WinInstaller::Inno => (
            f,
            args(&["/VERYSILENT", "/SUPPRESSMSGBOXES", "/NORESTART", "/SP-"]),
        ),
        WinInstaller::Burn => (f, args(&["/quiet", "/norestart"])),
        WinInstaller::Exe => (f, silent?.iter().map(OsString::from).collect()),
        // Installed by `msix_install` (PowerShell needs the path outside its script text).
        WinInstaller::Msix => return None,
    })
}

/// Runs a Windows installer; `Ok(true)` when it asks for a restart to finish.
/// Runs a program with administrator rights through Windows' consent prompt; `Ok(true)` when
/// it asks for a restart. The values travel in environment variables, never as PowerShell code.
pub(crate) fn run_as_admin(prog: &str, args: &[&str]) -> std::result::Result<bool, String> {
    let joined: Vec<String> = args
        .iter()
        .map(|a| {
            if a.contains(' ') {
                format!("\"{a}\"")
            } else {
                (*a).to_string()
            }
        })
        .collect();
    let status = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$p = Start-Process -FilePath $env:SS_PROGRAM -ArgumentList $env:SS_ARGS -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        ])
        .env("SS_PROGRAM", which(prog).map_or_else(|| prog.into(), PathBuf::into_os_string))
        .env("SS_ARGS", joined.join(" "))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Could not ask for administrator rights: {e}"))?;
    match status.code() {
        Some(0) => Ok(false),
        Some(3010 | 1641) => Ok(true),
        Some(c) => Err(format!("{prog} stopped with code {c}.")),
        None => Err(format!("{prog} was interrupted.")),
    }
}

fn run_win_installer(prog: &OsString, args: &[OsString]) -> std::result::Result<bool, String> {
    let quiet = |c: &mut Command| {
        c.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    let status = match quiet(Command::new(prog).args(args)) {
        // ERROR_ELEVATION_REQUIRED: the installer needs administrator rights, which only
        // Windows' consent prompt grants. Start it through PowerShell's RunAs; the values travel
        // in environment variables, so nothing from the snapshot becomes PowerShell code.
        Err(e) if e.raw_os_error() == Some(740) => {
            let joined: Vec<String> = args
                .iter()
                .map(|a| {
                    let a = a.to_string_lossy();
                    if a.contains(' ') { format!("\"{a}\"") } else { a.into_owned() }
                })
                .collect();
            quiet(
                Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "$p = Start-Process -FilePath $env:SS_PROGRAM -ArgumentList $env:SS_ARGS -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
                    ])
                    .env("SS_PROGRAM", prog)
                    .env("SS_ARGS", joined.join(" ")),
            )
        }
        s => s,
    }
    .map_err(|e| format!("Could not start the installer: {e}"))?;
    // 3010 and 1641: installed, restart required.
    match status.code() {
        Some(0) => Ok(false),
        Some(3010 | 1641) => Ok(true),
        Some(c) => Err(format!(
            "The installer stopped with code {c}; nothing is assumed installed."
        )),
        None => Err("The installer was interrupted.".into()),
    }
}

fn bundle_id_of(app_dir: &Path) -> Option<String> {
    plist::Value::from_file(app_dir.join("Contents/Info.plist"))
        .ok()?
        .as_dictionary()?
        .get("CFBundleIdentifier")?
        .as_string()
        .map(String::from)
}

/// `Ok(Some(path))` if the app is in Applications (with the expected bundle ID, when known),
/// `Err` if a different app of that name is there.
fn mac_find(
    app: &str,
    bundle_id: Option<&str>,
    home: &Path,
) -> std::result::Result<Option<PathBuf>, String> {
    for dir in [PathBuf::from("/Applications"), home.join("Applications")] {
        let p = dir.join(app);
        if !p.is_dir() {
            continue;
        }
        return match (bundle_id, bundle_id_of(&p)) {
            (Some(want), Some(have)) if want != have => Err(format!(
                "A different {app} ({have}) is already in {}; it is left alone.",
                dir.display()
            )),
            _ => Ok(Some(p)),
        };
    }
    Ok(None)
}

/// An app with this bundle identifier is installed (Spotlight's index).
fn mac_bundle_present(id: &str) -> bool {
    plain_name(id)
        && Command::new("mdfind")
            .arg(format!("kMDItemCFBundleIdentifier == '{id}'"))
            .stdin(Stdio::null())
            .output()
            .is_ok_and(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
}

/// What `pkgutil --check-signature` says: signed (by whom), unsigned, or refused (a broken,
/// revoked or untrusted signature).
fn pkg_signature(pkg: &Path) -> std::result::Result<String, String> {
    let out = Command::new("pkgutil")
        .arg("--check-signature")
        .arg(pkg)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Could not check the package's signature: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let status = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("Status:"))
        .map(str::trim)
        .unwrap_or("unknown");
    match status {
        s if s.contains("no signature") => Ok("unsigned (its SHA-256 was verified)".into()),
        s if s.starts_with("signed") && !s.contains("untrusted") => Ok(s.to_string()),
        s => Err(format!(
            "The installer package's signature is not valid ({s}); it is not installed."
        )),
    }
}

/// Installs a macOS installer package for the whole computer: as root, through `sudo`, or
/// through macOS's own password dialog. The package path reaches the dialog's script only as
/// an environment variable quoted by AppleScript, never as script text.
fn mac_install_pkg(
    pkg: &Path,
    elev: Elevation,
    status: &mut dyn FnMut(String),
) -> std::result::Result<String, String> {
    let signature = pkg_signature(pkg)?;
    let receipts = || {
        Command::new("pkgutil")
            .arg("--pkgs")
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .map(String::from)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default()
    };
    let before = receipts();
    status("Running the installer package (macOS asks for an administrator password)".into());
    let mut c = match elev {
        Elevation::None => {
            let mut c = Command::new("installer");
            c.arg("-pkg").arg(pkg).args(["-target", "/"]);
            c
        }
        Elevation::Sudo => {
            let mut c = Command::new("sudo");
            c.args(["--", "installer", "-pkg"])
                .arg(pkg)
                .args(["-target", "/"]);
            c
        }
        Elevation::Pkexec => {
            let mut c = Command::new("osascript");
            c.args([
                "-e",
                "do shell script \"/usr/sbin/installer -pkg \" & quoted form of (system attribute \"SS_PKG\") & \" -target /\" with administrator privileges",
            ])
            .env("SS_PKG", pkg);
            c
        }
    };
    run(&mut c, &NEVER).map_err(|e| e.to_string())?;
    let new: Vec<String> = receipts().difference(&before).cloned().collect();
    Ok(format!(
        "the installer package ({signature}){}",
        if new.is_empty() {
            String::new()
        } else {
            format!("; receipts {}", new.join(", "))
        }
    ))
}

fn mac_install(
    container: MacContainer,
    file: &Path,
    app: &str,
    home: &Path,
    status: &mut dyn FnMut(String),
) -> std::result::Result<(), String> {
    let tmp = std::env::temp_dir().join(format!("simple-setup-{}", std::process::id()));
    fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;
    let go = |c: &mut Command| run(c, &NEVER).map(|_| ()).map_err(|e| e.to_string());
    let result = (|| {
        let src = match container {
            MacContainer::Dmg => {
                status(format!("Opening the disk image for {app}"));
                go(Command::new("hdiutil")
                    .args([
                        "attach",
                        "-nobrowse",
                        "-readonly",
                        "-noautoopen",
                        "-mountpoint",
                    ])
                    .arg(&tmp)
                    .arg(file))?;
                tmp.join(app)
            }
            MacContainer::Zip => {
                go(Command::new("ditto").args(["-x", "-k"]).arg(file).arg(&tmp))?;
                tmp.join(app)
            }
            MacContainer::Pkg => return Err("Installer packages are opened by the user.".into()),
        };
        if !src.is_dir() {
            return Err(format!("{app} was not found in the download."));
        }
        // /Applications is writable for administrators without a password; else ~/Applications.
        let probe =
            Path::new("/Applications").join(format!(".simple-setup-{}", std::process::id()));
        let dest_dir = if fs::write(&probe, b"").is_ok() {
            let _ = fs::remove_file(&probe);
            PathBuf::from("/Applications")
        } else {
            let d = home.join("Applications");
            fs::create_dir_all(&d).map_err(|e| e.to_string())?;
            d
        };
        status(format!("Copying {app} into {}", dest_dir.display()));
        go(Command::new("ditto").arg(&src).arg(dest_dir.join(app)))
    })();
    if container == MacContainer::Dmg {
        let _ = run(
            Command::new("hdiutil")
                .arg("detach")
                .arg(&tmp)
                .arg("-force"),
            &NEVER,
        );
    }
    let _ = fs::remove_dir_all(&tmp);
    result
}

fn same_file(p: &Path, f: &StoredFile) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_file() && m.len() == f.size)
        && hash_file(p, &NEVER, |_| {}).is_ok_and(|(_, h)| h == f.sha256)
}

/// Whether the computer `info` (this one) can use the recipe: `Ok(already installed)`, or why
/// not. The distribution rules are [`crate::compat::bundle_fits`]. Runs only read-only queries.
pub fn check(r: &Recipe, info: &SystemInfo, home: &Path) -> std::result::Result<bool, String> {
    crate::compat::bundle_fits(r, info)?;
    match r {
        Recipe::NativeSet {
            manager, requested, ..
        } => Ok(crate::native_sets::installed(*manager, requested)),
        Recipe::VendorRpm { package, .. } => {
            let present = succeeds("rpm", &["-q", package]);
            if !present && !info.online {
                return Err("The vendor RPM needs destination dependency resolution. Restore online or supply a complete native package set.".into());
            }
            Ok(present)
        }
        Recipe::Flatpak { app_ref, .. } => {
            which("flatpak").ok_or(
                "Flatpak is not installed on this computer. Install Flatpak, then restore again.",
            )?;
            Ok(succeeds("flatpak", &["info", app_ref]))
        }
        Recipe::Rpm {
            distro: d,
            packages,
            ..
        } => {
            if which("dnf").is_none() || which("rpm").is_none() {
                return Err(format!(
                    "The packages were collected for {d}, and this computer has no DNF. Install {} with this system's own software source.",
                    packages.join(", ")
                ));
            }
            Ok(packages.iter().all(|p| succeeds("rpm", &["-q", p])))
        }
        Recipe::Deb { package, .. } => {
            if which("apt-get").is_none() || which("dpkg-query").is_none() {
                return Err(format!(
                    "The .deb package of {package} is for Debian, Ubuntu, Linux Mint and similar systems; this computer has no APT. Install it from this system's software source instead."
                ));
            }
            Ok(deb_installed(package))
        }
        Recipe::Snap { snaps, .. } => {
            which("snap").ok_or("snapd is not installed on this computer.")?;
            Ok(snaps
                .last()
                .is_some_and(|s| succeeds("snap", &["list", &s.name])))
        }
        Recipe::Archive { dir, file, .. } => {
            let d = untilde(dir, home).map_err(|e| e.to_string())?;
            if archive_installed(r, home) {
                Ok(true)
            } else if fs::symlink_metadata(&d).is_ok() && !own_archive_dir(&d) {
                Err(format!(
                    "A different folder is already at {dir}; it is left alone. The archive is in the snapshot at {}.",
                    file.stored
                ))
            } else {
                Ok(false)
            }
        }
        Recipe::Program { original, file, .. } => {
            let dest = untilde(original, home).map_err(|e| e.to_string())?;
            if fs::symlink_metadata(&dest).is_err() {
                Ok(false)
            } else if same_file(&dest, file) {
                Ok(true)
            } else {
                Err(format!(
                    "A different file is already at {original}; it is left alone. The program is in the snapshot at {}.",
                    file.stored
                ))
            }
        }
        Recipe::Windows {
            product, installer, ..
        } => {
            if win_installed(product)
                || (*installer == WinInstaller::Msix && msix_installed(product))
            {
                Ok(true)
            } else {
                by_hand(r).map_or(Ok(false), Err)
            }
        }
        Recipe::MacApp {
            container: MacContainer::Pkg,
            bundle_id,
            ..
        } => Ok(bundle_id.as_deref().is_some_and(mac_bundle_present)),
        Recipe::MacApp { app, bundle_id, .. } => match (by_hand(r), app) {
            (None, Some(a)) => mac_find(a, bundle_id.as_deref(), home).map(|p| p.is_some()),
            // Named when the disk image is opened (see `name_dmg_app`).
            (None, None) => Ok(false),
            (Some(why), _) => Err(why),
        },
    }
}

fn why_or(s: Option<String>) -> String {
    s.unwrap_or_else(|| "Install it by hand from the snapshot.".into())
}

/// What an install did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    pub message: String,
    /// The installer asked for a restart to finish (Windows).
    pub restart: bool,
}

/// Checks every bundled file, then installs. Success here only means the commands finished;
/// call [`installed`] to confirm the app is really there. `bundle` is the snapshot folder, or
/// the download folder for installers fetched during a restore.
pub fn install(
    bundle: &Path,
    r: &Recipe,
    home: &Path,
    elev: Elevation,
    status: &mut dyn FnMut(String),
) -> std::result::Result<Done, String> {
    let mut restart = false;
    let path = |f: &StoredFile| installer_path(bundle, f);
    status("Checking the installer files".into());
    for f in r.files() {
        match hash_file(&path(f)?, &NEVER, |_| {}) {
            Ok((_, h)) if h == f.sha256 => {}
            Ok(_) => {
                return Err(format!(
                    "{} is damaged (checksum mismatch); nothing was installed.",
                    f.stored
                ));
            }
            Err(e) => return Err(format!("{} could not be read: {e}", f.stored)),
        }
    }
    let admin = needs_admin(r);
    let go = |prog: &str, args: Vec<OsString>| {
        run(admin_cmd(prog, admin, elev).args(args), &NEVER)
            .map(|_| ())
            .map_err(|e| e.to_string())
    };
    let os = |s: &[&str]| s.iter().map(OsString::from).collect::<Vec<_>>();
    let mut done = Vec::new();
    match r {
        Recipe::NativeSet {
            manager,
            requested,
            packages,
            ..
        } => {
            if !crate::native_sets::installed(*manager, requested) {
                let paths = packages
                    .iter()
                    .filter(|p| !p.file.stored.ends_with(".sig"))
                    .map(|p| path(&p.file))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                let (prog, mut args) = crate::native_sets::install_args(*manager);
                if *manager == crate::manifest::NativeManager::Apt {
                    let cache = paths
                        .first()
                        .and_then(|p| p.parent())
                        .ok_or("Empty APT dependency set")?;
                    args.push("-o".into());
                    args.push(format!("Dir::Cache::archives={}/", cache.display()).into());
                }
                args.extend(paths.into_iter().map(OsString::from));
                go(prog, args)?;
                done.extend(requested.iter().cloned());
            }
        }
        Recipe::VendorRpm {
            profile,
            package,
            file,
        } => {
            if !succeeds("rpm", &["-q", package]) {
                let p = crate::distros::Distros::load_default()
                    .resolve(profile)
                    .map_err(|e| e.to_string())?;
                let zypper = p.package_manager == crate::distros::Adapter::Zypper;
                let (prog, mut args) = if zypper {
                    ("zypper", os(&["--non-interactive", "install"]))
                } else {
                    ("dnf", os(&["install", "-y"]))
                };
                args.push(path(file)?.into());
                go(prog, args)?;
                done.push(package.clone());
            }
        }
        Recipe::Flatpak { bundles, .. } => {
            for b in bundles {
                if succeeds("flatpak", &["info", &b.name]) {
                    continue;
                }
                status(format!("Installing {} with Flatpak", b.name));
                let mut a = os(&[
                    "install",
                    "--user",
                    "--noninteractive",
                    "--no-related",
                    "-y",
                    "--bundle",
                ]);
                a.push(path(&b.file)?.into());
                go("flatpak", a)?;
                done.push(b.name.clone());
            }
        }
        Recipe::Rpm { packages, .. } if packages.iter().all(|p| succeeds("rpm", &["-q", p])) => {}
        Recipe::Rpm { rpms, .. } => {
            let have: HashSet<String> = run(
                Command::new("rpm").args(["-qa", "--qf", "%{NAME}\\n"]),
                &NEVER,
            )
            .map_err(|e| e.to_string())?
            .lines()
            .map(String::from)
            .collect();
            // Packages already on this computer are left alone (never up- or downgraded).
            let need = rpms
                .iter()
                .filter(|p| !have.contains(&p.name))
                .map(|p| path(&p.file))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if !need.is_empty() {
                status(format!(
                    "Installing {} package(s) with dnf (administrator rights needed)",
                    need.len()
                ));
                let mut a = os(&[
                    "install",
                    "-y",
                    "--cacheonly",
                    "--disablerepo=*",
                    "--skip-broken",
                    "--setopt=install_weak_deps=False",
                ]);
                a.extend(need.iter().map(OsString::from));
                go("dnf", a)?;
                done.push(format!("{} package(s) with dnf", need.len()));
            }
        }
        Recipe::Deb { package, .. } if deb_installed(package) => {}
        Recipe::Deb { package, file } => {
            status(format!(
                "Installing {package} with APT (administrator rights needed)"
            ));
            // APT installs a file only when given a path, never a bare name.
            let mut a = os(&["install", "-y", "--no-install-recommends"]);
            a.push(path(file)?.into());
            go("apt-get", a)?;
            done.push(package.clone());
        }
        Recipe::Snap { classic, snaps } => {
            for (i, s) in snaps.iter().enumerate() {
                if succeeds("snap", &["list", &s.name]) {
                    continue;
                }
                status(format!(
                    "Installing snap {} (administrator rights needed)",
                    s.name
                ));
                let mut ack = os(&["ack"]);
                ack.push(path(&s.assertion)?.into());
                go("snap", ack)?;
                let mut a = os(&["install"]);
                if *classic && i + 1 == snaps.len() {
                    a.push("--classic".into());
                }
                a.push(path(&s.snap)?.into());
                go("snap", a)?;
                done.push(s.name.clone());
            }
        }
        Recipe::Archive {
            file,
            dir,
            program,
            link,
            top,
        } => {
            if !archive_installed(r, home) {
                let d = untilde(dir, home).map_err(|e| e.to_string())?;
                let parent = d.parent().unwrap_or(home);
                ensure_dirs(home, parent).map_err(|e| e.to_string())?;
                let name = d.file_name().map(|n| n.to_string_lossy().into_owned());
                let tmp = parent.join(format!(".{}.partial", name.unwrap_or_default()));
                let src = path(file)?;
                if !same_file(&src, file) {
                    return Err(format!("{} is damaged (checksum mismatch).", file.stored));
                }
                unpack(&src, &tmp, program, &NEVER).map_err(|e| e.to_string())?;
                let from = top.as_ref().map_or(tmp.clone(), |t| tmp.join(t));
                // A folder left by an earlier, interrupted restore of the same archive.
                if own_archive_dir(&d) {
                    fs::remove_dir_all(&d).map_err(|e| e.to_string())?;
                }
                let moved = fs::rename(&from, &d);
                let _ = fs::remove_dir_all(&tmp);
                moved.map_err(|e| format!("Installing into {dir}: {e}"))?;
                fs::write(d.join(ARCHIVE_MARK), &file.sha256).map_err(|e| e.to_string())?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = fs::set_permissions(d.join(program), fs::Permissions::from_mode(0o755));
                }
                done.push(format!(
                    "{} in {dir}",
                    file.stored.rsplit('/').next().unwrap_or("it")
                ));
            }
            if let Some(l) = link {
                done.push(link_program(home, dir, program, l));
            }
        }
        Recipe::Program {
            original,
            file,
            launcher,
        } => {
            let dest = untilde(original, home).map_err(|e| e.to_string())?;
            if !same_file(&dest, file) {
                ensure_dirs(home, dest.parent().unwrap_or(home)).map_err(|e| e.to_string())?;
                let entry = FileEntry {
                    root: String::new(),
                    rel: String::new(),
                    stored: file.stored.clone(),
                    size: file.size,
                    sha256: file.sha256.clone(),
                    mtime: None,
                    mode: Some(0o755),
                    stored_sha256: None,
                };
                restore_file(bundle, &entry, &dest, None, &NEVER)?;
                done.push(format!("the program at {original}"));
            }
            if let Some(name) = launcher {
                done.push(match write_launcher(home, name, &dest) {
                    Ok(true) => format!("{name} in the app menu"),
                    Ok(false) => format!(
                        "{name} in the app menu (this computer has no FUSE for AppImages, so it unpacks itself each time it starts; installing FUSE, e.g. the libfuse2 package, makes it start faster)"
                    ),
                    Err(e) => format!("no app-menu entry ({e})"),
                });
            }
        }
        Recipe::Windows {
            installer,
            file,
            product,
            silent,
        } => {
            if *installer == WinInstaller::Msix {
                if !msix_installed(product) {
                    status("Installing the app package".into());
                    msix_install(&path(file)?)?;
                    done.push(product[0].clone());
                }
            } else if !win_installed(product) {
                let (prog, args) = win_command(*installer, silent.as_deref(), &path(file)?)
                    .ok_or_else(|| why_or(by_hand(r)))?;
                status(format!(
                    "Running the {installer:?} installer (Windows may ask for permission)"
                ));
                restart = run_win_installer(&prog, &args)?;
                done.push(product[0].clone());
            }
        }
        Recipe::MacApp {
            container: MacContainer::Pkg,
            file,
            bundle_id,
            ..
        } => {
            if !bundle_id.as_deref().is_some_and(mac_bundle_present) {
                let pkg = path(file)?;
                done.push(mac_install_pkg(&pkg, elev, status)?);
                if let Some(id) = bundle_id
                    && !mac_bundle_present(id)
                {
                    return Err(format!(
                        "The installer finished, but no app with the bundle ID {id} is there."
                    ));
                }
            }
        }
        Recipe::MacApp {
            container,
            file,
            app,
            bundle_id,
        } => {
            let a = match (by_hand(r), app.as_deref()) {
                (Some(why), _) => return Err(why),
                (None, Some(a)) => a,
                (None, None) => {
                    return Err(format!(
                        "{} does not hold exactly one app; open it from the snapshot and drag the app into Applications.",
                        file.stored
                    ));
                }
            };
            if mac_find(a, bundle_id.as_deref(), home)?.is_none() {
                mac_install(*container, &path(file)?, a, home, status)?;
                done.push(a.to_string());
            }
        }
    }
    Ok(Done {
        message: if done.is_empty() {
            "It was already installed.".into()
        } else if restart {
            format!(
                "Installed {}. Restart the computer to finish.",
                done.join(", ")
            )
        } else {
            format!("Installed {}.", done.join(", "))
        },
        restart,
    })
}

/// Whether this computer can mount AppImages with FUSE at all (`/dev/fuse` and fusermount).
pub fn fuse_available() -> bool {
    Path::new("/dev/fuse").exists()
        && (which("fusermount3").is_some() || which("fusermount").is_some())
}

/// Whether this computer can mount the AppImage `exe` with FUSE: the older AppImage runtime
/// needs libfuse.so.2 (missing on stock Ubuntu, Debian and Fedora), the newer one fusermount.
fn fuse_ready(exe: &Path) -> bool {
    // ponytail: fixed library folders instead of asking ldconfig; covers Fedora, Debian/Ubuntu
    // and Arch.
    const LIBS: [&str; 6] = [
        "/usr/lib64",
        "/usr/lib",
        "/lib64",
        "/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
    ];
    let old = file_head(exe, 1 << 20).is_some_and(|h| h.windows(12).any(|w| w == b"libfuse.so.2"));
    Path::new("/dev/fuse").exists()
        && if old {
            LIBS.iter()
                .any(|d| Path::new(d).join("libfuse.so.2").exists())
        } else {
            which("fusermount3").is_some() || which("fusermount").is_some()
        }
}

/// Puts `name` in the app menu (a launcher in `~/.local/share/applications`), starting `exe`.
/// The launcher's file name comes from the app name, so a newer version replaces it. Without
/// FUSE the AppImage is started unpacked (both runtimes support `APPIMAGE_EXTRACT_AND_RUN`).
/// Returns whether FUSE is there.
fn write_launcher(home: &Path, name: &str, exe: &Path) -> std::result::Result<bool, String> {
    let fuse = fuse_ready(exe);
    let exe = exe
        .to_str()
        .filter(|s| !s.contains(['"', '`', '$', '\\', '%']))
        .ok_or("its path has characters a launcher cannot hold")?;
    let run = if fuse {
        format!("\"{exe}\"")
    } else {
        format!("env APPIMAGE_EXTRACT_AND_RUN=1 \"{exe}\"")
    };
    let dir = home.join(".local/share/applications");
    crate::restore::ensure_dirs(home, &dir).map_err(|e| e.to_string())?;
    let p = dir.join(format!("simple-setup-{}.desktop", portable_name(name)));
    // Never write through a link left at that name.
    let _ = fs::remove_file(&p);
    let text = format!(
        "[Desktop Entry]\nType=Application\nName={name}\nExec={run}\nIcon=application-x-executable\nTerminal=false\nCategories=Utility;\n"
    );
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&p)
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .map_err(|e| e.to_string())?;
    Ok(fuse)
}

/// A disk image from a GitHub release does not say which app it holds: on a Mac it is opened
/// read-only and its single app at the top is named in the recipe used to install and confirm.
pub fn name_dmg_app(bundle: &Path, r: &Recipe) -> Recipe {
    let mut r = r.clone();
    if let Recipe::MacApp {
        container: MacContainer::Dmg,
        file,
        app: app @ None,
        ..
    } = &mut r
        && cfg!(target_os = "macos")
        && let Ok(p) = join_rel(bundle, &file.stored, true)
    {
        *app = dmg_app(&p);
    }
    r
}

/// True only if the app is really present: the package manager lists it, or the program file
/// matches its checksum and is executable.
pub fn installed(r: &Recipe, home: &Path) -> bool {
    match r {
        Recipe::NativeSet {
            manager, requested, ..
        } => crate::native_sets::installed(*manager, requested),
        Recipe::VendorRpm { package, .. } => succeeds("rpm", &["-q", package]),
        Recipe::Flatpak { app_ref, .. } => succeeds("flatpak", &["info", app_ref]),
        Recipe::Rpm { packages, .. } => packages.iter().all(|p| succeeds("rpm", &["-q", p])),
        Recipe::Deb { package, .. } => deb_installed(package),
        Recipe::Snap { snaps, .. } => snaps
            .last()
            .is_some_and(|s| succeeds("snap", &["list", &s.name])),
        Recipe::Archive { .. } => archive_installed(r, home),
        Recipe::Program { original, file, .. } => untilde(original, home).is_ok_and(|d| {
            #[cfg(unix)]
            let exec = fs::metadata(&d).is_ok_and(|m| {
                std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o100 != 0
            });
            #[cfg(not(unix))]
            let exec = true;
            exec && same_file(&d, file)
        }),
        Recipe::Windows {
            product, installer, ..
        } => {
            win_installed(product) || (*installer == WinInstaller::Msix && msix_installed(product))
        }
        Recipe::MacApp { app, bundle_id, .. } => app
            .as_deref()
            .is_some_and(|a| matches!(mac_find(a, bundle_id.as_deref(), home), Ok(Some(_)))),
    }
}

pub(crate) fn wants_rpm(info: &SystemInfo) -> bool {
    info.profile().is_some_and(|p| {
        p.has_format(crate::distros::Format::Rpm)
            && matches!(
                p.package_manager,
                crate::distros::Adapter::Dnf | crate::distros::Adapter::Zypper
            )
    }) && !info.linux.as_ref().is_some_and(|l| l.immutable)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_application_repo_is_available_without_a_package_homepage() {
        let mut app = App::new(
            Source::Dpkg,
            "gh",
            "GitHub CLI",
            crate::discovery::Kind::CommandLine,
        );
        app.canonical = Some("github-cli".into());
        assert!(app.github.is_none());
        assert_eq!(github_repo(&app), Some("cli/cli"));
        app.canonical = None;
        assert_eq!(github_repo(&app), None);
    }

    #[test]
    fn flatpak_extensions_from_metadata() {
        let runtime = "[Runtime]\nname=org.freedesktop.Platform\n\n\
            [Extension org.freedesktop.Platform.GL]\nversions = 25.08;25.08-extra;1.4\nversion = 1.4\nsubdirectories = true\n\n\
            [Extension org.freedesktop.Platform.GL.Debug]\nversions = 25.08\nsubdirectories = true\n\n\
            [Extension org.freedesktop.Platform.Locale]\ndirectory = share/runtime/locale\n\n\
            [Extension org.freedesktop.Platform.codecs-extra]\nversion = 25.08-extra\n\n\
            [Extension org.freedesktop.Platform.Timezones]\ndirectory = share/zoneinfo\n";
        let installed: Vec<String> = [
            "org.freedesktop.Platform/x86_64/25.08",
            "org.freedesktop.Platform.GL.default/x86_64/25.08",
            "org.freedesktop.Platform.GL.default/x86_64/25.08-extra",
            "org.freedesktop.Platform.GL.default/aarch64/25.08",
            "org.freedesktop.Platform.GL.Debug.default/x86_64/25.08",
            "org.freedesktop.Platform.GLX/x86_64/25.08",
            "org.freedesktop.Platform.Locale/x86_64/25.08",
            "org.freedesktop.Platform.codecs-extra/x86_64/25.08-extra",
            "org.freedesktop.Platform.Timezones/x86_64/25.08",
            "org.gnome.Platform/x86_64/50",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(
            extension_refs(runtime, "x86_64", "25.08", &installed),
            vec![
                "org.freedesktop.Platform.GL.default/x86_64/25.08",
                "org.freedesktop.Platform.GL.default/x86_64/25.08-extra",
                "org.freedesktop.Platform.codecs-extra/x86_64/25.08-extra",
                "org.freedesktop.Platform.Timezones/x86_64/25.08",
            ]
        );
        let app = "[Application]\nname=com.spotify.Client\n[Extension com.spotify.Client.Debug]\ndirectory=lib/debug\n";
        assert!(extension_refs(app, "x86_64", "stable", &installed).is_empty());
    }

    #[test]
    fn snap_yaml() {
        let y = "name: hello\nbase: core22\nconfinement: strict\napps:\n  hello:\n    command: bin/hello\nplugs:\n  gnome-42-2204:\n    interface: content\n    default-provider: gnome-42-2204\n  gtk-3-themes:\n    default-provider: \"gtk-common-themes:gtk-3-themes\"\n";
        assert_eq!(
            parse_snap_yaml(y),
            (
                Some("core22".into()),
                vec!["gnome-42-2204".into(), "gtk-common-themes".into()],
                false
            )
        );
        assert!(parse_snap_yaml("name: code\nconfinement: classic\n").2);
    }

    #[test]
    fn elevation_wraps_only_admin_commands() {
        let c = admin_cmd("dnf", true, Elevation::Sudo);
        assert_eq!(c.get_program(), "sudo");
        assert_eq!(c.get_args().collect::<Vec<_>>(), ["--", "dnf"]);
        assert_eq!(
            admin_cmd("flatpak", false, Elevation::Pkexec).get_program(),
            "flatpak"
        );
        assert_eq!(admin_cmd("dnf", true, Elevation::None).get_program(), "dnf");
    }

    #[test]
    fn hostile_recipes_are_rejected() {
        let f = |stored: &str| StoredFile {
            stored: stored.into(),
            size: 1,
            sha256: "a".repeat(64),
        };
        let bad = [
            Recipe::Rpm {
                distro: "fedora 44".into(),
                packages: vec!["--setopt=reposdir=/tmp/evil".into()],
                rpms: vec![],
            },
            Recipe::Flatpak {
                app_ref: "app/x/x86_64/stable".into(),
                bundles: vec![Package {
                    name: "app/x/x86_64/stable".into(),
                    file: f("../outside.flatpak"),
                }],
            },
            Recipe::Program {
                original: "/usr/bin/sudo".into(),
                file: f("installers/a/sudo"),
                launcher: None,
            },
            Recipe::Program {
                original: "~/../../etc/x".into(),
                file: f("installers/a/x"),
                launcher: None,
            },
            // A launcher name that would add lines to the launcher file.
            Recipe::Program {
                original: "~/Applications/App.AppImage".into(),
                file: f("installers/a/App.AppImage"),
                launcher: Some("App\nExec=/bin/sh".into()),
            },
            Recipe::Snap {
                classic: false,
                snaps: vec![SnapFile {
                    name: "-x".into(),
                    snap: f("installers/s"),
                    assertion: f("installers/a"),
                }],
            },
        ];
        for r in bad {
            assert!(crate::manifest::check_recipe(&r).is_err(), "{r:?}");
        }
        let good = Recipe::Program {
            original: "~/Applications/App.AppImage".into(),
            file: f("installers/appimage-App/App.AppImage"),
            launcher: Some("App".into()),
        };
        assert!(crate::manifest::check_recipe(&good).is_ok());

        // An .exe runs unattended only with plain catalog switches.
        let exe = |silent: Option<&[&str]>| Recipe::Windows {
            installer: WinInstaller::Exe,
            file: f("installers/windows-x86_64/App/App-1.exe"),
            product: vec!["App".into()],
            silent: silent.map(|a| a.iter().map(|s| s.to_string()).collect()),
        };
        for hostile in [
            &["/S", "&", "calc"][..],
            &["\"/D=C:\\x\""],
            &["/LOG=<LOGPATH>"],
            &[""],
        ] {
            assert!(
                crate::manifest::check_recipe(&exe(Some(hostile))).is_err(),
                "{hostile:?}"
            );
        }
        let spotify = exe(Some(&["/silent", "/skip-app-launch"]));
        assert!(crate::manifest::check_recipe(&spotify).is_ok());
        assert!(unattended(&spotify) && by_hand(&spotify).is_none());
        assert!(unattended(&exe(Some(&[]))));
        let manual = exe(None);
        assert!(!unattended(&manual) && by_hand(&manual).is_some());
        assert_eq!(
            win_command(
                WinInstaller::Exe,
                Some(&["/silent".into()]),
                Path::new("a.exe")
            )
            .unwrap()
            .1,
            vec![OsString::from("/silent")]
        );
        assert!(win_command(WinInstaller::Exe, None, Path::new("a.exe")).is_none());
        // Old snapshots without the field still read.
        let old: Recipe = serde_json::from_str(r#"{"kind":"windows","installer":"exe","file":{"stored":"installers/w/a.exe","size":1,"sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},"product":["A"]}"#).unwrap();
        assert!(!unattended(&old));

        // A GitHub disk image is installed on the Mac even when its app is named only there;
        // a zip without one app and an installer package stay with the user.
        let mac = |container, app: Option<&str>| Recipe::MacApp {
            container,
            file: f("installers/macos-aarch64/App/App-1.dmg"),
            app: app.map(String::from),
            bundle_id: None,
        };
        assert!(
            unattended(&mac(MacContainer::Dmg, None))
                && by_hand(&mac(MacContainer::Dmg, None)).is_none()
        );
        assert!(by_hand(&mac(MacContainer::Zip, None)).is_some());
        // Installer packages run unattended (with the administrator password, signature checked).
        assert!(
            unattended(&mac(MacContainer::Pkg, Some("A.app")))
                && by_hand(&mac(MacContainer::Pkg, Some("A.app"))).is_none()
        );
        assert_eq!(
            check(
                &mac(MacContainer::Dmg, None),
                &SystemInfo::assumed(
                    Platform {
                        os: OsFamily::Macos,
                        arch: Arch::Aarch64
                    },
                    None
                ),
                Path::new("/nonexistent")
            ),
            Ok(false)
        );
    }

    /// A release's `.deb` names its package and processor in the control file.
    #[cfg(unix)]
    #[test]
    fn debian_package_control() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path();
        fs::write(
            dir.join("control"),
            "Package: foo\nVersion: 1.0\nArchitecture: amd64\nDescription: x\n",
        )
        .unwrap();
        run(
            Command::new("tar")
                .arg("-czf")
                .arg(dir.join("control.tar.gz"))
                .arg("-C")
                .arg(dir)
                .arg("./control"),
            &NEVER,
        )
        .unwrap();
        // An ar archive: the format version, the control member (odd length, so padded), data.
        let mut deb = b"!<arch>\n".to_vec();
        for (name, body) in [
            ("debian-binary", b"2.0\n\n".to_vec()),
            (
                "control.tar.gz",
                fs::read(dir.join("control.tar.gz")).unwrap(),
            ),
            ("data.tar.xz", vec![0; 3]),
        ] {
            deb.extend(
                format!(
                    "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    0,
                    0,
                    0,
                    100644,
                    body.len()
                )
                .bytes(),
            );
            deb.extend(&body);
            if body.len() % 2 == 1 {
                deb.push(b'\n');
            }
        }
        fs::write(dir.join("foo.deb"), &deb).unwrap();
        assert_eq!(
            deb_control(&dir.join("foo.deb"), dir).unwrap(),
            ("foo".to_string(), "amd64".to_string())
        );
        assert!(!dir.join(".deb-control").exists());
        fs::write(dir.join("x.deb"), b"\x7fELF").unwrap();
        assert!(deb_control(&dir.join("x.deb"), dir).is_err());

        let deb = |package: &str, stored: &str| Recipe::Deb {
            package: package.into(),
            file: StoredFile {
                stored: stored.into(),
                size: 1,
                sha256: "a".repeat(64),
            },
        };
        assert!(
            crate::manifest::check_recipe(&deb("foo", "installers/linux-x86_64/foo.deb")).is_ok()
        );
        for bad in [
            deb("-o", "installers/linux-x86_64/foo.deb"),
            deb("foo; rm", "installers/linux-x86_64/foo.deb"),
            deb("foo", "installers/linux-x86_64/foo.sh"),
        ] {
            assert!(crate::manifest::check_recipe(&bad).is_err(), "{bad:?}");
        }
    }

    /// An AppImage downloaded for the snapshot is put in the app menu when it is installed.
    #[cfg(unix)]
    #[test]
    fn appimage_gets_a_menu_entry() {
        let t = tempfile::tempdir().unwrap();
        let (bundle, home) = (t.path().join("snap"), t.path().join("home"));
        fs::create_dir_all(bundle.join("installers")).unwrap();
        fs::create_dir_all(&home).unwrap();
        let src = bundle.join("installers/App.AppImage");
        fs::write(&src, b"\x7fELF program").unwrap();
        let (size, sha256) = hash_file(&src, &NEVER, |_| {}).unwrap();
        let r = Recipe::Program {
            original: "~/Applications/App-1.AppImage".into(),
            file: StoredFile {
                stored: "installers/App.AppImage".into(),
                size,
                sha256,
            },
            launcher: Some("My App".into()),
        };
        let done = install(&bundle, &r, &home, Elevation::None, &mut |_| {}).unwrap();
        assert!(
            done.message.contains("My App in the app menu"),
            "{}",
            done.message
        );
        assert!(installed(&r, &home));
        let menu = home.join(".local/share/applications");
        let entries: Vec<_> = fs::read_dir(&menu).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);
        let text = fs::read_to_string(entries[0].path()).unwrap();
        assert!(text.contains("\nName=My App\n"), "{text}");
        let exe = home.join("Applications/App-1.AppImage");
        // Started directly, or unpacked when this computer has no FUSE.
        let direct = format!("\nExec=\"{}\"\n", exe.display());
        let unpacked = format!(
            "\nExec=env APPIMAGE_EXTRACT_AND_RUN=1 \"{}\"\n",
            exe.display()
        );
        assert!(text.contains(&direct) || text.contains(&unpacked), "{text}");
        // Installing again (a newer version) replaces the entry rather than adding one.
        install(&bundle, &r, &home, Elevation::None, &mut |_| {}).unwrap();
        assert_eq!(fs::read_dir(&menu).unwrap().count(), 1);
    }

    #[test]
    fn elf_programs_match_the_processor() {
        let t = tempfile::tempdir().unwrap();
        let elf = |machine: u16| {
            let mut b = b"\x7fELF\x02\x01\x01".to_vec();
            b.resize(18, 0);
            b.extend(machine.to_le_bytes());
            b.resize(64, 0);
            b
        };
        let linux = |arch| Platform {
            os: OsFamily::Linux,
            arch,
        };
        for (name, bytes, want) in [
            ("x64", elf(0x3e), Some(Arch::X86_64)),
            ("arm64", elf(0xb7), Some(Arch::Aarch64)),
            ("armv7", elf(0x28), None),
            ("script", b"#!/bin/sh\necho hi\n".to_vec(), None),
        ] {
            let p = t.path().join(name);
            fs::write(&p, bytes).unwrap();
            assert_eq!(elf_arch(&p), want, "{name}");
            assert_eq!(
                is_program(&p, linux(Arch::Aarch64)),
                want == Some(Arch::Aarch64),
                "{name}"
            );
        }
    }

    /// Regression: archives were unpacked without looking at their entries.
    #[test]
    fn hostile_archives_are_refused() {
        use crate::catalog::zip_entry_problem;
        assert!(zip_entry_problem("rg-14/rg", 0o100755).is_none());
        assert!(zip_entry_problem("../.bashrc", 0o100644).is_some());
        assert!(zip_entry_problem("/etc/cron.d/x", 0).is_some());
        assert!(zip_entry_problem("C:/Windows/x.dll", 0).is_some());
        assert!(zip_entry_problem("a\\..\\b", 0).is_some());
        assert!(zip_entry_problem("link", 0o120777).is_some());

        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("out");
        fs::create_dir_all(root.join("app/lib")).unwrap();
        fs::write(root.join("app/bin"), "x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink("../bin", root.join("app/lib/inside")).unwrap();
            assert_eq!(escaping_link(&root), None);
            symlink("../../../etc", root.join("app/lib/out")).unwrap();
            assert!(escaping_link(&root).is_some());
            fs::remove_file(root.join("app/lib/out")).unwrap();
            symlink("/etc/passwd", root.join("abs")).unwrap();
            assert!(escaping_link(&root).is_some());
        }
        assert!(is_library(Path::new("lib/libfoo.so.6")));
        assert!(is_library(Path::new("x/Qt6Core.DLL")));
        assert!(!is_library(Path::new("bin/solver")));
    }

    /// Regression: bare program `.exe` assets were treated as installers.
    #[test]
    fn portable_exe_names() {
        assert!(portable_exe("jq-windows-amd64.exe"));
        assert!(portable_exe("App-Portable-Setup.exe"));
        assert!(!portable_exe("Simple.Cell.Setup.1.2.0.exe"));
        assert!(!portable_exe("app-installer-x64.exe"));
        assert!(!portable_exe("app.msi"));
    }

    /// Regression: registered links were always Windows installers with no file name.
    #[test]
    fn registered_links_are_classified() {
        let d = |url: &str, target| crate::discovery::Download {
            target,
            url: url.into(),
            sha256: "a".repeat(64),
        };
        let linux = Platform {
            os: OsFamily::Linux,
            arch: Arch::X86_64,
        };
        let win = Platform {
            os: OsFamily::Windows,
            arch: Arch::X86_64,
        };
        let p = registered_pick(
            &d("https://x.org/dl/tool-1.0-x86_64.tar.gz?a=1", linux),
            linux,
            None,
        );
        assert_eq!(p.name, "tool-1.0-x86_64.tar.gz");
        assert!(p.archive && p.win.is_none());
        let p = registered_pick(&d("https://x.org/app.rpm", linux), linux, None);
        assert!(!p.archive && p.win.is_none());
        let p = registered_pick(&d("https://x.org/App.MSI", win), win, Some("2"));
        assert_eq!(p.win, Some(WinInstaller::Msi));
        assert_eq!(p.version, "2");
        let p = registered_pick(&d("https://x.org/app.zip", win), win, None);
        assert!(p.archive && p.win.is_none());
    }

    /// A program with its own libraries is installed as a folder: unpacked from the checked
    /// archive, linked into ~/.local/bin, recognised afterwards, never over someone else's folder.
    #[cfg(unix)]
    #[test]
    fn archive_installs_as_a_folder() {
        let t = tempfile::tempdir().unwrap();
        let (bundle, home) = (t.path().join("snap"), t.path().join("home"));
        let src = t.path().join("src/tool-1.0");
        fs::create_dir_all(src.join("lib")).unwrap();
        fs::create_dir_all(bundle.join("installers/linux-x86_64/Tool")).unwrap();
        fs::create_dir_all(&home).unwrap();
        fs::write(src.join("tool"), "#!/bin/sh\necho tool\n").unwrap();
        fs::write(src.join("lib/libtool.so.1"), "lib").unwrap();
        let rel = "installers/linux-x86_64/Tool/tool-1.0.tar.gz";
        let archive = bundle.join(rel);
        assert!(
            Command::new("tar")
                .arg("-czf")
                .arg(&archive)
                .arg("-C")
                .arg(t.path().join("src"))
                .arg("tool-1.0")
                .status()
                .unwrap()
                .success()
        );
        let (size, sha256) = hash_file(&archive, &NEVER, |_| {}).unwrap();
        let r = Recipe::Archive {
            file: StoredFile {
                stored: rel.into(),
                size,
                sha256,
            },
            dir: "~/.local/opt/Tool".into(),
            program: "tool".into(),
            link: Some("~/.local/bin/tool".into()),
            top: Some("tool-1.0".into()),
        };
        crate::manifest::check_recipe(&r).unwrap();
        assert!(!installed(&r, &home));
        install(&bundle, &r, &home, Elevation::None, &mut |_| {}).unwrap();
        assert!(installed(&r, &home));
        assert!(home.join(".local/opt/Tool/lib/libtool.so.1").is_file());
        let link = fs::read_link(home.join(".local/bin/tool")).unwrap();
        assert_eq!(link, home.join(".local/opt/Tool/tool"));
        // Again: nothing to do.
        install(&bundle, &r, &home, Elevation::None, &mut |_| {}).unwrap();
        // Someone else's folder is left alone.
        let other = tempfile::tempdir().unwrap();
        fs::create_dir_all(other.path().join(".local/opt/Tool")).unwrap();
        let info = crate::probe::SystemInfo::assumed(
            Platform {
                os: OsFamily::Linux,
                arch: Arch::X86_64,
            },
            None,
        );
        assert!(check(&r, &info, other.path()).is_err());
        // Paths outside the home folder are refused.
        let mut bad = r.clone();
        if let Recipe::Archive { dir, .. } = &mut bad {
            *dir = "~/../etc".into();
        }
        assert!(crate::manifest::check_recipe(&bad).is_err());
    }

    /// Regression (found on a real Ubuntu runner): a snapshot given as a relative folder made
    /// `apt-get install migration/installers/…/x.deb` look for a package of that name.
    #[test]
    fn installer_paths_are_absolute() {
        let f = StoredFile {
            stored: "installers/linux-x86_64/rg/rg.deb".into(),
            size: 1,
            sha256: "a".repeat(64),
        };
        let p = installer_path(Path::new("migration"), &f).unwrap();
        assert!(p.is_absolute(), "{}", p.display());
        assert!(p.ends_with("migration/installers/linux-x86_64/rg/rg.deb"));
    }
}
