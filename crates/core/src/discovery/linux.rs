//! Linux adapters: Flatpak, RPM/DNF, DPKG/APT, Pacman, Snap, AppImage and desktop shortcuts.
//! Each `parse_*` function is pure and fixture-tested; the scan functions only run read-only
//! listing commands (no root, no network).

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::{
    App, Confidence, Ctx, GitHub, Kind, LocKind, Origin, ScanOptions, Source, git, output, run,
};
use crate::registry;

pub fn scan(cx: &mut Ctx, opts: &ScanOptions) {
    flatpak(cx);
    snap(cx);
    let shortcuts = desktop_entries(cx.home);
    let owned = native(cx, &shortcuts);
    unowned_shortcuts(cx, &shortcuts, &owned);
    appimages(cx, opts);
}

// ---------------------------------------------------------------- Flatpak

#[derive(Debug, PartialEq)]
pub struct FlatpakRow {
    pub id: String,
    pub name: String,
    pub version: String,
    pub branch: String,
    pub origin: String,
    pub installation: String,
    pub arch: String,
    pub size: Option<u64>,
    pub runtime: String,
}

pub const FLATPAK_COLUMNS: &str =
    "application,name,version,branch,origin,installation,arch,size,runtime";

pub fn parse_flatpak(text: &str) -> Vec<FlatpakRow> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').map(str::trim).collect();
            (f.len() >= 9 && !f[0].is_empty()).then(|| FlatpakRow {
                id: f[0].into(),
                name: if f[1].is_empty() {
                    f[0].into()
                } else {
                    f[1].into()
                },
                version: f[2].into(),
                branch: f[3].into(),
                origin: f[4].into(),
                installation: f[5].into(),
                arch: f[6].into(),
                size: parse_human_size(f[7]),
                runtime: f[8].into(),
            })
        })
        .collect()
}

/// "525.6 MB" / "2.4 kB" (GLib SI units) → bytes.
pub fn parse_human_size(s: &str) -> Option<u64> {
    let s = s.replace('\u{a0}', " ");
    let (num, unit) = s.trim().split_once(' ')?;
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "bytes" | "B" => 1.0,
        "kB" | "KB" => 1e3,
        "MB" => 1e6,
        "GB" => 1e9,
        "TB" => 1e12,
        _ => return None,
    };
    Some((n * mult) as u64)
}

fn flatpak(cx: &mut Ctx) {
    let Some(text) = run(
        "flatpak",
        &["list", "--app", &format!("--columns={FLATPAK_COLUMNS}")],
    ) else {
        return;
    };
    for r in parse_flatpak(&text) {
        let mut app = App::new(Source::Flatpak, &r.id, &r.name, Kind::Desktop);
        app.version = (!r.version.is_empty()).then(|| r.version.clone());
        app.repository = Some(r.origin.clone());
        app.install_size = r.size;
        // Sandbox home: `config` holds settings; everything else except caches is data
        // (some apps keep profiles in dot-folders here, e.g. `.thunderbird`).
        let base = cx.home.join(".var/app").join(&r.id);
        app.add_location(LocKind::Config, base.join("config"), Origin::Detected);
        let mut data: Vec<PathBuf> = std::fs::read_dir(&base)
            .map(|rd| rd.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        data.sort();
        for p in data {
            let n = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if !matches!(n.as_str(), "config" | "cache" | ".ld.so") {
                app.add_location(LocKind::Data, p, Origin::Detected);
            }
        }
        app.evidence.push(format!(
            "Flatpak app/{}/{}/{} from remote \"{}\" ({} installation)",
            r.id, r.arch, r.branch, r.origin, r.installation
        ));
        if !r.runtime.is_empty() {
            app.evidence
                .push(format!("Requires Flatpak runtime {}", r.runtime));
        }
        let installation = match r.installation.as_str() {
            "user" => cx.home.join(".local/share/flatpak"),
            _ => PathBuf::from("/var/lib/flatpak"),
        };
        let meta = installation
            .join("app")
            .join(&r.id)
            .join("current/active/files/share/metainfo");
        let found = ["metainfo.xml", "appdata.xml"]
            .iter()
            .find_map(|ext| std::fs::read_to_string(meta.join(format!("{}.{ext}", r.id))).ok())
            .and_then(|xml| metainfo_github(&xml));
        if let Some((repo, kind)) = found {
            app.evidence.push(format!(
                "The app's AppStream metadata links github.com/{repo} ({kind})"
            ));
            app.github = Some(GitHub {
                repo,
                reference: None,
                asset: None,
                confidence: Confidence::Probable,
                origin: Origin::Detected,
            });
        }
        cx.apps.push(app);
    }
}

// ---------------------------------------------------------------- Snap

#[derive(Debug, PartialEq)]
pub struct SnapRow {
    pub name: String,
    pub version: String,
    pub rev: String,
    pub tracking: String,
    pub notes: String,
}

pub fn parse_snap_list(text: &str) -> Vec<SnapRow> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 6).then(|| SnapRow {
                name: f[0].into(),
                version: f[1].into(),
                rev: f[2].into(),
                tracking: f[3].into(),
                notes: f[5].into(),
            })
        })
        .collect()
}

