//! macOS adapters: application bundles (`/Applications`, `~/Applications`), Homebrew formulae and
//! casks, MacPorts ports and installer-package receipts. Parsers are pure and fixture-tested on
//! any OS; reading runs only read-only listings, with Homebrew's auto-update and analytics off.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;

use super::{App, Confidence, Ctx, GitHub, Kind, LocKind, Origin, Source, git, output, run};

pub fn scan(cx: &mut Ctx) {
    let app_dirs = [PathBuf::from("/Applications"), cx.home.join("Applications")];
    brew(cx, &app_dirs);
    if let Some(text) = run("/opt/local/bin/port", &["-q", "installed", "requested"]) {
        cx.apps.extend(port_apps(&text));
    }
    let bundles = bundle_apps(&app_dirs, cx.home);
    let ids: Vec<String> = bundles.iter().map(|b| b.native_id.clone()).collect();
    add_bundles(&mut cx.apps, bundles);
    let (apps, hidden) = receipt_apps(Path::new("/var/db/receipts"), &ids);
    cx.apps.extend(apps);
    cx.hidden += hidden;
}

fn github(url: Option<&str>, app: &mut App) {
    if let Some(repo) = url.and_then(git::github_repo) {
        app.evidence.push(format!("Homepage is github.com/{repo}"));
        app.github = Some(GitHub {
            repo,
            reference: app.version.clone(),
            asset: None,
            confidence: Confidence::Probable,
            origin: Origin::Detected,
            commit: None,
            build: None,
        });
    }
}

/// Reverse-DNS identifiers become folder names, so only plain ones are used.
fn plain_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
}

// ---------------------------------------------------------------- App bundles

#[derive(Debug, Deserialize)]
struct InfoPlist {
    #[serde(rename = "CFBundleIdentifier")]
    id: Option<String>,
    #[serde(rename = "CFBundleShortVersionString")]
    version: Option<String>,
}

/// `.app` bundles directly in each folder or one level down (vendor folders such as
/// "/Applications/Microsoft Office"). Info.plist may be XML or binary.
pub fn bundle_apps(dirs: &[PathBuf], home: &Path) -> Vec<App> {
    let is_app = |p: &Path| p.extension().is_some_and(|e| e == "app");
    let mut paths = Vec::new();
    for d in dirs {
        let Ok(rd) = fs::read_dir(d) else { continue };
        for p in rd.flatten().map(|e| e.path()) {
            if is_app(&p) {
                paths.push(p);
            } else if p.is_dir()
                && !p.is_symlink()
                && let Ok(sub) = fs::read_dir(&p)
            {
                paths.extend(sub.flatten().map(|e| e.path()).filter(|p| is_app(p)));
            }
        }
    }
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| bundle_app(p, home))
        .collect()
}

fn bundle_app(path: PathBuf, home: &Path) -> Option<App> {
    let info: InfoPlist = plist::from_file(path.join("Contents/Info.plist")).ok()?;
    let id = info.id.filter(|i| plain_id(i))?;
    let name = path.file_stem()?.to_string_lossy().into_owned();
    let mut app = App::new(Source::AppBundle, &id, &name, Kind::Desktop);
    app.version = info.version;
    app.system = id.starts_with("com.apple.");
    app.evidence
        .push(format!("App bundle {} ({id})", path.display()));
    if path.join("Contents/_MASReceipt/receipt").is_file() {
        app.repository = Some("Mac App Store".into());
        app.evidence.push("Has a Mac App Store receipt".into());
    }
    let lib = home.join("Library");
    app.add_location(
        LocKind::Config,
        lib.join("Preferences").join(format!("{id}.plist")),
        Origin::Detected,
    );
    app.add_location(
        LocKind::Data,
        lib.join("Application Support").join(&id),
        Origin::Detected,
    );
    app.add_location(
        LocKind::Data,
        lib.join("Containers").join(&id),
        Origin::Detected,
    );
    app.path = Some(path);
    Some(app)
}

