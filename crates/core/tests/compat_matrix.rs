//! Cross-system compatibility scenarios: one snapshot manifest per source system, planned for
//! destination computers described by their distribution profile (no live queries), plus the
//! provenance rules with package-manager answers parsed from real output.
//!
//! The rule under test throughout: a package file is only used where the destination's
//! capabilities say it fits; the processor matching is never enough.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::Path;

use serde_json::{Value, json};
use simple_setup_core::compat::{self, Query, Verdict};
use simple_setup_core::discovery::{Confidence, Source};
use simple_setup_core::distros::{Adapter, Distros, ProfileFile, UserStore};
use simple_setup_core::manifest::{ManifestApp, Status};
use simple_setup_core::platform::{Arch, OsFamily, OsRelease, Platform};
use simple_setup_core::probe::{LinuxHost, SystemInfo};
use simple_setup_core::resolve::{Channel, Method, Prefs};
use simple_setup_core::restore::{self, PlanItem, RestorePlan, Target};

const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn x64(os: OsFamily) -> Platform {
    Platform {
        os,
        arch: Arch::X86_64,
    }
}

fn arm(os: OsFamily) -> Platform {
    Platform {
        os,
        arch: Arch::Aarch64,
    }
}

fn app(source: &str, native: &str, name: &str, canonical: Option<&str>) -> Value {
    json!({
        "id": format!("{source}:{native}"), "name": name, "version": "1.0", "source": source,
        "native_id": native, "canonical": canonical, "repository": null, "github": null,
        "confidence": "confirmed", "evidence": [], "installer": true, "config": false, "data": false
    })
}

fn with_github(mut a: Value, repo: &str) -> Value {
    a["github"] = json!({"repo": repo, "reference": null, "asset": null,
        "confidence": "confirmed", "origin": "detected"});
    a
}

fn file(stored: &str) -> Value {
    json!({"stored": stored, "size": 10, "sha256": SHA})
}

fn rpm_bundle(app_id: &str, pkg: &str, release: &str, p: Platform) -> Value {
    json!({"app": app_id, "platform": p, "evidence": [], "recipe": {
        "kind": "rpm", "distro": release, "packages": [pkg],
        "rpms": [{"name": pkg, "file": file(&format!("installers/rpm-{pkg}/rpms/{pkg}.rpm"))}]}})
}

fn deb_bundle(app_id: &str, pkg: &str, p: Platform) -> Value {
    json!({"app": app_id, "platform": p, "evidence": [], "recipe": {
        "kind": "deb", "package": pkg, "file": file(&format!("installers/{p}/{pkg}/{pkg}.deb"))}})
}

fn flatpak_bundle(app_id: &str, id: &str, p: Platform) -> Value {
    json!({"app": app_id, "platform": p, "evidence": [], "recipe": {
        "kind": "flatpak", "app_ref": format!("app/{id}/{}/stable", p.arch),
        "bundles": [{"name": format!("app/{id}/{}/stable", p.arch),
                     "file": file(&format!("installers/flatpak/{id}.flatpak"))}]}})
}

/// A snapshot made on `source` (`release`: its distribution, if Linux) holding `apps` and
/// `installers`, written into a fresh folder.
fn snapshot(
    source: Platform,
    release: Option<&str>,
    apps: Vec<Value>,
    installers: Vec<Value>,
) -> tempfile::TempDir {
    let t = tempfile::tempdir().unwrap();
    let mut m = json!({
        "schema_version": 2, "id": "matrix", "name": "Matrix", "created_at": "2026-11-11T00:00:00Z",
        "generator": "test",
        "source": {"hostname": "old", "os_name": "old", "platform": source},
        "targets": [source], "selection": {"apps": [], "folders": [], "targets": [source]},
        "apps": apps, "roots": [], "files": [], "installers": installers, "links": [],
        "excluded": {"secrets": 0, "transient": 0, "by_user": 0, "unreadable": 0, "special": 0,
                     "secret_examples": [], "unreadable_examples": []},
        "coverage": [], "readiness": [], "warnings": [], "total_bytes": 0
    });
    if let Some(r) = release {
        let id = r.split(' ').next().unwrap();
        let p = Distros::builtin().resolve(id).unwrap();
        m["source_linux"] = json!({"release": r, "profile": p.id, "namespaces": p.namespaces});
    }
    std::fs::write(t.path().join("simple-setup.json"), m.to_string()).unwrap();
    t
}