fn snap(cx: &mut Ctx) {
    let Some(text) = run("snap", &["list"]) else {
        return;
    };
    for r in parse_snap_list(&text) {
        let is_base = r
            .notes
            .split(',')
            .any(|n| matches!(n, "base" | "core" | "snapd"));
        let yaml = std::fs::read_to_string(format!("/snap/{}/current/meta/snap.yaml", r.name))
            .unwrap_or_default();
        let has_shortcut = std::fs::read_dir("/var/lib/snapd/desktop/applications")
            .map(|d| {
                d.flatten().any(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{}_", r.name))
                })
            })
            .unwrap_or(false);
        // Content providers (GPU drivers, themes, SDKs) exist to serve other snaps.
        let provider = !has_shortcut && yaml.contains("interface: content");
        if is_base || provider || !yaml.lines().any(|l| l.starts_with("apps:")) {
            cx.hidden += 1;
            continue;
        }
        let kind = if has_shortcut {
            Kind::Desktop
        } else {
            Kind::CommandLine
        };
        let mut app = App::new(Source::Snap, &r.name, &r.name, kind);
        app.version = Some(r.version.clone());
        app.repository = Some(format!("snap store ({})", r.tracking));
        app.install_size =
            std::fs::metadata(format!("/var/lib/snapd/snaps/{}_{}.snap", r.name, r.rev))
                .ok()
                .map(|m| m.len());
        app.add_location(
            LocKind::Data,
            cx.home.join("snap").join(&r.name),
            Origin::Detected,
        );
        app.evidence.push(format!(
            "snap list: {} revision {} tracking {}",
            r.name, r.rev, r.tracking
        ));
        cx.apps.push(app);
    }
}

// ---------------------------------------------------------------- Desktop shortcuts

#[derive(Debug, Default, PartialEq)]
pub struct DesktopEntry {
    pub name: String,
    pub exec: Option<String>,
    pub hidden: bool,
    pub flatpak: Option<String>,
    pub snap: bool,
}

pub fn parse_desktop(text: &str) -> Option<DesktopEntry> {
    let (mut main, mut app) = (false, false);
    let mut e = DesktopEntry::default();
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            main = line == "[Desktop Entry]";
            continue;
        }
        let Some((k, v)) = line.split_once('=').filter(|_| main) else {
            continue;
        };
        let v = v.trim();
        match k.trim() {
            "Type" => app = v == "Application",
            "Name" => e.name = v.into(),
            "Exec" => e.exec = Some(v.into()),
            "NoDisplay" | "Hidden" => e.hidden |= v == "true",
            "X-Flatpak" => e.flatpak = Some(v.into()),
            "X-SnapInstanceName" => e.snap = true,
            _ => {}
        }
    }
    (app && !e.name.is_empty()).then_some(e)
}

