//! Whether, and how, an app can be installed on a destination computer: the one place that
//! decides compatibility.
//!
//! Decisions use the destination's capabilities (its distribution profile, package manager,
//! portable formats, processor), never the source app's file type or the processors alone:
//! - [`bundle_fits`] says whether an installer in the snapshot can be used there. RPM
//!   dependency sets only on the release they were collected on, vendor `.deb` files only on
//!   DEB distributions, nothing on a read-only system that needs a package manager. Packages
//!   are never converted between formats.
//! - [`candidates`] takes the ways [`resolve::choose`] found, applies the profile's rules and
//!   the user's preferences, checks the package manager really offers each package and from
//!   which repository, and explains every option it drops.
//! - [`verdict`] puts the outcome in one of four groups people see.
//!
//! Everything is deterministic: the same snapshot, profile and package lists give the same
//! plan and the same sentences, so a plan can be audited.

use std::collections::HashMap;
use std::process::Command;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::discovery::Source;
use crate::distros::{Adapter, Avail, Format, MethodKind, Resolved, RuleAction};
use crate::manifest::{ManifestApp, Recipe, Status};
use crate::platform::OsFamily;
use crate::probe::SystemInfo;
use crate::resolve::{self, Channel, Method, Prefs};

/// The four groups of the compatibility summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Installs as it is, or with the destination's own equivalent package.
    Ready,
    /// Installs another way than on the old computer (a Flatpak, a download…).
    Alternative,
    /// Needs the user: an approval (source build, unvetted repository, custom commands,
    /// changing a read-only system), a download while offline, or a manual step.
    Review,
    /// Cannot be installed there; skipped with the reason.
    Unavailable,
}

impl Verdict {
    /// For plans and coverage written before verdicts existed.
    pub fn from_status(s: Status) -> Self {
        match s {
            Status::Ready => Self::Ready,
            Status::Manual => Self::Review,
            Status::Unsupported => Self::Unavailable,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready to install",
            Self::Alternative => "Alternative installation method",
            Self::Review => "Needs your approval or a download",
            Self::Unavailable => "Cannot be transferred",
        }
    }
}

/// An option that was looked at, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Considered {
    pub option: String,
    /// Chosen, kept as a fallback, or why it was not used.
    pub outcome: String,
}

/// The kind of install a method is, as rules name them.
pub fn kind(m: &Method) -> MethodKind {
    match m {
        Method::System(..) | Method::Winget(_) | Method::Brew { .. } => MethodKind::Native,
        Method::Flathub(_) => MethodKind::Flatpak,
        Method::Snap(_) => MethodKind::Snap,
        Method::Download => MethodKind::Download,
        Method::Source { .. } => MethodKind::Source,
    }
}

fn describe(info: &SystemInfo) -> String {
    match info.profile() {
        Some(p) => match info.distro.as_deref() {
            Some(d)
                if info
                    .linux
                    .as_ref()
                    .is_some_and(|l| l.os_release.version_id.is_some()) =>
            {
                format!("{} ({d})", p.name)
            }
            _ => p.name.clone(),
        },
        None => info.device.os_name.clone(),
    }
}

fn formats(p: &Resolved) -> String {
    if p.formats.is_empty() {
        "no native package format Simple Setup installs".into()
    } else {
        p.formats
            .iter()
            .map(|f| f.label())
            .collect::<Vec<_>>()
            .join("/")
            + " packages"
    }
}