/// Bundles that a Homebrew cask installed enrich that cask instead of being listed twice.
pub fn add_bundles(apps: &mut Vec<App>, bundles: Vec<App>) {
    for b in bundles {
        let Some(cask) = apps
            .iter_mut()
            .find(|a| a.source == Source::BrewCask && a.path.is_some() && a.path == b.path)
        else {
            apps.push(b);
            continue;
        };
        cask.evidence.extend(b.evidence);
        cask.version = cask.version.take().or(b.version);
        for l in b.locations {
            if !cask.locations.iter().any(|x| x.path == l.path) {
                cask.locations.push(l);
            }
        }
    }
}

// ---------------------------------------------------------------- Homebrew

#[derive(Debug, Deserialize)]
struct BrewInfo {
    #[serde(default)]
    formulae: Vec<Formula>,
    #[serde(default)]
    casks: Vec<Cask>,
}

#[derive(Debug, Deserialize)]
struct Formula {
    name: String,
    full_name: String,
    tap: Option<String>,
    homepage: Option<String>,
    #[serde(default)]
    installed: Vec<Keg>,
}

#[derive(Debug, Deserialize)]
struct Keg {
    version: String,
    #[serde(default)]
    installed_on_request: bool,
}

#[derive(Debug, Deserialize)]
struct Cask {
    token: String,
    full_token: String,
    tap: Option<String>,
    #[serde(default)]
    name: Vec<String>,
    homepage: Option<String>,
    installed: Option<String>,
    #[serde(default)]
    artifacts: Vec<serde_json::Value>,
}

/// `.app` names a cask installs: `{"app": ["Foo.app"]}` or `{"app": ["Foo.app", {"target": "Bar.app"}]}`.
fn cask_app_names(artifacts: &[serde_json::Value]) -> Vec<String> {
    artifacts
        .iter()
        .filter_map(|a| a.get("app")?.as_array())
        .filter_map(|list| {
            let target = list.iter().find_map(|v| v.get("target")?.as_str());
            let name = target.or_else(|| list.iter().find_map(|v| v.as_str()))?;
            Some(Path::new(name).file_name()?.to_string_lossy().into_owned())
        })
        .collect()
}

/// Apps from `brew info --json=v2 --installed`. Formulae pulled in only as dependencies are
/// counted as hidden. Casks point at the bundle they installed in `app_dirs`, if it is there.
pub fn brew_apps(json: &str, app_dirs: &[PathBuf]) -> (Vec<App>, usize) {
    let Ok(info) = serde_json::from_str::<BrewInfo>(json) else {
        return (Vec::new(), 0);
    };
    let (mut apps, mut hidden) = (Vec::new(), 0);
    for f in info.formulae {
        if !f.installed.iter().any(|k| k.installed_on_request) {
            hidden += 1;
            continue;
        }
        let mut app = App::new(Source::Brew, &f.full_name, &f.name, Kind::CommandLine);
        app.version = f.installed.last().map(|k| k.version.clone());
        app.repository = f.tap;
        app.evidence
            .push(format!("Installed with Homebrew (formula {})", f.full_name));
        github(f.homepage.as_deref(), &mut app);
        apps.push(app);
    }
    for c in info.casks {
        let name = c.name.first().cloned().unwrap_or_else(|| c.token.clone());
        let mut app = App::new(Source::BrewCask, &c.full_token, &name, Kind::Desktop);
        app.version = c.installed;
        app.repository = c.tap;
        app.evidence
            .push(format!("Installed with Homebrew (cask {})", c.full_token));
        app.path = cask_app_names(&c.artifacts)
            .iter()
            .flat_map(|n| app_dirs.iter().map(move |d| d.join(n)))
            .find(|p| p.is_dir());
        github(c.homepage.as_deref(), &mut app);
        apps.push(app);
    }
    (apps, hidden)
}