/// The program an `Exec=` line starts: first word (desktop-entry quoting), skipping `env VAR=x`.
pub fn exec_program(exec: &str) -> Option<String> {
    let mut words = Vec::new();
    let (mut cur, mut quoted, mut esc, mut started) = (String::new(), false, false, false);
    for c in exec.chars() {
        match c {
            _ if esc => {
                cur.push(c);
                esc = false;
            }
            '\\' if quoted => esc = true,
            '"' => {
                quoted = !quoted;
                started = true;
            }
            ' ' | '\t' if !quoted => {
                if started || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                started = false;
            }
            _ => cur.push(c),
        }
    }
    if started || !cur.is_empty() {
        words.push(cur);
    }
    let mut it = words.into_iter().peekable();
    if it.peek().is_some_and(|w| w == "env" || w.ends_with("/env")) {
        it.next();
        while it
            .peek()
            .is_some_and(|w| w.contains('=') && !w.starts_with('/'))
        {
            it.next();
        }
    }
    it.next().filter(|w| !w.starts_with('%'))
}

pub struct Shortcut {
    pub file: PathBuf,
    pub entry: DesktopEntry,
}

fn desktop_entries(home: &Path) -> Vec<Shortcut> {
    let dirs = [
        PathBuf::from("/usr/share/applications"),
        PathBuf::from("/usr/local/share/applications"),
        home.join(".local/share/applications"),
    ];
    let mut out = Vec::new();
    for dir in dirs {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "desktop"))
            .collect();
        files.sort();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            if let Some(entry) =
                parse_desktop(&text).filter(|e| !e.hidden && e.flatpak.is_none() && !e.snap)
            {
                out.push(Shortcut { file, entry });
            }
        }
    }
    out
}

// ---------------------------------------------------------------- Native package managers

#[derive(Debug, Clone, PartialEq)]
pub struct Pkg {
    pub name: String,
    pub version: String,
    pub arch: String,
    pub size: Option<u64>,
}

/// `rpm -qa --qf RPM_QF`
pub const RPM_QF: &str = "%{NAME}\\t%{EPOCHNUM}\\t%{VERSION}-%{RELEASE}\\t%{ARCH}\\t%{SIZE}\\n";

pub fn parse_rpm_qa(text: &str) -> Vec<Pkg> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 5).then(|| Pkg {
                name: f[0].into(),
                version: if f[1] == "0" {
                    f[2].into()
                } else {
                    format!("{}:{}", f[1], f[2])
                },
                arch: f[3].into(),
                size: f[4].parse().ok(),
            })
        })
        .collect()
}

/// `dnf repoquery --installed --qf '%{name}|%{reason}|%{from_repo}\n'` → name → (reason, repo).
pub fn parse_dnf_reasons(text: &str) -> HashMap<String, (String, String)> {
    text.lines()
        .filter_map(|l| {
            let mut f = l.splitn(3, '|');
            let (n, r) = (f.next()?, f.next()?);
            Some((
                n.into(),
                (r.trim().into(), f.next().unwrap_or("").trim().into()),
            ))
        })
        .collect()
}

/// `rpm -qf --qf '[%{FILENAMES}\t%{=NAME}\n]' files…` → file → package (lines for other files ignored).
pub fn parse_rpm_owners(text: &str, wanted: &HashSet<PathBuf>) -> HashMap<PathBuf, String> {
    text.lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(f, n)| (PathBuf::from(f), n.to_string()))
        .filter(|(f, _)| wanted.contains(f))
        .collect()
}

/// `dpkg-query -W -f='${Package}\t${Version}\t${Architecture}\t${Installed-Size}\t${db:Status-Abbrev}\n'`
pub fn parse_dpkg_query(text: &str) -> Vec<Pkg> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 5 && f[4].starts_with("ii")).then(|| Pkg {
                name: f[0].into(),
                version: f[1].into(),
                arch: f[2].into(),
                size: f[3].trim().parse::<u64>().ok().map(|kib| kib * 1024),
            })
        })
        .collect()
}