/// A destination computer: `profile[:release]` on Linux, or Windows/macOS.
fn dest(p: Platform, profile: Option<&str>, online: bool) -> Target {
    dest_with(p, profile, online, &Distros::builtin())
}

fn dest_with(p: Platform, profile: Option<&str>, online: bool, distros: &Distros) -> Target {
    let (id, release) = match profile.and_then(|s| s.split_once(':')) {
        Some((i, r)) => (Some(i), Some(r)),
        None => (profile, None),
    };
    let resolved = id.map(|i| distros.resolve(i).unwrap());
    let mut info = SystemInfo::assumed(p, resolved);
    if let (Some(l), Some(r)) = (info.linux.as_mut(), release) {
        l.os_release.version_id = Some(r.into());
        info.distro = Some(l.os_release.release());
    }
    info.online = online;
    Target::described(info)
}

fn plan(snap: &tempfile::TempDir, t: &Target) -> RestorePlan {
    restore::plan(snap.path(), t).unwrap()
}

fn item<'a>(p: &'a RestorePlan, app: &str) -> &'a PlanItem {
    p.items
        .iter()
        .find(|i| i.kind == restore::ItemKind::Installer && i.app.as_deref() == Some(app))
        .unwrap_or_else(|| panic!("no install item for {app}: {:#?}", p.items))
}

fn explain(i: &PlanItem) -> String {
    format!(
        "{:?} {:?} via {:?}: {} | {:?}",
        i.verdict, i.status, i.method, i.reason, i.considered
    )
}

#[test]
fn fedora_to_linux_mint() {
    let src = x64(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("fedora 44"),
        vec![
            app("rpm", "firefox", "Firefox", Some("firefox")),
            app("rpm", "gimp", "GIMP", Some("gimp")),
            with_github(app("app_image", "MyTool", "My Tool", None), "me/mytool"),
            app("rpm", "fedora-only-tool", "Fedora-only Tool", None),
        ],
        vec![
            rpm_bundle("rpm:firefox", "firefox", "fedora 44", src),
            rpm_bundle("rpm:gimp", "gimp", "fedora 44", src),
        ],
    );
    let p = plan(&snap, &dest(src, Some("linuxmint:22"), true));
    let ff = item(&p, "Firefox");
    // Same processor, but the Fedora RPMs are never used on Mint.
    assert_ne!(ff.channel, Some(Channel::Snapshot), "{}", explain(ff));
    assert!(
        ff.considered
            .iter()
            .any(|c| c.option == "The installer in the snapshot"
                && c.outcome.contains("RPM packages are never converted")),
        "{}",
        explain(ff)
    );
    // Mint packages Firefox itself (its rule overrides Ubuntu's snap-only package).
    assert_eq!(
        ff.method.as_deref(),
        Some("APT package firefox"),
        "{}",
        explain(ff)
    );
    assert_eq!(ff.verdict, Verdict::Ready);
    let gimp = item(&p, "GIMP");
    assert_eq!(
        gimp.method.as_deref(),
        Some("APT package gimp"),
        "{}",
        explain(gimp)
    );
    assert_eq!(gimp.verdict, Verdict::Ready);
    let tool = item(&p, "My Tool");
    assert_eq!(
        tool.method.as_deref(),
        Some("a verified download from its GitHub release")
    );
    let only = item(&p, "Fedora-only Tool");
    assert_eq!(
        (only.status, only.verdict),
        (Status::Unsupported, Verdict::Unavailable)
    );
    assert!(only.reason.contains("Linux Mint"), "{}", explain(only));
    // Snap is switched off on Mint: never planned.
    assert!(
        !p.items
            .iter()
            .any(|i| i.method.as_deref().is_some_and(|m| m.starts_with("snap"))),
        "{:#?}",
        p.items
    );
}

#[test]
fn ubuntu_to_fedora_refuses_the_deb() {
    let src = x64(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("ubuntu 24.04"),
        vec![
            app("dpkg", "firefox", "Firefox", Some("firefox")),
            app("dpkg", "ubuntu-only-tool", "Ubuntu Tool", None),
        ],
        vec![deb_bundle("dpkg:firefox", "firefox", src)],
    );
    let p = plan(&snap, &dest(src, Some("fedora:44"), true));
    let ff = item(&p, "Firefox");
    assert_ne!(ff.channel, Some(Channel::Snapshot), "{}", explain(ff));
    assert!(
        ff.considered[0].outcome.contains("DEB distributions"),
        "{}",
        explain(ff)
    );
    assert_eq!(ff.method.as_deref(), Some("DNF package firefox"));
    assert_eq!(item(&p, "Ubuntu Tool").verdict, Verdict::Unavailable);
}

