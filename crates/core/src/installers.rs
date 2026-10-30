//! Offline installers. While a snapshot is built, each selected app's installer is collected
//! into it: Flatpak bundles are made from this computer's Flatpak repository, RPM dependency
//! sets and snaps are downloaded from their usual sources, and self-contained program files are
//! copied. On the new computer they are installed from the snapshot alone, with fixed argument
//! lists per recipe type; nothing in the manifest is ever run as a command.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
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
use crate::platform::{Arch, OsFamily, Platform};
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
    let name = cmd.get_program().to_string_lossy().into_owned();
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
    if status.success() {
        Ok(out)
    } else {
        let why = last_lines(if err.trim().is_empty() { &out } else { &err }, 3);
        Err(invalid(format!("{name} failed: {why}")))
    }
}

fn succeeds(prog: &str, args: &[&str]) -> bool {
    Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub fn which(prog: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
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

/// What would be bundled for `app` on `target` when the snapshot is built on `source`, or why
/// nothing can be (and what to do instead). Reads only metadata; never uses the network.
pub fn plan(
    app: &App,
    home: &Path,
    source: Platform,
    target: Platform,
) -> std::result::Result<String, String> {
    if let Some(d) = app.downloads.iter().find(|d| d.target == target) {
        let host = d
            .url
            .trim_start_matches("https://")
            .split('/')
            .next()
            .unwrap_or_default();
        return Ok(format!(
            "the download you registered (from {host}), checked against the SHA-256 you entered"
        ));
    }
    if target == source {
        return local_plan(app, home)
            .or_else(|why| remote_plan(app, source, target).map_err(|_| why));
    }
    remote_plan(app, source, target)
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
        Source::Dpkg | Source::Pacman => Err(format!(
            "Offline {} packages are not collected yet. On the new computer, install {} with the system's package manager.",
            if app.source == Source::Dpkg {
                "APT"
            } else {
                "Pacman"
            },
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
        | Source::Macports => Err(format!(
            "No offline installer is known for {name}. On the new computer, {}.",
            reinstall(app)
        )),
    }
}

fn winget_id(app: &App) -> Option<String> {
    let e = app.canonical.as_deref().and_then(registry::get)?;
    e.packages.get("winget")?.first().cloned()
}

fn cask_token(app: &App) -> Option<String> {
    if app.source == Source::BrewCask {
        return Some(app.native_id.clone());
    }
    let e = app.canonical.as_deref().and_then(registry::get)?;
    e.packages.get("brew_cask")?.first().cloned()
}

/// `owner/name` with only the characters GitHub allows.
fn github_repo(app: &App) -> Option<&str> {
    let r = app.github.as_ref()?.repo.as_str();
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
fn remote_plan(
    app: &App,
    source: Platform,
    target: Platform,
) -> std::result::Result<String, String> {
    let name = &app.name;
    let store = matches!(app.source, Source::Msix | Source::AppBundle) && app.repository.is_some();
    if store && registry::method_os(source_key(app.source)) == Some(target.os) {
        return Err(format!(
            "{name} came from the {}; install it from there on the new computer (it is tied to your account).",
            app.repository.as_deref().unwrap_or_default()
        ));
    }
    let github = |what: &str| {
        github_repo(app).map(|r| {
            format!("{what} from the latest release of github.com/{r}, checked against the SHA-256 GitHub publishes")
        })
    };
    let found = match target.os {
        OsFamily::Windows => winget_id(app)
            .map(|id| format!("the Windows installer listed in the winget catalog as {id}, checked against its published SHA-256"))
            .or_else(|| github("a Windows installer")),
        OsFamily::Macos => cask_token(app)
            .map(|t| format!("the macOS download of Homebrew cask {t}, checked against its published SHA-256"))
            .or_else(|| github("a macOS disk image")),
        OsFamily::Linux => github("an AppImage"),
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
                Source::Rpm | Source::Dpkg | Source::Pacman => {
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
}

fn local(staging: &Path, rel: &str) -> PathBuf {
    staging.join(rel.split('/').collect::<PathBuf>())
}

fn portable_name(s: &str) -> String {
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

fn stored(c: &Collect, rel: String) -> Result<StoredFile> {
    let (size, sha256) = hash_file(&local(c.staging, &rel), c.cancel, |_| {})?;
    Ok(StoredFile {
        stored: rel,
        size,
        sha256,
    })
}

fn fresh_dir(dir: &Path) -> Result<()> {
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
    if !registered && target == source && local_plan(app, c.home).is_ok() {
        return collect_local(app, c);
    }
    collect_remote(app, target, c)
}

fn collect_local(app: &App, c: &mut Collect) -> Result<(Recipe, Vec<String>)> {
    if !plain_name(&app.native_id)
        && matches!(app.source, Source::Flatpak | Source::Rpm | Source::Snap)
    {
        return Err(invalid(format!(
            "Unexpected package name {:?}.",
            app.native_id
        )));
    }
    match app.source {
        Source::Flatpak => collect_flatpak(app, c),
        Source::Rpm => collect_rpm(app, c),
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

fn github_pick(repo: &str, target: Platform, c: &mut Collect) -> Result<(Pick, String)> {
    (c.status)(format!(
        "Looking up the latest release of github.com/{repo}"
    ));
    let rel: catalog::Release = fetch::json(
        &format!("https://api.github.com/repos/{repo}/releases/latest"),
        Some(GITHUB_API),
        c.cancel,
    )?;
    let pick = catalog::github_asset(&rel, target).map_err(invalid)?;
    Ok((pick, format!("GitHub release {} of {repo}", rel.tag_name)))
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

fn collect_remote(app: &App, target: Platform, c: &mut Collect) -> Result<(Recipe, Vec<String>)> {
    let entry = app.canonical.as_deref().and_then(registry::get);
    let catalog_id = match target.os {
        OsFamily::Windows => winget_id(app),
        OsFamily::Macos => cask_token(app),
        OsFamily::Linux => None,
    };
    let registered = app.downloads.iter().find(|d| d.target == target);
    let mut fallback = None;
    let (pick, app_bundle, origin) = match (target.os, catalog_id) {
        _ if let Some(d) = registered => {
            d.check()?;
            let msi = d
                .url
                .split(['?', '#'])
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase()
                .ends_with(".msi");
            let pick = Pick {
                url: d.url.clone(),
                sha256: d.sha256.to_ascii_lowercase(),
                version: app.version.clone().unwrap_or_else(|| "download".into()),
                win: Some(if msi {
                    WinInstaller::Msi
                } else {
                    WinInstaller::Exe
                }),
                detail: "link you registered".into(),
            };
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
                    let (p, o) = github_pick(repo, target, c)?;
                    (p, None, o)
                }
                (Err(e), None) => return Err(e),
            }
        }
        _ => {
            let repo = github_repo(app)
                .ok_or_else(|| invalid(format!("No catalog lists {} for {target}.", app.name)))?;
            let (p, o) = github_pick(repo, target, c)?;
            (p, None, o)
        }
    };
    let ext = match (target.os, pick.win) {
        (OsFamily::Windows, Some(WinInstaller::Msi)) => "msi",
        (OsFamily::Windows, Some(WinInstaller::Msix)) => "msix",
        (OsFamily::Windows, _) => "exe",
        (OsFamily::Macos, _) => "download",
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
    fetch::download(&pick.url, &dst, &pick.sha256, c.cancel)?;
    if target.os == OsFamily::Linux && !is_elf(&dst) {
        let _ = fs::remove_file(&dst);
        return Err(invalid(
            "The Linux download is not a self-contained program (AppImage).",
        ));
    }
    let mut evidence: Vec<String> = fallback.into_iter().collect();
    evidence.extend([
        format!("Downloaded from {} ({origin}, {}).", pick.url, pick.detail),
        if registered.is_some() {
            "The file matched the SHA-256 checksum you entered.".into()
        } else {
            "The file matched the SHA-256 checksum the catalog publishes.".into()
        },
    ]);
    let recipe = match target.os {
        OsFamily::Windows => {
            let mut product: Vec<String> = entry
                .and_then(|e| e.packages.get("arp").cloned())
                .unwrap_or_default();
            if product.is_empty() {
                product.push(app.name.clone());
            }
            Recipe::Windows {
                installer: pick.win.unwrap_or(WinInstaller::Exe),
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
            Recipe::MacApp {
                container,
                file: stored(c, rel)?,
                app: app_bundle.filter(|_| container != MacContainer::Pkg),
                bundle_id,
            }
        }
        OsFamily::Linux => Recipe::Program {
            original: format!("~/Applications/{stem}.AppImage"),
            file: stored(c, rel)?,
        },
    };
    crate::manifest::check_recipe(&recipe)?;
    Ok((recipe, evidence))
}

/// True when restore can install the recipe by itself; otherwise the bundled file waits for the
/// user (see [`check`] for the instructions).
pub fn unattended(r: &Recipe) -> bool {
    match r {
        Recipe::Windows { installer, .. } => {
            !matches!(installer, WinInstaller::Exe | WinInstaller::Msix)
        }
        Recipe::MacApp { container, app, .. } => *container != MacContainer::Pkg && app.is_some(),
        _ => true,
    }
}

/// What the user does with a bundled installer restore cannot run by itself.
pub fn by_hand(r: &Recipe) -> Option<String> {
    let f = &r.files().first()?.stored;
    match r {
        Recipe::Windows {
            installer: WinInstaller::Msix,
            ..
        } => Some(format!(
            "Open {f} from the snapshot to install it (no internet needed)."
        )),
        Recipe::Windows {
            installer: WinInstaller::Exe,
            ..
        } => Some(format!(
            "Run {f} from the snapshot (no internet needed); it has no known unattended mode, so it is not started for you."
        )),
        Recipe::MacApp {
            container: MacContainer::Pkg,
            ..
        } => Some(format!(
            "Open {f} from the snapshot to install it (no internet needed; macOS asks for an administrator password)."
        )),
        Recipe::MacApp { app: None, .. } => Some(format!(
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
    matches!(r, Recipe::Rpm { .. } | Recipe::Snap { .. })
}

fn admin_cmd(prog: &str, admin: bool, elev: Elevation) -> Command {
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

/// The installed-programs list has an entry for one of `product`'s names.
fn win_installed(product: &[String]) -> bool {
    crate::discovery::windows::read_uninstall_entries().is_ok_and(|es| {
        es.iter()
            .any(|e| product.iter().any(|p| registry::arp_match(&e.name, p)))
    })
}

/// The fixed program and arguments for each unattended installer type.
fn win_command(kind: WinInstaller, file: &Path) -> Option<(OsString, Vec<OsString>)> {
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
        WinInstaller::Exe | WinInstaller::Msix => return None,
    })
}

fn run_win_installer(prog: &OsString, args: &[OsString]) -> std::result::Result<(), String> {
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
        Some(0 | 3010 | 1641) => Ok(()),
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

/// Whether this computer can use the recipe: `Ok(already installed)`, or why not.
/// Runs only read-only queries.
pub fn check(r: &Recipe, distro: Option<&str>, home: &Path) -> std::result::Result<bool, String> {
    match r {
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
            if distro != Some(d.as_str()) {
                return Err(format!(
                    "The packages were collected for {d}; this computer runs {}. Install {} from this system's software source instead.",
                    distro.unwrap_or("another system"),
                    packages.join(", ")
                ));
            }
            Ok(packages.iter().all(|p| succeeds("rpm", &["-q", p])))
        }
        Recipe::Snap { snaps, .. } => {
            which("snap").ok_or("snapd is not installed on this computer.")?;
            Ok(snaps
                .last()
                .is_some_and(|s| succeeds("snap", &["list", &s.name])))
        }
        Recipe::Program { original, file } => {
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
        Recipe::Windows { product, .. } => {
            if win_installed(product) {
                Ok(true)
            } else {
                by_hand(r).map_or(Ok(false), Err)
            }
        }
        Recipe::MacApp { app, bundle_id, .. } => match (by_hand(r), app) {
            (None, Some(a)) => mac_find(a, bundle_id.as_deref(), home).map(|p| p.is_some()),
            (why, _) => Err(why_or(why)),
        },
    }
}

fn why_or(s: Option<String>) -> String {
    s.unwrap_or_else(|| "Install it by hand from the snapshot.".into())
}

/// Checks every bundled file, then installs. Success here only means the commands finished;
/// call [`installed`] to confirm the app is really there.
pub fn install(
    bundle: &Path,
    r: &Recipe,
    home: &Path,
    elev: Elevation,
    status: &mut dyn FnMut(String),
) -> std::result::Result<String, String> {
    let path = |f: &StoredFile| join_rel(bundle, &f.stored, true).map_err(|e| e.to_string());
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
        Recipe::Program { original, file } => {
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
        }
        Recipe::Windows {
            installer,
            file,
            product,
        } => {
            if !win_installed(product) {
                let (prog, args) =
                    win_command(*installer, &path(file)?).ok_or_else(|| why_or(by_hand(r)))?;
                status(format!(
                    "Running the {installer:?} installer (Windows may ask for permission)"
                ));
                run_win_installer(&prog, &args)?;
                done.push(product[0].clone());
            }
        }
        Recipe::MacApp {
            container,
            file,
            app,
            bundle_id,
        } => {
            let a = app
                .as_deref()
                .filter(|_| by_hand(r).is_none())
                .ok_or_else(|| why_or(by_hand(r)))?;
            if mac_find(a, bundle_id.as_deref(), home)?.is_none() {
                mac_install(*container, &path(file)?, a, home, status)?;
                done.push(a.to_string());
            }
        }
    }
    Ok(if done.is_empty() {
        "It was already installed.".into()
    } else {
        format!("Installed {}.", done.join(", "))
    })
}

/// True only if the app is really present: the package manager lists it, or the program file
/// matches its checksum and is executable.
pub fn installed(r: &Recipe, home: &Path) -> bool {
    match r {
        Recipe::Flatpak { app_ref, .. } => succeeds("flatpak", &["info", app_ref]),
        Recipe::Rpm { packages, .. } => packages.iter().all(|p| succeeds("rpm", &["-q", p])),
        Recipe::Snap { snaps, .. } => snaps
            .last()
            .is_some_and(|s| succeeds("snap", &["list", &s.name])),
        Recipe::Program { original, file } => untilde(original, home).is_ok_and(|d| {
            #[cfg(unix)]
            let exec = fs::metadata(&d).is_ok_and(|m| {
                std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o100 != 0
            });
            #[cfg(not(unix))]
            let exec = true;
            exec && same_file(&d, file)
        }),
        Recipe::Windows { product, .. } => win_installed(product),
        Recipe::MacApp { app, bundle_id, .. } => app
            .as_deref()
            .is_some_and(|a| matches!(mac_find(a, bundle_id.as_deref(), home), Ok(Some(_)))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            },
            Recipe::Program {
                original: "~/../../etc/x".into(),
                file: f("installers/a/x"),
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
        };
        assert!(crate::manifest::check_recipe(&good).is_ok());
    }
}