/// `dpkg -S files…` → "pkg1, pkg2: /path" lines.
pub fn parse_dpkg_owners(text: &str) -> HashMap<PathBuf, String> {
    text.lines()
        .filter_map(|l| l.split_once(": "))
        .filter_map(|(pkgs, f)| {
            let pkg = pkgs
                .split(',')
                .next()?
                .trim()
                .split(':')
                .next()?
                .to_string();
            Some((PathBuf::from(f.trim()), pkg))
        })
        .collect()
}

/// `pacman -Q` → "name version" lines.
pub fn parse_pacman_q(text: &str) -> Vec<Pkg> {
    text.lines()
        .filter_map(|l| l.split_once(' '))
        .map(|(n, v)| Pkg {
            name: n.into(),
            version: v.trim().into(),
            arch: String::new(),
            size: None,
        })
        .collect()
}

/// `pacman -Qo files…` → "/path is owned by pkg version" lines.
pub fn parse_pacman_owners(text: &str) -> HashMap<PathBuf, String> {
    text.lines()
        .filter_map(|l| l.split_once(" is owned by "))
        .filter_map(|(f, rest)| {
            Some((
                PathBuf::from(f),
                rest.split_whitespace().next()?.to_string(),
            ))
        })
        .collect()
}

#[derive(Clone, Copy)]
enum Manager {
    Rpm,
    Dpkg,
    Pacman,
}

fn detect_manager() -> Option<Manager> {
    if run("rpm", &["-q", "rpm"]).is_some() {
        Some(Manager::Rpm)
    } else if run("dpkg-query", &["-W", "dpkg"]).is_some() {
        Some(Manager::Dpkg)
    } else if run("pacman", &["-Q", "pacman"]).is_some() {
        Some(Manager::Pacman)
    } else {
        None
    }
}

/// Packages, the names the user installed on purpose, and recorded install reasons (None = unknown).
type Listing = (
    Vec<Pkg>,
    Option<HashSet<String>>,
    Option<HashMap<String, (String, String)>>,
);

fn list_packages(m: Manager) -> Listing {
    match m {
        Manager::Rpm => {
            let pkgs = run("rpm", &["-qa", "--qf", RPM_QF])
                .map(|t| parse_rpm_qa(&t))
                .unwrap_or_default();
            let qf = [
                "-C",
                "-q",
                "repoquery",
                "--installed",
                "--qf",
                "%{name}|%{reason}|%{from_repo}\\n",
            ];
            let reasons = run("dnf5", &qf)
                .or_else(|| run("dnf", &qf))
                .map(|t| parse_dnf_reasons(&t));
            let user = reasons.as_ref().map(|r| {
                r.iter()
                    .filter(|(_, (why, _))| why.contains("User"))
                    .map(|(n, _)| n.clone())
                    .collect()
            });
            (pkgs, user, reasons)
        }
        Manager::Dpkg => {
            let f =
                "${Package}\t${Version}\t${Architecture}\t${Installed-Size}\t${db:Status-Abbrev}\n";
            let pkgs = run("dpkg-query", &["-W", "-f", f])
                .map(|t| parse_dpkg_query(&t))
                .unwrap_or_default();
            let user =
                run("apt-mark", &["showmanual"]).map(|t| t.lines().map(str::to_string).collect());
            (pkgs, user, None)
        }
        Manager::Pacman => {
            let pkgs = run("pacman", &["-Q"])
                .map(|t| parse_pacman_q(&t))
                .unwrap_or_default();
            let user = run("pacman", &["-Qqe"]).map(|t| t.lines().map(str::to_string).collect());
            (pkgs, user, None)
        }
    }
}