#[test]
fn debian_arm64_to_ubuntu_arm64_uses_the_deb() {
    let src = arm(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("debian 12"),
        vec![
            app("dpkg", "git", "Git", Some("git")),
            // A program file built for x86-64 only, with no other source.
            app("app_image", "OldTool", "Old Tool", None),
        ],
        vec![deb_bundle("dpkg:git", "git", src)],
    );
    let p = plan(&snap, &dest(src, Some("ubuntu:24.04"), true));
    let git = item(&p, "Git");
    // A .deb fits every DEB distribution of the same processor.
    assert_eq!(git.channel, Some(Channel::Snapshot), "{}", explain(git));
    assert_eq!(git.verdict, Verdict::Ready);
    assert_eq!(item(&p, "Old Tool").verdict, Verdict::Unavailable);
    // On x86-64 the ARM64 .deb is not even looked at (installers are per platform).
    let p = plan(
        &snap,
        &dest(x64(OsFamily::Linux), Some("ubuntu:24.04"), true),
    );
    let git = item(&p, "Git");
    assert_ne!(git.channel, Some(Channel::Snapshot));
    assert_eq!(git.method.as_deref(), Some("APT package git"));
}

#[test]
fn arch_to_fedora() {
    let src = x64(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("arch"),
        vec![
            app("pacman", "firefox", "Firefox", Some("firefox")),
            app("pacman", "yay", "yay", None),
        ],
        vec![],
    );
    let p = plan(&snap, &dest(src, Some("fedora:44"), true));
    assert_eq!(
        item(&p, "Firefox").method.as_deref(),
        Some("DNF package firefox")
    );
    // An Arch package name means nothing to DNF.
    assert_eq!(item(&p, "yay").verdict, Verdict::Unavailable);
}

#[test]
fn fedora_to_windows_and_back() {
    let lin = x64(OsFamily::Linux);
    let win = x64(OsFamily::Windows);
    let snap = snapshot(
        lin,
        Some("fedora 44"),
        vec![
            app("rpm", "firefox", "Firefox", Some("firefox")),
            app("rpm", "fedora-only-tool", "Fedora-only Tool", None),
        ],
        vec![rpm_bundle("rpm:firefox", "firefox", "fedora 44", lin)],
    );
    let p = plan(&snap, &dest(win, None, true));
    let ff = item(&p, "Firefox");
    assert_eq!(ff.method.as_deref(), Some("winget (Mozilla.Firefox)"));
    assert_eq!(ff.verdict, Verdict::Ready);
    assert_eq!(item(&p, "Fedora-only Tool").verdict, Verdict::Unavailable);

    let snap = snapshot(
        win,
        None,
        vec![
            app("arp", "Mozilla Firefox", "Mozilla Firefox", Some("firefox")),
            app("arp", "Paint.NET", "Paint.NET", None),
        ],
        vec![],
    );
    let p = plan(&snap, &dest(lin, Some("linuxmint:22"), true));
    assert_eq!(
        item(&p, "Mozilla Firefox").method.as_deref(),
        Some("APT package firefox")
    );
    assert_eq!(item(&p, "Paint.NET").verdict, Verdict::Unavailable);
}

#[test]
fn macos_to_linux() {
    let snap = snapshot(
        arm(OsFamily::Macos),
        None,
        vec![app("brew_cask", "firefox", "Firefox", Some("firefox"))],
        vec![],
    );
    let p = plan(&snap, &dest(x64(OsFamily::Linux), Some("fedora:44"), true));
    assert_eq!(
        item(&p, "Firefox").method.as_deref(),
        Some("DNF package firefox")
    );
}