fn brew(cx: &mut Ctx, app_dirs: &[PathBuf]) {
    let Some(brew) = ["/opt/homebrew/bin/brew", "/usr/local/bin/brew"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
    else {
        return;
    };
    let listed = output(
        Command::new(&brew)
            .args(["info", "--json=v2", "--installed"])
            .env("HOMEBREW_NO_AUTO_UPDATE", "1")
            .env("HOMEBREW_NO_ANALYTICS", "1")
            .env("HOMEBREW_NO_ENV_HINTS", "1"),
    );
    let Some((true, json)) = listed else {
        cx.notes
            .push("Homebrew is installed, but its package list could not be read.".into());
        return;
    };
    let (apps, hidden) = brew_apps(&json, app_dirs);
    cx.apps.extend(apps);
    cx.hidden += hidden;
}

// ---------------------------------------------------------------- MacPorts

/// `port -q installed requested`: "  git @2.45.2_0+doc+pcre (active)". Inactive versions are skipped.
pub fn port_apps(text: &str) -> Vec<App> {
    text.lines()
        .filter(|l| l.trim_end().ends_with("(active)"))
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let name = f.next()?;
            let version = f.next()?.strip_prefix('@')?.split('+').next()?;
            let mut app = App::new(Source::Macports, name, name, Kind::CommandLine);
            app.version = Some(version.into());
            app.evidence
                .push(format!("Installed with MacPorts (port {name})"));
            Some(app)
        })
        .collect()
}

// ---------------------------------------------------------------- Installer receipts

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Receipt {
    package_identifier: String,
    package_version: Option<String>,
}