fn owners(m: Manager, files: &[&Path]) -> HashMap<PathBuf, String> {
    if files.is_empty() {
        return HashMap::new();
    }
    let args: Vec<&str> = files.iter().filter_map(|p| p.to_str()).collect();
    let call = |cmd: &str, pre: &[&str]| {
        let all: Vec<&str> = pre.iter().copied().chain(args.iter().copied()).collect();
        output(Command::new(cmd).args(&all))
            .map(|(_, s)| s)
            .unwrap_or_default()
    };
    match m {
        Manager::Rpm => {
            let wanted = files.iter().map(|p| p.to_path_buf()).collect();
            parse_rpm_owners(
                &call("rpm", &["-qf", "--qf", "[%{FILENAMES}\\t%{=NAME}\\n]"]),
                &wanted,
            )
        }
        Manager::Dpkg => parse_dpkg_owners(&call("dpkg", &["-S"])),
        Manager::Pacman => parse_pacman_owners(&call("pacman", &["-Qo"])),
    }
}

/// Lists packaged desktop apps (one per package owning a visible shortcut) and registry-known
/// command-line tools the user installed. Returns the shortcut files that a package owns.
fn native(cx: &mut Ctx, shortcuts: &[Shortcut]) -> HashSet<PathBuf> {
    let Some(m) = detect_manager() else {
        cx.notes
            .push("No RPM, DPKG or Pacman package database was found.".into());
        return HashSet::new();
    };
    let (source, label) = match m {
        Manager::Rpm => (Source::Rpm, "RPM"),
        Manager::Dpkg => (Source::Dpkg, "DPKG"),
        Manager::Pacman => (Source::Pacman, "Pacman"),
    };
    let (pkgs, user, reasons) = list_packages(m);
    if user.is_none() {
        cx.notes.push(format!(
            "Could not tell which {label} packages you installed yourself; all are shown as yours."
        ));
    }
    let files: Vec<&Path> = shortcuts.iter().map(|s| s.file.as_path()).collect();
    let owner = owners(m, &files);
    let by_name: HashMap<&str, &Pkg> = pkgs.iter().map(|p| (p.name.as_str(), p)).collect();

    let mut shown = HashSet::new();
    // A package's main shortcut is usually named after it (org.kicad.kicad.desktop for kicad).
    let primary = |file: &Path, pkg: &str| {
        let stem = file
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        stem == pkg || stem.ends_with(&format!(".{pkg}"))
    };
    let make = |p: &Pkg, name: &str, kind: Kind| {
        let mut app = App::new(source, &p.name, name, kind);
        app.version = Some(p.version.clone());
        app.install_size = p.size;
        app.system = user.as_ref().is_some_and(|u| !u.contains(&p.name));
        let (why, repo) = reasons
            .as_ref()
            .and_then(|r| r.get(&p.name))
            .map_or((None, None), |(w, r)| (Some(w), Some(r)));
        app.repository = repo.filter(|r| !r.is_empty()).cloned();
        app.evidence.push(
            format!("{label} package {} {} {}", p.name, p.version, p.arch)
                .trim_end()
                .to_string(),
        );
        if let Some(why) = why {
            app.evidence.push(format!(
                "Install reason recorded by the package manager: {why}"
            ));
        }
        if app.system {
            app.confidence = Confidence::Probable;
        }
        app
    };

    for s in shortcuts {
        let Some(pkg) = owner.get(&s.file).and_then(|n| by_name.get(n.as_str())) else {
            continue;
        };
        if !shown.insert(pkg.name.clone()) {
            if let Some(a) = cx
                .apps
                .iter_mut()
                .find(|a| a.source == source && a.native_id == pkg.name)
            {
                a.evidence
                    .push(format!("Also owns shortcut {}", s.file.display()));
                if primary(&s.file, &pkg.name) {
                    a.name = s.entry.name.clone();
                }
            }
            continue;
        }
        let mut app = make(pkg, &s.entry.name, Kind::Desktop);
        app.evidence
            .push(format!("Owns shortcut {}", s.file.display()));
        cx.apps.push(app);
    }
    for p in &pkgs {
        if shown.contains(&p.name) || !user.as_ref().is_none_or(|u| u.contains(&p.name)) {
            continue;
        }
        if let Some(entry) = registry::lookup(source, &p.name) {
            let mut app = make(p, &entry.name, Kind::CommandLine);
            app.evidence
                .push("Command-line tool known to the application registry".into());
            cx.apps.push(app);
            shown.insert(p.name.clone());
        }
    }
    cx.hidden += pkgs.len().saturating_sub(shown.len());
    owner.into_keys().collect()
}