/// Whether an installer in the snapshot suits the computer described by `info`, as far as the
/// distribution goes (tools and processor are checked by [`crate::installers::check`] and the
/// installer's platform). `Err` says why not, in a sentence.
pub fn bundle_fits(r: &Recipe, info: &SystemInfo) -> std::result::Result<(), String> {
    let Some(p) = info.profile() else {
        return Ok(());
    };
    let here = describe(info);
    let image = info.linux.as_ref().is_some_and(|l| l.immutable);
    match r {
        Recipe::Rpm { distro, .. } => {
            if !p.has_format(Format::Rpm) {
                return Err(format!(
                    "The snapshot holds RPM packages collected on {distro}; {here} uses {}. RPM packages are never converted to another format.",
                    formats(p)
                ));
            }
            if image {
                return Err(format!(
                    "The snapshot holds RPM packages for {distro}, but {here} has a read-only system that does not install package files this way."
                ));
            }
            // A destination that is not here is known only by its distribution, not release.
            let known_release = info
                .linux
                .as_ref()
                .is_some_and(|l| l.os_release.version_id.is_some());
            let ok = if known_release {
                info.distro.as_deref() == Some(distro.as_str())
            } else {
                distro.split(' ').next() == Some(p.id.as_str())
            };
            if ok {
                Ok(())
            } else {
                Err(format!(
                    "The snapshot's RPM packages were collected for {distro} and only install offline on that release; this computer runs {here}."
                ))
            }
        }
        Recipe::Deb { package, .. } => {
            if !p.has_format(Format::Deb) {
                Err(format!(
                    "The snapshot's .deb package of {package} is for DEB distributions (Debian, Ubuntu, Linux Mint…); {here} uses {}.",
                    formats(p)
                ))
            } else if image {
                Err(format!(
                    "The snapshot's .deb package of {package} cannot be installed on {here}'s read-only system."
                ))
            } else {
                Ok(())
            }
        }
        Recipe::Flatpak { .. } if p.flatpak == Avail::Unsupported && !info.has("flatpak") => {
            Err(format!("Flatpak is not available on {here}."))
        }
        Recipe::Snap { .. } if p.snap == Avail::Unsupported && !info.has("snap") => {
            Err(format!("Snap is switched off or not available on {here}."))
        }
        Recipe::Program {
            launcher: Some(_), ..
        } if p.appimage == Avail::Unsupported => Err(format!(
            "AppImages do not run on {here}{}",
            p.notes
                .as_deref()
                .map(|n| format!(": {n}"))
                .unwrap_or_else(|| ".".into())
        )),
        _ => Ok(()),
    }
}

/// Whether this computer's own copy of a package may be collected for a destination: only
/// for the same distribution profile (and never for a read-only one), as the files are tied
/// to it. `None` destination: this computer's own distribution.
pub fn local_package_fits(source: &SystemInfo, dest: &SystemInfo) -> bool {
    match (source.profile(), dest.profile()) {
        (Some(s), Some(d)) => s.id == d.id && !dest.linux.as_ref().is_some_and(|l| l.immutable),
        _ => true,
    }
}

/// The destination takes `.deb` files from an app's GitHub release.
pub fn wants_deb(dest: &SystemInfo) -> bool {
    dest.device.platform.os == OsFamily::Linux
        && dest.profile().is_some_and(|p| {
            p.has_format(Format::Deb) && !dest.linux.as_ref().is_some_and(|l| l.immutable)
        })
}

// ---------------------------------------------------------------- package lists

/// What the destination's package manager says about package names: the repository each
/// available one comes from. Live queries run read-only commands; for a computer that is not
/// here nothing can be asked.
pub trait Query {
    /// `None`: the package manager cannot be asked (not here, no package lists, a failure).
    fn origins(&mut self, mgr: Adapter, names: &[String]) -> Option<HashMap<String, String>>;
}

/// For destinations that are not this computer: everything is checked when restoring.
pub struct NoQuery;

impl Query for NoQuery {
    fn origins(&mut self, _: Adapter, _: &[String]) -> Option<HashMap<String, String>> {
        None
    }
}

/// Asks this computer's package manager (read-only). Answers are kept per name, so asking for
/// every app's names once up front ([`Query::origins`] with all of them) serves the rest.
#[derive(Default)]
pub struct LiveQuery {
    asked: HashMap<Adapter, std::collections::HashSet<String>>,
    found: HashMap<Adapter, HashMap<String, String>>,
    failed: std::collections::HashSet<Adapter>,
}

/// A read-only query's output; `strict`: a failure means the answer is unknown (pacman exits
/// 1 when only some names are missing, so it is not strict).
fn stdout(prog: &str, args: &[&str], strict: bool) -> Option<String> {
    let o = Command::new(prog)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    (!strict || o.status.success()).then(|| String::from_utf8_lossy(&o.stdout).into_owned())
}