/// Receipts in `/var/db/receipts` for packages that are not part of macOS and not from the
/// vendor of an app bundle already listed (those are usually that app's own installer).
pub fn receipt_apps(dir: &Path, bundle_ids: &[String]) -> (Vec<App>, usize) {
    let vendor = |id: &str| id.split('.').take(2).collect::<Vec<_>>().join(".");
    let vendors: HashSet<String> = bundle_ids.iter().map(|i| vendor(i)).collect();
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "plist"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let (mut apps, mut hidden) = (Vec::new(), 0);
    for f in files {
        let Ok(r) = plist::from_file::<_, Receipt>(&f) else {
            continue;
        };
        let id = r.package_identifier;
        if id.starts_with("com.apple.") || vendors.contains(&vendor(&id)) {
            hidden += 1;
            continue;
        }
        let mut app = App::new(Source::Pkg, &id, &id, Kind::CommandLine);
        app.version = r.package_version;
        app.evidence
            .push(format!("Installer package receipt {}", f.display()));
        apps.push(app);
    }
    (apps, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_bundle(dir: &Path, name: &str, id: &str, binary: bool) -> PathBuf {
        let app = dir.join(format!("{name}.app"));
        fs::create_dir_all(app.join("Contents")).unwrap();
        let mut d = plist::Dictionary::new();
        d.insert("CFBundleIdentifier".into(), id.into());
        d.insert("CFBundleShortVersionString".into(), "1.2.3".into());
        let v = plist::Value::Dictionary(d);
        let f = app.join("Contents/Info.plist");
        if binary {
            v.to_file_binary(&f).unwrap();
        } else {
            v.to_file_xml(&f).unwrap();
        }
        app
    }

    #[test]
    fn bundles_casks_and_receipts() {
        let t = tempfile::tempdir().unwrap();
        let (apps_dir, home) = (t.path().join("Applications"), t.path().join("home"));
        let ff = write_bundle(&apps_dir, "Firefox", "org.mozilla.firefox", true);
        write_bundle(
            &apps_dir.join("Vendor Suite"),
            "Editor",
            "com.vendor.editor",
            false,
        );
        write_bundle(&apps_dir, "Safari", "com.apple.Safari", false);
        write_bundle(&apps_dir, "Evil", "../../etc", false);
        fs::create_dir_all(ff.join("Contents/_MASReceipt")).unwrap();
        fs::write(ff.join("Contents/_MASReceipt/receipt"), b"x").unwrap();
        let prefs = home.join("Library/Preferences/org.mozilla.firefox.plist");
        fs::create_dir_all(prefs.parent().unwrap()).unwrap();
        fs::write(&prefs, b"x").unwrap();

        let bundles = bundle_apps(&[apps_dir.clone(), home.join("Applications")], &home);
        let ids: Vec<&str> = bundles.iter().map(|b| b.native_id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "org.mozilla.firefox",
                "com.apple.Safari",
                "com.vendor.editor"
            ]
        );
        let fx = &bundles[0];
        assert_eq!(
            (fx.name.as_str(), fx.version.as_deref()),
            ("Firefox", Some("1.2.3"))
        );
        assert_eq!(fx.repository.as_deref(), Some("Mac App Store"));
        assert_eq!(fx.locations[0].path, prefs);
        assert!(bundles[1].system && !fx.system);

        let json = r#"{"formulae":[
            {"name":"gh","full_name":"gh","tap":"homebrew/core","homepage":"https://github.com/cli/cli","installed":[{"version":"2.62.0","installed_on_request":true,"installed_as_dependency":false}]},
            {"name":"openssl@3","full_name":"openssl@3","tap":"homebrew/core","homepage":"https://openssl.org","installed":[{"version":"3.4.0","installed_on_request":false,"installed_as_dependency":true}]}],
          "casks":[{"token":"firefox","full_token":"firefox","tap":"homebrew/cask","name":["Mozilla Firefox"],"homepage":"https://www.mozilla.org/firefox/","installed":"133.0","artifacts":[{"uninstall":[{"quit":"org.mozilla.firefox"}]},{"app":["Firefox.app"]},{"zap":[{"trash":["~/Library/Caches/Firefox"]}]}]},
                   {"token":"tool","full_token":"me/tap/tool","tap":"me/tap","name":[],"installed":"1","artifacts":[{"app":["Tool.app",{"target":"Renamed Tool.app"}]}]}]}"#;
        let (mut apps, hidden) = brew_apps(json, std::slice::from_ref(&apps_dir));
        assert_eq!(hidden, 1);
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].github.as_ref().unwrap().repo, "cli/cli");
        assert_eq!(apps[1].path.as_deref(), Some(ff.as_path()));
        assert_eq!(apps[2].name, "tool");
        assert_eq!(
            cask_app_names(
                &serde_json::from_str::<Vec<serde_json::Value>>(
                    r#"[{"app":["Tool.app",{"target":"Renamed Tool.app"}]}]"#
                )
                .unwrap()
            ),
            ["Renamed Tool.app"]
        );
        assert!(brew_apps("garbage", &[]).0.is_empty());

        add_bundles(&mut apps, bundles);
        let fox: Vec<&App> = apps.iter().filter(|a| a.name.contains("Firefox")).collect();
        assert_eq!(fox.len(), 1, "cask and its bundle are one app");
        assert_eq!(fox[0].source, Source::BrewCask);
        assert_eq!(fox[0].locations[0].path, prefs);
        assert_eq!(apps.len(), 5);

        let receipts = t.path().join("receipts");
        fs::create_dir_all(&receipts).unwrap();
        for (i, id) in [
            "com.apple.pkg.CLTools_Executables",
            "com.vendor.editor.pkg",
            "org.nodejs.node.pkg",
        ]
        .iter()
        .enumerate()
        {
            let mut d = plist::Dictionary::new();
            d.insert("PackageIdentifier".into(), (*id).into());
            d.insert("PackageVersion".into(), "20.1.0".into());
            plist::Value::Dictionary(d)
                .to_file_xml(receipts.join(format!("{i}.plist")))
                .unwrap();
        }
        let ids: Vec<String> = apps
            .iter()
            .filter(|a| a.source == Source::AppBundle)
            .map(|a| a.native_id.clone())
            .collect();
        let (pkgs, hidden) = receipt_apps(&receipts, &ids);
        assert_eq!(hidden, 2);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(
            (pkgs[0].native_id.as_str(), pkgs[0].version.as_deref()),
            ("org.nodejs.node.pkg", Some("20.1.0"))
        );
    }

    #[test]
    fn macports() {
        let text = "  git @2.45.2_0+credential_osxkeychain+doc (active)\n  python312 @3.12.4_0+lto\n  python312 @3.12.5_0+lto (active)\n";
        let apps = port_apps(text);
        let v: Vec<(&str, &str)> = apps
            .iter()
            .map(|a| (a.native_id.as_str(), a.version.as_deref().unwrap()))
            .collect();
        assert_eq!(v, [("git", "2.45.2_0"), ("python312", "3.12.5_0")]);
    }
}