#[test]
fn same_distribution_uses_the_bundle_only_on_its_release() {
    let src = x64(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("fedora 44"),
        vec![app("rpm", "firefox", "Firefox", Some("firefox"))],
        vec![rpm_bundle("rpm:firefox", "firefox", "fedora 44", src)],
    );
    let p = plan(&snap, &dest(src, Some("fedora:44"), false));
    let ff = item(&p, "Firefox");
    assert_eq!(
        (ff.channel, ff.verdict),
        (Some(Channel::Snapshot), Verdict::Ready)
    );
    assert!(p.offline_possible);
    // Another release of the same distribution: online with DNF instead.
    let p = plan(&snap, &dest(src, Some("fedora:43"), true));
    let ff = item(&p, "Firefox");
    assert_ne!(ff.channel, Some(Channel::Snapshot), "{}", explain(ff));
    assert!(
        ff.considered[0]
            .outcome
            .contains("only install offline on that release")
    );
    assert_eq!(ff.method.as_deref(), Some("DNF package firefox"));
    // …and offline it cannot be installed at all: it needs a download.
    let p = plan(&snap, &dest(src, Some("fedora:43"), false));
    let ff = item(&p, "Firefox");
    assert_eq!(
        (ff.status, ff.verdict),
        (Status::Manual, Verdict::Review),
        "{}",
        explain(ff)
    );
    assert!(!p.offline_possible);
}

#[test]
fn offline_restores_report_what_needs_the_network() {
    let src = x64(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("fedora 44"),
        vec![
            app("flatpak", "org.gimp.GIMP", "GIMP", Some("gimp")),
            app("rpm", "git", "Git", Some("git")),
        ],
        vec![flatpak_bundle(
            "flatpak:org.gimp.GIMP",
            "org.gimp.GIMP",
            src,
        )],
    );
    // A Mint computer with no internet: the Flatpak bundle fits any distribution.
    let p = plan(&snap, &dest(src, Some("linuxmint:22"), false));
    let gimp = item(&p, "GIMP");
    assert_eq!(
        (gimp.channel, gimp.verdict),
        (Some(Channel::Snapshot), Verdict::Ready)
    );
    let git = item(&p, "Git");
    assert_eq!(git.status, Status::Manual, "{}", explain(git));
    assert!(git.reason.contains("Needs internet"), "{}", explain(git));
    assert!(!p.offline_possible);
}

#[test]
fn immutable_and_musl_destinations() {
    let src = x64(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("fedora 44"),
        vec![
            app("rpm", "firefox", "Firefox", Some("firefox")),
            with_github(app("app_image", "MyTool", "My Tool", None), "me/mytool"),
        ],
        vec![rpm_bundle("rpm:firefox", "firefox", "fedora 44", src)],
    );
    // Fedora Silverblue, same release: the RPMs do not go onto a read-only system, and
    // Flatpak comes first by the profile's rule.
    let p = plan(&snap, &dest(src, Some("fedora-atomic:44"), true));
    let ff = item(&p, "Firefox");
    assert_ne!(ff.channel, Some(Channel::Snapshot), "{}", explain(ff));
    assert!(
        ff.considered[0].outcome.contains("read-only"),
        "{}",
        explain(ff)
    );
    assert_eq!(
        ff.method.as_deref(),
        Some("Flatpak from Flathub (org.mozilla.firefox)")
    );
    assert_eq!(ff.verdict, Verdict::Alternative);
    // Layering the RPM is still possible, but only with approval.
    assert!(
        ff.considered
            .iter()
            .any(|c| c.option.starts_with("rpm-ostree package"))
    );
    // Alpine (musl): downloaded Linux programs do not run.
    let p = plan(&snap, &dest(src, Some("alpine:3.22"), true));
    assert_eq!(item(&p, "My Tool").verdict, Verdict::Unavailable);
}