impl Query for LiveQuery {
    fn origins(&mut self, mgr: Adapter, names: &[String]) -> Option<HashMap<String, String>> {
        if self.failed.contains(&mgr) {
            return None;
        }
        let asked = self.asked.entry(mgr).or_default();
        let missing: Vec<String> = names
            .iter()
            .filter(|n| !asked.contains(*n))
            .cloned()
            .collect();
        if !missing.is_empty() {
            match ask(mgr, &missing) {
                Some(v) => {
                    asked.extend(missing);
                    self.found.entry(mgr).or_default().extend(v);
                }
                None => {
                    self.failed.insert(mgr);
                    return None;
                }
            }
        }
        let found = self.found.get(&mgr)?;
        Some(
            names
                .iter()
                .filter_map(|n| Some((n.clone(), found.get(n)?.clone())))
                .collect(),
        )
    }
}

/// One read-only query of `mgr`'s package lists for `names`: name → repository.
fn ask(mgr: Adapter, names: &[String]) -> Option<HashMap<String, String>> {
    let with = |first: &[&'static str]| -> Vec<String> {
        first
            .iter()
            .map(|s| s.to_string())
            .chain(names.iter().cloned())
            .collect()
    };
    let run = |prog: &str, args: Vec<String>, strict: bool| {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        stdout(prog, &args, strict)
    };
    match mgr {
        Adapter::Apt => {
            // No package lists yet (a fresh install): the restore refreshes them first.
            let lists = std::fs::read_dir("/var/lib/apt/lists").ok()?;
            if !lists
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with("_Packages"))
            {
                return None;
            }
            let sources = stdout("apt-cache", &["policy"], true)?;
            Some(parse_apt_policy(
                &run("apt-cache", with(&["policy"]), true)?,
                &sources,
            ))
        }
        Adapter::Dnf => Some(parse_name_repo(&run(
            "dnf",
            with(&[
                "-q",
                "repoquery",
                "--available",
                "--qf",
                "%{name} %{repoid}\n",
            ]),
            true,
        )?)),
        Adapter::Zypper | Adapter::TransactionalUpdate => Some(parse_info_blocks(&run(
            "zypper",
            with(&["--non-interactive", "--no-refresh", "info"]),
            true,
        )?)),
        Adapter::Pacman => Some(parse_info_blocks(&run("pacman", with(&["-Si"]), false)?)),
        Adapter::Apk => Some(parse_apk_policy(&run("apk", with(&["policy"]), true)?)),
        Adapter::RpmOstree | Adapter::Custom | Adapter::None => None,
    }
}

/// `apt-cache policy <names>` plus `apt-cache policy` (which maps each source to its signed
/// release `Origin`): package → origin of its candidate version.
pub fn parse_apt_policy(packages: &str, sources: &str) -> HashMap<String, String> {
    // " 500 http://archive.ubuntu.com/ubuntu noble-updates/main amd64 Packages"
    //      "release v=24.04,o=Ubuntu,a=noble-updates,…"
    let mut origin_of: HashMap<String, String> = HashMap::new();
    let mut last: Option<String> = None;
    for l in sources.lines() {
        let t = l.trim();
        if let Some(rel) = t.strip_prefix("release ") {
            if let (Some(src), Some(o)) = (
                last.take(),
                rel.split(',').find_map(|kv| kv.strip_prefix("o=")),
            ) {
                origin_of.insert(src, o.to_string());
            }
        } else if t
            .split_whitespace()
            .next()
            .is_some_and(|n| n.parse::<i32>().is_ok())
        {
            last = t.split_once(' ').map(|(_, src)| src.trim().to_string());
        }
    }
    let mut out = HashMap::new();
    let mut name: Option<String> = None;
    let mut candidate: Option<String> = None;
    let mut in_candidate = false;
    for l in packages.lines() {
        if !l.starts_with(' ') && l.ends_with(':') {
            name = Some(l.trim_end_matches(':').to_string());
            candidate = None;
            in_candidate = false;
            continue;
        }
        let t = l.trim();
        if let Some(c) = t.strip_prefix("Candidate:") {
            let c = c.trim();
            candidate = (c != "(none)").then(|| c.to_string());
            continue;
        }
        // Version table: "*** 1.2-3 500" or "1.2-3 500", then its sources indented below.
        let fields: Vec<&str> = t.trim_start_matches("*** ").split_whitespace().collect();
        if fields.len() == 2 && fields[1].parse::<i32>().is_ok() && !fields[0].contains('/') {
            in_candidate = candidate.as_deref() == Some(fields[0]);
            continue;
        }
        if in_candidate
            && let (Some(n), Some((_, src))) = (&name, t.split_once(' '))
            && !src.starts_with("/var/lib/dpkg/status")
            && !out.contains_key(n)
        {
            let src = src.trim();
            let origin = origin_of
                .get(src)
                .cloned()
                .unwrap_or_else(|| src.split_whitespace().next().unwrap_or(src).to_string());
            out.insert(n.clone(), origin);
        }
    }
    out
}