// ---------------------------------------------------------------- Custom installs and AppImages

fn resolve_program(prog: &str, home: &Path) -> Option<PathBuf> {
    let p = PathBuf::from(prog);
    if p.is_absolute() {
        return p.is_file().then_some(p);
    }
    if prog.contains('/') {
        return None;
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain([home.join(".local/bin")])
        .map(|d| d.join(prog))
        .find(|c| c.is_file())
}

fn unowned_shortcuts(cx: &mut Ctx, shortcuts: &[Shortcut], owned: &HashSet<PathBuf>) {
    for s in shortcuts.iter().filter(|s| !owned.contains(&s.file)) {
        let Some(prog) = s.entry.exec.as_deref().and_then(exec_program) else {
            continue;
        };
        let Some(path) = resolve_program(&prog, cx.home) else {
            continue;
        };
        // Shortcuts that start OS-provided programs are not separate installs.
        if path.starts_with("/usr") && !path.starts_with("/usr/local") || path.starts_with("/bin") {
            continue;
        }
        let path = path.canonicalize().unwrap_or(path);
        if is_appimage(&path) {
            let mut app = appimage_app(&path);
            app.name = s.entry.name.clone();
            app.evidence
                .push(format!("Shortcut {} launches it", s.file.display()));
            cx.apps.push(app);
            continue;
        }
        let mut app = App::new(
            Source::Custom,
            &path.to_string_lossy(),
            &s.entry.name,
            Kind::Desktop,
        );
        app.confidence = Confidence::Probable;
        app.evidence.push(format!(
            "Shortcut {} launches {}",
            s.file.display(),
            path.display()
        ));
        app.install_size = std::fs::metadata(&path).ok().map(|m| m.len());
        if let Some(repo) = git::checkout_containing(&path, cx.home) {
            app.evidence.push(format!(
                "Program lives in a Git checkout at {}",
                repo.dir.display()
            ));
            app.version = repo.reference.clone();
            app.github = repo.github(Confidence::Probable);
            app.evidence.extend(repo.github_evidence());
        }
        app.path = Some(path);
        cx.apps.push(app);
    }
}

/// AppImage type 2 magic: "AI\x02" at byte offset 8.
fn is_appimage(path: &Path) -> bool {
    if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("appimage"))
    {
        return true;
    }
    let mut head = [0u8; 11];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && &head[8..11] == b"AI\x02"
}

/// "Obsidian-1.5.3-x86_64" → ("Obsidian", Some("1.5.3")).
pub fn split_name_version(stem: &str) -> (String, Option<String>) {
    let b = stem.as_bytes();
    let cut = (1..b.len()).find(|&i| {
        matches!(b[i - 1], b'-' | b'_') && b[i].is_ascii_digit()
            || b[i - 1] == b'v'
                && i >= 2
                && matches!(b[i - 2], b'-' | b'_')
                && b[i].is_ascii_digit()
    });
    match cut {
        Some(i) => {
            let name_end = if b[i - 1] == b'v' { i - 2 } else { i - 1 };
            let ver: String = stem[i..].split(['-', '_']).next().unwrap_or("").to_string();
            (stem[..name_end].to_string(), Some(ver))
        }
        None => (stem.to_string(), None),
    }
}

/// The GitHub project an AppStream metainfo file links to: its source browser, else its bug
/// tracker or home page.
pub fn metainfo_github(xml: &str) -> Option<(String, &'static str)> {
    ["vcs-browser", "bugtracker", "homepage"]
        .into_iter()
        .find_map(|kind| {
            let tag = format!("<url type=\"{kind}\">");
            let rest = &xml[xml.find(&tag)? + tag.len()..];
            git::github_repo(rest[..rest.find("</url>")?].trim()).map(|r| (r, kind))
        })
}