#[test]
fn an_imported_profile_makes_an_unknown_distribution_work() {
    let t = tempfile::tempdir().unwrap();
    let us = UserStore::at(t.path());
    let f: ProfileFile = simple_setup_core::distros::parse(
        r#"{"schema_version":1,"distros":[{"id":"nimbus","name":"Nimbus OS","parent":"ubuntu","match":{"ids":["nimbus"]}}]}"#,
        false,
    )
    .unwrap();
    us.import(&f).unwrap();
    let d = us.distros();
    let os = OsRelease::parse("ID=nimbus\nNAME=\"Nimbus OS\"\nVERSION_ID=3\n").unwrap();
    // Without the profile it is unknown (no ID_LIKE): only portable formats.
    assert_eq!(
        Distros::builtin().detect(&os).0.package_manager,
        Adapter::None
    );
    let host = LinuxHost::from_parts(os, &d, None, None, "x86_64".into(), false, true);
    assert_eq!(host.profile.id, "nimbus");
    let src = x64(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("fedora 44"),
        vec![
            app("rpm", "firefox", "Firefox", Some("firefox")),
            app("rpm", "gimp", "GIMP", Some("gimp")),
        ],
        vec![],
    );
    let p = plan(&snap, &dest_with(src, Some("nimbus"), true, &d));
    // Inherited from Ubuntu: APT names, and Ubuntu's rule that its firefox package is only a
    // snap, so the real snap comes first.
    assert_eq!(item(&p, "GIMP").method.as_deref(), Some("APT package gimp"));
    let ff = item(&p, "Firefox");
    assert_eq!(
        ff.method.as_deref(),
        Some("snap firefox"),
        "{}",
        explain(ff)
    );
    assert!(
        ff.considered
            .iter()
            .any(|c| c.outcome.contains("only installs the Firefox snap"))
    );
}

#[test]
fn unsupported_processor_is_reported() {
    let snap = snapshot(
        x64(OsFamily::Linux),
        Some("fedora 44"),
        vec![app("custom", "/home/me/bin/tool", "Tool", None)],
        vec![],
    );
    let p = plan(&snap, &dest(arm(OsFamily::Linux), Some("fedora:44"), true));
    let tool = item(&p, "Tool");
    assert_eq!(tool.verdict, Verdict::Unavailable, "{}", explain(tool));
}

/// Package-manager answers from fixtures, parsed by the real parsers.
struct Fixture(HashMap<String, String>);

impl Query for Fixture {
    fn origins(&mut self, _: Adapter, names: &[String]) -> Option<HashMap<String, String>> {
        Some(
            names
                .iter()
                .filter_map(|n| Some((n.clone(), self.0.get(n)?.clone())))
                .collect(),
        )
    }
}

fn mapp(source: Source, native: &str, canonical: Option<&str>) -> ManifestApp {
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
fn provenance_decides_between_repositories() {
    let sources = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/packages/mint22-apt-cache-policy.txt"),
    )
    .unwrap();
    let pkgs = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/packages/mint22-apt-cache-policy-pkgs.txt"),
    )
    .unwrap();
    let mut q = Fixture(compat::parse_apt_policy(&pkgs, &sources));
    let mint = Distros::builtin().resolve("linuxmint").unwrap();
    let mut info = SystemInfo::assumed(x64(OsFamily::Linux), Some(mint));
    info.tools.push("flatpak".into());
    let from = vec!["rpm".to_string()];
    // Offered by Mint's own repository: chosen without asking.
    let (c, _) = compat::candidates(
        &mapp(Source::Rpm, "firefox", Some("firefox")),
        &from,
        &info,
        &Prefs::default(),
        &mut q,
    );
    assert_eq!(c[0].method, Method::System(Adapter::Apt, "firefox".into()));
    assert!(c[0].review.is_none() && c[0].confirmed);
    // Not offered at all (the registry's alternative name is not in the lists): dropped.
    assert!(
        !c.iter()
            .any(|x| x.method == Method::System(Adapter::Apt, "firefox-esr".into()))
    );
    // Only in a third-party repository: Flathub first, the repository's package last and only
    // with approval.
    let (c, considered) = compat::candidates(
        &mapp(Source::Rpm, "code", Some("vscode")),
        &from,
        &info,
        &Prefs::default(),
        &mut q,
    );
    assert!(matches!(c[0].method, Method::Flathub(_)), "{c:?}");
    let third = c
        .iter()
        .find(|x| matches!(&x.method, Method::System(_, n) if n == "code"))
        .unwrap_or_else(|| panic!("{c:?} {considered:?}"));
    assert!(
        third
            .review
            .as_deref()
            .unwrap()
            .contains("not one of Linux Mint's own")
    );
    let v = compat::verdict(
        Status::Ready,
        Some(third),
        false,
        Some(Channel::Online),
        Source::Rpm,
    );
    assert_eq!(v, Verdict::Review);
}