/// Lines of `name repoid` (dnf repoquery).
pub fn parse_name_repo(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for l in text.lines() {
        if let Some((n, r)) = l.trim().split_once(' ') {
            out.entry(n.to_string())
                .or_insert_with(|| r.trim().to_string());
        }
    }
    out
}

/// `Key : value` blocks (zypper info, pacman -Si): name → repository.
pub fn parse_info_blocks(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let (mut name, mut repo): (Option<String>, Option<String>) = (None, None);
    let mut flush = |name: &mut Option<String>, repo: &mut Option<String>| {
        if let (Some(n), Some(r)) = (name.take(), repo.take())
            && r != "@System"
        {
            out.entry(n).or_insert(r);
        }
    };
    for l in text.lines() {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Repository" => {
                if repo.is_some() && name.is_some() {
                    flush(&mut name, &mut repo);
                }
                repo = Some(v.to_string());
            }
            "Name" => {
                if name.is_some() && repo.is_some() {
                    flush(&mut name, &mut repo);
                }
                name = Some(v.to_string());
            }
            _ => {}
        }
        if name.is_some() && repo.is_some() {
            flush(&mut name, &mut repo);
        }
    }
    out
}

/// `apk policy <names>`: name → the first repository URL offering it.
pub fn parse_apk_policy(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut name: Option<String> = None;
    for l in text.lines() {
        if let Some(n) = l.strip_suffix(" policy:") {
            name = Some(n.trim().to_string());
        } else if let Some(n) = &name {
            let t = l.trim();
            if t.contains("://") && !out.contains_key(n) {
                out.insert(n.clone(), t.to_string());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- choosing

/// One way to install the app, as planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub method: Method,
    /// Why using it needs the user's approval, if it does.
    pub review: Option<String>,
}

/// The ways the computer `info` can install `a`, best first, and what happened to every other
/// option. `from`: see [`resolve::choose`].
pub fn candidates(
    a: &ManifestApp,
    from: &[String],
    info: &SystemInfo,
    prefs: &Prefs,
    query: &mut dyn Query,
) -> (Vec<Candidate>, Vec<Considered>) {
    let os = info.device.platform.os;
    let mut considered = Vec::new();
    let profile = info.profile();
    let mut methods = resolve::choose(a, from, info, prefs);
    if let Some(p) = profile {
        for r in p.rules_for(a.canonical.as_deref()) {
            match (r.action, r.method) {
                (RuleAction::Skip, _) => {
                    considered.extend(methods.drain(..).map(|m| Considered {
                        option: m.label(os),
                        outcome: format!("Not used: {}", r.reason),
                    }));
                    return (
                        Vec::new(),
                        std::iter::once(Considered {
                            option: format!("{} on {}", a.name, p.name),
                            outcome: r.reason.clone(),
                        })
                        .chain(considered)
                        .collect(),
                    );
                }
                (RuleAction::Avoid, Some(k)) => {
                    methods.retain(|m| {
                        let drop = kind(m) == k;
                        if drop {
                            considered.push(Considered {
                                option: m.label(os),
                                outcome: format!("Not used on {}: {}", p.name, r.reason),
                            });
                        }
                        !drop
                    });
                }
                (RuleAction::Prefer, Some(k)) => {
                    // Stable: the preferred kind moves to the front, order otherwise kept.
                    methods.sort_by_key(|m| kind(m) != k);
                }
                _ => {}
            }
        }
    }
    // Ask the package manager which names it offers, and from where (one query).
    let system_names: Vec<String> = methods
        .iter()
        .filter_map(|m| match m {
            Method::System(_, n) => Some(n.clone()),
            _ => None,
        })
        .collect();
    let mgr = methods.iter().find_map(|m| match m {
        Method::System(mgr, _) => Some(*mgr),
        _ => None,
    });
    let origins = match mgr {
        Some(mgr) if !system_names.is_empty() => query.origins(mgr, &system_names),
        _ => None,
    };
    let image = info.linux.as_ref().is_some_and(|l| l.immutable);
    let mut out: Vec<Candidate> = Vec::new();
    let mut third_party: Vec<Candidate> = Vec::new();
    for m in methods {
        let label = m.label(os);
        let mut review = None;
        if let Method::System(mgr, n) = &m {
            if let Some(o) = &origins {
                match o.get(n) {
                    None => {
                        considered.push(Considered {
                            option: label,
                            outcome: format!(
                                "Not offered by {}'s package sources.",
                                describe(info)
                            ),
                        });
                        continue;
                    }
                    Some(repo) => {
                        let official = profile.and_then(|p| p.official_repo(repo));
                        if official != Some(true) {
                            third_party.push(Candidate {
                                method: m.clone(),
                                review: Some(format!(
                                    "{n} comes from the repository {repo:?}, which is not one of {}'s own; install it only if you trust that source.",
                                    profile.map_or("this system", |p| p.name.as_str())
                                )),
                            });
                            continue;
                        }
                    }
                }
            }
            if *mgr == Adapter::Custom {
                review = Some(format!(
                    "Uses the custom commands of your {} profile: {}",
                    profile.map_or("distribution", |p| p.name.as_str()),
                    resolve::custom_command(info, &[n]).unwrap_or_default()
                ));
            } else if mgr.needs_restart() || image {
                review = Some(format!(
                    "Adds {n} to {}'s read-only system image with {}; it takes effect after a restart.",
                    describe(info),
                    mgr.label()
                ));
            }
        }
        if matches!(m, Method::Source { .. }) {
            review = Some(
                "Builds the project's source code on this computer, which runs that project's build scripts."
                    .into(),
            );
        }
        out.push(Candidate { method: m, review });
    }
    // Packages from repositories the distribution does not vouch for go after everything else
    // except source builds.
    let at = out
        .iter()
        .position(|c| matches!(c.method, Method::Source { .. }))
        .unwrap_or(out.len());
    out.splice(at..at, third_party);
    for (i, c) in out.iter().enumerate() {
        considered.push(Considered {
            option: c.method.label(os),
            outcome: if i == 0 {
                "Chosen.".into()
            } else {
                format!("Fallback {i}, tried if the ones before fail.")
            },
        });
    }
    if out.is_empty() && considered.is_empty() {
        considered.push(Considered {
            option: a.name.clone(),
            outcome: no_way(a, info),
        });
    }
    (out, considered)
}

/// Why nothing installs `a`, from what [`candidates`] considered (a rule's reason, a package
/// the sources do not offer…), else [`no_way`].
pub fn why_none(considered: &[Considered], a: &ManifestApp, info: &SystemInfo) -> String {
    considered
        .iter()
        .find(|c| !c.outcome.starts_with("Chosen") && c.option != "The installer in the snapshot")
        .map(|c| format!("{}: {}", c.option, c.outcome))
        .unwrap_or_else(|| no_way(a, info))
}

/// Why nothing installs `a` on `info`'s system, in a sentence.
pub fn no_way(a: &ManifestApp, info: &SystemInfo) -> String {
    let here = describe(info);
    if matches!(a.source, Source::AppImage | Source::Custom)
        && a.canonical.is_none()
        && a.github.is_none()
    {
        return format!(
            "{} is a program file with no known source, and no {here} version of it is known.",
            a.name
        );
    }
    match info.profile() {
        Some(p) if p.usable_manager().is_none() && p.package_manager == Adapter::Custom => format!(
            "No {here} version of {} is known: the profile's custom commands are not trusted yet (Settings → Linux distributions).",
            a.name
        ),
        _ => format!("No version of {} for {here} is known.", a.name),
    }
}

/// The group an install item falls in. `bundled`: it installs from the snapshot; `source`:
/// how the app was installed on the old computer.
pub fn verdict(
    status: Status,
    chosen: Option<&Candidate>,
    bundled: bool,
    channel: Option<Channel>,
    source: Source,
) -> Verdict {
    match status {
        Status::Unsupported => return Verdict::Unavailable,
        Status::Manual => return Verdict::Review,
        Status::Ready => {}
    }
    if bundled || channel == Some(Channel::Snapshot) {
        return Verdict::Ready;
    }
    let Some(c) = chosen else {
        return Verdict::Ready;
    };
    if c.review.is_some() || matches!(c.method, Method::Source { .. }) {
        return Verdict::Review;
    }
    let same = match (&c.method, source) {
        // The destination's own package (any distribution) or the same store as before.
        (Method::System(..) | Method::Winget(_) | Method::Brew { .. }, _) => true,
        (Method::Flathub(_), Source::Flatpak) | (Method::Snap(_), Source::Snap) => true,
        // A program file from its own release, as it was before.
        (Method::Download, Source::AppImage | Source::Custom) => true,
        _ => false,
    };
    if same {
        Verdict::Ready
    } else {
        Verdict::Alternative
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apt_policy_reports_the_candidate_origin() {
        let sources = "Package files:\n 100 /var/lib/dpkg/status\n     release a=now\n 500 http://packages.linuxmint.com wilma/main amd64 Packages\n     release v=22,o=linuxmint,a=wilma,n=wilma,l=linuxmint,c=main,b=amd64\n     origin packages.linuxmint.com\n 500 http://archive.ubuntu.com/ubuntu noble/main amd64 Packages\n     release v=24.04,o=Ubuntu,a=noble,n=noble,l=Ubuntu,c=main,b=amd64\n     origin archive.ubuntu.com\n 500 https://packages.microsoft.com/repos/code stable/main amd64 Packages\n     release o=code stable,a=stable,n=stable,l=code stable,c=main,b=amd64\n     origin packages.microsoft.com\n";
        let pkgs = "firefox:\n  Installed: (none)\n  Candidate: 1:144.0+linuxmint1+wilma\n  Version table:\n     1:144.0+linuxmint1+wilma 500\n        500 http://packages.linuxmint.com wilma/main amd64 Packages\n     1:1snap1-0ubuntu5 500\n        500 http://archive.ubuntu.com/ubuntu noble/main amd64 Packages\ngit:\n  Installed: (none)\n  Candidate: 1:2.43.0-1ubuntu7\n  Version table:\n     1:2.43.0-1ubuntu7 500\n        500 http://archive.ubuntu.com/ubuntu noble/main amd64 Packages\ncode:\n  Installed: (none)\n  Candidate: 1.100.0-1\n  Version table:\n     1.100.0-1 500\n        500 https://packages.microsoft.com/repos/code stable/main amd64 Packages\nnothere:\n  Installed: (none)\n  Candidate: (none)\n  Version table:\n";
        let o = parse_apt_policy(pkgs, sources);
        assert_eq!(o.get("firefox").map(String::as_str), Some("linuxmint"));
        assert_eq!(o.get("git").map(String::as_str), Some("Ubuntu"));
        assert_eq!(o.get("code").map(String::as_str), Some("code stable"));
        assert!(!o.contains_key("nothere"));
    }

    #[test]
    fn info_blocks_and_apk_policy() {
        let zypper = "Loading repository data...\nInformation for package MozillaFirefox:\n----------------------------------------\nRepository     : openSUSE-Tumbleweed-Oss\nName           : MozillaFirefox\nVersion        : 144.0-1.1\n\nInformation for package gimp:\nRepository     : repo-oss\nName           : gimp\n";
        let o = parse_info_blocks(zypper);
        assert_eq!(
            o.get("MozillaFirefox").map(String::as_str),
            Some("openSUSE-Tumbleweed-Oss")
        );
        assert_eq!(o.get("gimp").map(String::as_str), Some("repo-oss"));
        let pacman = "Repository      : extra\nName            : firefox\nVersion         : 144.0-1\n\nRepository      : chaotic-aur\nName            : brave-bin\n";
        let o = parse_info_blocks(pacman);
        assert_eq!(o.get("firefox").map(String::as_str), Some("extra"));
        assert_eq!(o.get("brave-bin").map(String::as_str), Some("chaotic-aur"));
        let apk = "firefox policy:\n  144.0-r0:\n    https://dl-cdn.alpinelinux.org/alpine/v3.22/community\ngit policy:\n  2.49.1-r0:\n    lib/apk/db/installed\n    https://dl-cdn.alpinelinux.org/alpine/v3.22/main\n";
        let o = parse_apk_policy(apk);
        assert_eq!(o["git"], "https://dl-cdn.alpinelinux.org/alpine/v3.22/main");
        assert!(o["firefox"].ends_with("/community"));
        assert_eq!(
            parse_name_repo("firefox updates\nfirefox fedora\n")["firefox"],
            "updates"
        );
    }
}