/// Embedded AppImage update information, e.g. `gh-releases-zsync|owner|repo|latest|App-*x86_64.AppImage.zsync`.
pub fn parse_update_info(info: &str) -> Option<GitHub> {
    let f: Vec<&str> = info.split('|').collect();
    (f.first() == Some(&"gh-releases-zsync") && f.len() >= 5).then(|| GitHub {
        repo: format!("{}/{}", f[1], f[2]),
        reference: (f[3] != "latest").then(|| f[3].to_string()),
        asset: Some(f[4].trim_end_matches(".zsync").to_string()),
        confidence: Confidence::Confirmed,
        origin: Origin::Detected,
    })
}

fn read_update_info(path: &Path) -> Option<String> {
    const NEEDLE: &[u8] = b"gh-releases-zsync|";
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(4 << 20)
        .read_to_end(&mut buf)
        .ok()?;
    let start = buf.windows(NEEDLE.len()).position(|w| w == NEEDLE)?;
    let end = buf[start..]
        .iter()
        .position(|&c| c == 0 || c < 0x20)
        .map_or(buf.len(), |e| start + e);
    String::from_utf8(buf[start..end].to_vec()).ok()
}

fn appimage_app(path: &Path) -> App {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (name, version) = split_name_version(&stem);
    let mut app = App::new(
        Source::AppImage,
        &path.to_string_lossy(),
        &name,
        Kind::Desktop,
    );
    app.version = version;
    app.install_size = std::fs::metadata(path).ok().map(|m| m.len());
    app.path = Some(path.to_path_buf());
    app.evidence
        .push(format!("AppImage file {}", path.display()));
    match read_update_info(path)
        .as_deref()
        .and_then(parse_update_info)
    {
        Some(gh) => {
            app.evidence.push(format!(
                "Embedded update information points to GitHub releases of {}",
                gh.repo
            ));
            app.github = Some(gh);
        }
        None => app
            .evidence
            .push("No embedded update information; download origin unknown".into()),
    }
    app
}