#[test]
fn github_releases_pick_the_destination_format_and_processor() {
    let rel: simple_setup_core::catalog::Release = serde_json::from_str(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/catalog/github-rustdesk.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let pick = |p: Platform, deb: bool| {
        simple_setup_core::catalog::github_asset(&rel, p, deb)
            .unwrap()
            .name
    };
    let mint = SystemInfo::assumed(
        x64(OsFamily::Linux),
        Distros::builtin().resolve("linuxmint").ok(),
    );
    let fedora = SystemInfo::assumed(
        x64(OsFamily::Linux),
        Distros::builtin().resolve("fedora").ok(),
    );
    assert!(compat::wants_deb(&mint) && !compat::wants_deb(&fedora));
    assert_eq!(
        pick(x64(OsFamily::Linux), compat::wants_deb(&mint)),
        "rustdesk-1.5.0-x86_64.deb"
    );
    assert_eq!(
        pick(arm(OsFamily::Linux), compat::wants_deb(&mint)),
        "rustdesk-1.5.0-aarch64.deb"
    );
    // Fedora never gets the .deb, even on the same processor.
    assert_eq!(
        pick(x64(OsFamily::Linux), compat::wants_deb(&fedora)),
        "rustdesk-1.5.0-x86_64.AppImage"
    );
    assert_eq!(
        pick(arm(OsFamily::Linux), false),
        "rustdesk-1.5.0-aarch64.AppImage"
    );
    assert!(
        pick(x64(OsFamily::Windows), false).ends_with(".msi")
            || pick(x64(OsFamily::Windows), false).ends_with(".exe")
    );
}

#[test]
fn version_1_snapshots_are_migrated() {
    let src = x64(OsFamily::Linux);
    let t = snapshot(
        src,
        None,
        vec![app("rpm", "firefox", "Firefox", Some("firefox"))],
        vec![rpm_bundle("rpm:firefox", "firefox", "fedora 44", src)],
    );
    let p = t.path().join("simple-setup.json");
    let mut m: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    m["schema_version"] = 1.into();
    m["selection"]["linux_distro"] = "linuxmint".into();
    m["coverage"] = json!([{"app": "rpm:firefox", "target": src, "part": "installer", "status": "manual", "reason": "x"}]);
    std::fs::write(&p, m.to_string()).unwrap();
    let read = simple_setup_core::manifest::Manifest::read(t.path()).unwrap();
    let sl = read.source_linux.as_ref().unwrap();
    assert_eq!(
        (sl.release.as_str(), sl.profile.as_str()),
        ("fedora 44", "fedora")
    );
    assert_eq!(read.coverage[0].verdict, Some(Verdict::Review));
    assert_eq!(
        read.destination.as_ref().unwrap().profile.as_deref(),
        Some("linuxmint")
    );
    // And it plans like a new one.
    let pl = plan(&t, &dest(src, Some("linuxmint:22"), true));
    assert_eq!(
        item(&pl, "Firefox").method.as_deref(),
        Some("APT package firefox")
    );
    // A snapshot from a newer Simple Setup says so.
    m["schema_version"] = 3.into();
    std::fs::write(&p, m.to_string()).unwrap();
    let e = simple_setup_core::manifest::Manifest::read(t.path())
        .unwrap_err()
        .to_string();
    assert!(e.contains("newer Simple Setup"), "{e}");
}

#[test]
fn rpm_names_differ_between_fedora_and_opensuse() {
    let src = x64(OsFamily::Linux);
    let snap = snapshot(
        src,
        Some("fedora 44"),
        vec![
            app("rpm", "firefox", "Firefox", Some("firefox")),
            app("rpm", "fedora-only-tool", "Fedora-only Tool", None),
        ],
        vec![rpm_bundle("rpm:firefox", "firefox", "fedora 44", src)],
    );
    // Both RPM-based, same processor: the Fedora RPMs and Fedora's names are not used.
    let p = plan(&snap, &dest(src, Some("opensuse-tumbleweed"), true));
    let ff = item(&p, "Firefox");
    assert_ne!(ff.channel, Some(Channel::Snapshot), "{}", explain(ff));
    assert_eq!(ff.method.as_deref(), Some("Zypper package MozillaFirefox"));
    assert_eq!(item(&p, "Fedora-only Tool").verdict, Verdict::Unavailable);
    // Alpine has its own names too.
    let p = plan(&snap, &dest(src, Some("alpine:3.22"), true));
    assert_eq!(
        item(&p, "Firefox").method.as_deref(),
        Some("apk package firefox")
    );
}