fn appimages(cx: &mut Ctx, opts: &ScanOptions) {
    let known: HashSet<String> = cx.apps.iter().map(|a| a.native_id.clone()).collect();
    let dirs = ["Applications", "AppImages", ".local/bin", "bin", "opt"].map(|d| cx.home.join(d));
    for dir in dirs.iter().chain(&opts.source_dirs) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if e.file_type().is_ok_and(|t| t.is_file())
                && is_appimage(&p)
                && !known.contains(&*p.to_string_lossy())
            {
                cx.apps.push(appimage_app(&p));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatpak_rows() {
        let t = "com.brave.Browser\tBrave\t1.98.62\tstable\tflathub\tsystem\tx86_64\t525.6\u{a0}MB\torg.freedesktop.Platform/x86_64/25.08\n\
                 org.x.NoName\t\t\tstable\tflathub\tuser\taarch64\t2.4 kB\t\n";
        let rows = parse_flatpak(t);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "com.brave.Browser");
        assert_eq!(rows[0].size, Some(525_600_000));
        assert_eq!(rows[1].name, "org.x.NoName");
        assert_eq!(rows[1].size, Some(2400));
    }

    #[test]
    fn snap_rows() {
        let t = "Name               Version   Rev    Tracking       Publisher    Notes\n\
                 core24             20260824  2124   latest/stable  canonical**  base\n\
                 firefox            131.0     5000   latest/stable  mozilla**    -\n";
        let rows = parse_snap_list(t);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "firefox");
        assert_eq!(rows[0].notes, "base");
    }

    #[test]
    fn desktop_entries() {
        let t = "[Desktop Entry]\nType=Application\nName=Firewall\nName[de]=Brandmauer\nExec=\"/home/u/my apps/fw\" %U\n\n[Desktop Action new]\nName=Other\nExec=nope\n";
        let e = parse_desktop(t).unwrap();
        assert_eq!(e.name, "Firewall");
        assert_eq!(
            exec_program(e.exec.as_deref().unwrap()).as_deref(),
            Some("/home/u/my apps/fw")
        );
        assert!(parse_desktop("[Desktop Entry]\nType=Link\nName=x\n").is_none());
        assert!(
            parse_desktop("[Desktop Entry]\nType=Application\nName=x\nNoDisplay=true\n")
                .unwrap()
                .hidden
        );
        assert_eq!(
            exec_program("env GDK_BACKEND=x11 FOO=1 /opt/app/run --x").as_deref(),
            Some("/opt/app/run")
        );
        assert_eq!(exec_program("code %F").as_deref(), Some("code"));
    }

    #[test]
    fn appstream_github_links() {
        let xml = "<component><url type=\"homepage\">https://www.orcaslicer.com</url>\n<url type=\"bugtracker\">https://github.com/OrcaSlicer/OrcaSlicer/issues/</url>\n<url type=\"vcs-browser\">https://github.com/OrcaSlicer/OrcaSlicer</url></component>";
        assert_eq!(
            metainfo_github(xml),
            Some(("OrcaSlicer/OrcaSlicer".into(), "vcs-browser"))
        );
        assert_eq!(
            metainfo_github("<url type=\"homepage\">https://rustdesk.com</url>"),
            None
        );
    }

    #[test]
    fn native_package_parsers() {
        let rpm =
            parse_rpm_qa("gh\t0\t2.83.0-1.fc44\tx86_64\t52000000\nkernel\t1\t6.0-1\tx86_64\t9\n");
        assert_eq!(rpm[0].version, "2.83.0-1.fc44");
        assert_eq!(rpm[1].version, "1:6.0-1");
        let r = parse_dnf_reasons(
            "gh|User|updates\nnautilus|Group|fedora\n7zip|Weak Dependency|updates\nmine|User|@commandline\n",
        );
        assert_eq!(r["7zip"], ("Weak Dependency".into(), "updates".into()));
        assert_eq!(r["mine"].1, "@commandline");
        let wanted: HashSet<PathBuf> = [PathBuf::from("/usr/share/applications/a.desktop")].into();
        let o = parse_rpm_owners(
            "/usr/bin/a\ta\n/usr/share/applications/a.desktop\ta\n",
            &wanted,
        );
        assert_eq!(o.len(), 1);

        let d = parse_dpkg_query("vlc\t3.0.20-1\tamd64\t120\tii \nold\t1\tamd64\t1\trc \n");
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].size, Some(120 * 1024));
        let o = parse_dpkg_owners(
            "vlc-bin, vlc: /usr/share/applications/vlc.desktop\ngimp:amd64: /usr/share/applications/gimp.desktop\n",
        );
        assert_eq!(
            o[Path::new("/usr/share/applications/vlc.desktop")],
            "vlc-bin"
        );
        assert_eq!(o[Path::new("/usr/share/applications/gimp.desktop")], "gimp");

        assert_eq!(parse_pacman_q("firefox 131.0-1\n")[0].version, "131.0-1");
        let o = parse_pacman_owners(
            "/usr/share/applications/firefox.desktop is owned by firefox 131.0-1\n",
        );
        assert_eq!(
            o[Path::new("/usr/share/applications/firefox.desktop")],
            "firefox"
        );
    }

    #[test]
    fn appimage_names_and_update_info() {
        assert_eq!(
            split_name_version("Obsidian-1.5.3"),
            ("Obsidian".into(), Some("1.5.3".into()))
        );
        assert_eq!(
            split_name_version("balenaEtcher-v1.18.11-x64"),
            ("balenaEtcher".into(), Some("1.18.11".into()))
        );
        assert_eq!(split_name_version("Tool"), ("Tool".into(), None));
        let gh = parse_update_info(
            "gh-releases-zsync|obsidianmd|obsidian-releases|latest|Obsidian-*.AppImage.zsync",
        )
        .unwrap();
        assert_eq!(gh.repo, "obsidianmd/obsidian-releases");
        assert_eq!(gh.reference, None);
        assert_eq!(gh.asset.as_deref(), Some("Obsidian-*.AppImage"));
        assert!(parse_update_info("zsync|https://example.com/x.zsync").is_none());
    }
}
