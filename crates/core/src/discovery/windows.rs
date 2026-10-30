//! Windows adapters: installed programs (the registry uninstall entries behind Settings > Apps),
//! Microsoft Store/MSIX packages, Scoop and Chocolatey. Parsers are pure and fixture-tested on
//! any OS; reading is read-only (registry, package folders, `Get-AppxPackage`).
//!
//! winget is deliberately not run: it may refresh its catalog and send usage telemetry, and the
//! application registry already maps known apps to winget IDs.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{App, Confidence, Ctx, GitHub, Kind, LocKind, Origin, Source, git, output};

pub fn scan(cx: &mut Ctx) {
    match read_uninstall_entries() {
        Ok(entries) => {
            let (apps, hidden) = uninstall_apps(entries);
            cx.apps.extend(apps);
            cx.hidden += hidden;
        }
        Err(e) => cx
            .notes
            .push(format!("Could not read the installed-apps list: {e}")),
    }
    msix(cx);
    for root in scoop_roots(cx.home) {
        cx.apps.extend(scoop_apps(&root));
    }
    let choco = std::env::var_os("ChocolateyInstall")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData\chocolatey"));
    let (apps, hidden) = choco_apps(&choco.join("lib"));
    cx.apps.extend(apps);
    cx.hidden += hidden;
}

// ---------------------------------------------------------------- Installed programs

/// One registry uninstall entry, with only the values discovery uses.
#[derive(Debug, Default, Clone)]
pub struct UninstallEntry {
    /// Subkey name: a product code GUID or the installer's own key.
    pub key: String,
    pub per_user: bool,
    pub name: String,
    pub version: Option<String>,
    pub publisher: Option<String>,
    pub location: Option<String>,
    pub size_kb: Option<u32>,
    /// `SystemComponent` = 1: Windows itself hides the entry.
    pub system_component: bool,
    /// An update or part of another entry (`ParentKeyName`, `ReleaseType`).
    pub child: bool,
    /// `URLInfoAbout`, `HelpLink`, `URLUpdateInfo`.
    pub urls: Vec<String>,
}

/// Shared runtimes and SDKs other programs depend on; counted, not listed.
const RUNTIMES: &[&str] = &[
    "Microsoft Visual C++",
    "Microsoft .NET",
    "Microsoft ASP.NET",
    "Microsoft Windows Desktop Runtime",
    "Microsoft Edge WebView2",
    "Microsoft Update Health Tools",
    "Windows Software Development Kit",
    "Windows SDK",
    "Windows Driver Package",
];
const BUILT_IN: &[&str] = &["Microsoft Edge", "Microsoft OneDrive"];

pub fn uninstall_apps(entries: Vec<UninstallEntry>) -> (Vec<App>, usize) {
    let mut seen = HashSet::new();
    let mut apps = Vec::new();
    let mut hidden = 0;
    for e in entries {
        let name = e.name.trim();
        if name.is_empty() || e.system_component || e.child {
            continue;
        }
        if RUNTIMES.iter().any(|r| name.starts_with(r)) {
            hidden += 1;
            continue;
        }
        // 64-bit and 32-bit views often list the same program twice.
        if !seen.insert((name.to_lowercase(), e.version.clone())) {
            continue;
        }
        let mut app = App::new(Source::Arp, &e.key, name, Kind::Desktop);
        app.version = e.version;
        app.system = BUILT_IN.iter().any(|b| name.starts_with(b));
        app.install_size = e.size_kb.map(|k| u64::from(k) * 1024);
        app.path = e.location.map(PathBuf::from);
        app.evidence.push(format!(
            "Listed in Settings > Apps ({} entry {})",
            if e.per_user {
                "per-user"
            } else {
                "machine-wide"
            },
            e.key
        ));
        if let Some(p) = e.publisher {
            app.evidence.push(format!("Publisher: {p}"));
        }
        if let Some((url, repo)) = e.urls.iter().find_map(|u| Some((u, git::github_repo(u)?))) {
            app.evidence
                .push(format!("Support link {url} points to GitHub"));
            app.github = Some(GitHub {
                repo,
                reference: None,
                asset: None,
                confidence: Confidence::Probable,
                origin: Origin::Detected,
            });
        }
        apps.push(app);
    }
    (apps, hidden)
}

#[cfg(windows)]
pub(crate) fn read_uninstall_entries() -> std::io::Result<Vec<UninstallEntry>> {
    use winreg::{HKCU, HKLM};
    const KEY: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall";
    const KEY_32: &str = r"SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall";
    let mut out = Vec::new();
    let mut opened = 0;
    for (hive, path, per_user) in [(HKLM, KEY, false), (HKLM, KEY_32, false), (HKCU, KEY, true)] {
        let Ok(root) = hive.open_subkey(path) else {
            continue;
        };
        opened += 1;
        for sub in root.enum_keys().flatten() {
            let Ok(k) = root.open_subkey(&sub) else {
                continue;
            };
            let s = |n: &str| {
                k.get_value::<String, _>(n)
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty())
            };
            let release = s("ReleaseType").unwrap_or_default();
            out.push(UninstallEntry {
                per_user,
                name: s("DisplayName").unwrap_or_default(),
                version: s("DisplayVersion"),
                publisher: s("Publisher"),
                location: s("InstallLocation"),
                size_kb: k.get_value::<u32, _>("EstimatedSize").ok(),
                system_component: k
                    .get_value::<u32, _>("SystemComponent")
                    .is_ok_and(|v| v == 1),
                child: s("ParentKeyName").is_some()
                    || ["Update", "Hotfix", "ServicePack"]
                        .iter()
                        .any(|t| release.contains(t)),
                urls: ["URLInfoAbout", "HelpLink", "URLUpdateInfo"]
                    .iter()
                    .filter_map(|n| s(n))
                    .collect(),
                key: sub,
            });
        }
    }
    if opened == 0 {
        return Err(std::io::Error::other(
            "the uninstall registry keys are missing",
        ));
    }
    Ok(out)
}

#[cfg(not(windows))]
pub(crate) fn read_uninstall_entries() -> std::io::Result<Vec<UninstallEntry>> {
    Err(std::io::Error::other("the registry exists only on Windows"))
}

// ---------------------------------------------------------------- Microsoft Store / MSIX

/// Removable, non-framework packages the user can see. No double quotes, so the script survives
/// Windows command-line quoting unchanged.
const APPX_SCRIPT: &str = "$p = @(Get-AppxPackage | Where-Object { -not $_.IsFramework -and -not $_.IsResourcePackage -and -not $_.NonRemovable -and [string]$_.SignatureKind -ne 'System' } | Select-Object Name, PackageFamilyName, @{n='Version';e={[string]$_.Version}}, InstallLocation, @{n='SignatureKind';e={[string]$_.SignatureKind}}); ConvertTo-Json -Compress -InputObject $p";

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct AppxRow {
    pub name: String,
    pub package_family_name: String,
    pub version: Option<String>,
    pub install_location: Option<String>,
    pub signature_kind: Option<String>,
}

pub fn parse_appx(json: &str) -> Vec<AppxRow> {
    serde_json::from_str(json.trim_start_matches('\u{feff}').trim()).unwrap_or_default()
}

/// The manifest's `<DisplayName>`, unless it is a resource reference; else the package name
/// without its publisher prefix, split at capitals ("SpotifyAB.SpotifyMusic" → "Spotify Music").
pub fn appx_display_name(manifest: Option<&str>, name: &str) -> String {
    if let Some(d) = manifest
        .and_then(|m| xml_text(m, "DisplayName"))
        .filter(|d| !d.starts_with("ms-resource:"))
    {
        return d;
    }
    let short = name.split_once('.').map_or(name, |(_, s)| s);
    let mut out = String::new();
    let mut prev_lower = false;
    for c in short.chars() {
        if c.is_uppercase() && prev_lower {
            out.push(' ');
        }
        prev_lower = c.is_lowercase();
        out.push(c);
    }
    out
}

pub fn msix_app(r: &AppxRow, manifest: Option<&str>, home: &Path) -> App {
    let mut app = App::new(
        Source::Msix,
        &r.name,
        &appx_display_name(manifest, &r.name),
        Kind::Desktop,
    );
    app.version = r.version.clone();
    app.system = r.name.starts_with("Microsoft") || r.name.starts_with("Windows");
    let store = r.signature_kind.as_deref() == Some("Store");
    app.repository = store.then(|| "Microsoft Store".into());
    app.path = r.install_location.as_ref().map(PathBuf::from);
    app.evidence.push(format!(
        "{} package {}",
        if store { "Microsoft Store" } else { "MSIX" },
        r.package_family_name
    ));
    // Each package keeps its own state folder under Packages\<family name>.
    let pfn = &r.package_family_name;
    if !pfn.is_empty()
        && pfn
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        let base = home
            .join("AppData")
            .join("Local")
            .join("Packages")
            .join(pfn);
        app.add_location(LocKind::Config, base.join("Settings"), Origin::Detected);
        app.add_location(LocKind::Config, base.join("RoamingState"), Origin::Detected);
        app.add_location(LocKind::Data, base.join("LocalState"), Origin::Detected);
    }
    app
}

fn msix(cx: &mut Ctx) {
    let Some((true, json)) = output(Command::new("powershell.exe").args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        APPX_SCRIPT,
    ])) else {
        cx.notes.push(
            "Could not list Microsoft Store apps (PowerShell Get-AppxPackage failed).".into(),
        );
        return;
    };
    for r in parse_appx(&json) {
        let manifest = r
            .install_location
            .as_ref()
            .and_then(|l| fs::read_to_string(Path::new(l).join("AppxManifest.xml")).ok());
        cx.apps.push(msix_app(&r, manifest.as_deref(), cx.home));
    }
}

// ---------------------------------------------------------------- Scoop

fn scoop_roots(home: &Path) -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    vec![
        env("SCOOP").unwrap_or_else(|| home.join("scoop")),
        env("SCOOP_GLOBAL").unwrap_or_else(|| PathBuf::from(r"C:\ProgramData\scoop")),
    ]
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Option<T> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()
}

#[derive(Debug, Default, Deserialize)]
struct ScoopManifest {
    version: Option<String>,
    homepage: Option<String>,
    shortcuts: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
struct ScoopInstall {
    bucket: Option<String>,
}

/// Apps under `<root>/apps/<name>/current`, with Scoop's `persist/<name>` folder as their data.
pub fn scoop_apps(root: &Path) -> Vec<App> {
    let Ok(rd) = fs::read_dir(root.join("apps")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for d in rd.flatten() {
        let id = d.file_name().to_string_lossy().into_owned();
        let current = d.path().join("current");
        if id == "scoop" {
            continue;
        }
        let Some(m) = read_json::<ScoopManifest>(&current.join("manifest.json")) else {
            continue;
        };
        let install = read_json::<ScoopInstall>(&current.join("install.json")).unwrap_or_default();
        let kind = if m.shortcuts.is_some() {
            Kind::Desktop
        } else {
            Kind::CommandLine
        };
        let mut app = App::new(Source::Scoop, &id, &id, kind);
        app.version = m.version;
        app.repository = install.bucket;
        app.evidence
            .push(format!("Installed with Scoop in {}", current.display()));
        if let Some(repo) = m.homepage.as_deref().and_then(git::github_repo) {
            app.evidence.push(format!("Homepage is github.com/{repo}"));
            app.github = Some(GitHub {
                repo,
                reference: app.version.clone(),
                asset: None,
                confidence: Confidence::Probable,
                origin: Origin::Detected,
            });
        }
        app.add_location(
            LocKind::Data,
            root.join("persist").join(&id),
            Origin::Detected,
        );
        app.path = Some(current);
        out.push(app);
    }
    out
}

// ---------------------------------------------------------------- Chocolatey

/// Text of the first `<tag>…</tag>` element, entity-decoded. Enough for nuspec and AppxManifest
/// metadata; not a general XML parser.
pub fn xml_text(doc: &str, tag: &str) -> Option<String> {
    let start = doc.find(&format!("<{tag}>"))? + tag.len() + 2;
    let len = doc[start..].find(&format!("</{tag}>"))?;
    let t = doc[start..start + len]
        .trim()
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    (!t.is_empty()).then_some(t)
}

/// Packages under Chocolatey's `lib` folder. Chocolatey itself, its extensions, Windows updates
/// and runtimes are counted as hidden, as are `.install`/`.portable` halves of a listed package.
pub fn choco_apps(lib: &Path) -> (Vec<App>, usize) {
    let mut ids: Vec<String> = fs::read_dir(lib)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    let all: HashSet<String> = ids.iter().map(|i| i.to_lowercase()).collect();
    let (mut apps, mut hidden) = (Vec::new(), 0);
    for id in ids {
        let l = id.to_lowercase();
        let base = l
            .strip_suffix(".install")
            .or_else(|| l.strip_suffix(".portable"));
        let helper = l == "chocolatey"
            || l.starts_with("chocolatey-")
            || l.ends_with(".extension")
            || ["vcredist", "dotnet", "netfx"]
                .iter()
                .any(|p| l.starts_with(p))
            || l.strip_prefix("kb")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        if helper || base.is_some_and(|b| all.contains(b)) {
            hidden += 1;
            continue;
        }
        let nuspec =
            fs::read_to_string(lib.join(&id).join(format!("{id}.nuspec"))).unwrap_or_default();
        let name = xml_text(&nuspec, "title")
            .unwrap_or_else(|| id[..base.map_or(id.len(), str::len)].to_string());
        let mut app = App::new(Source::Choco, &id, &name, Kind::Desktop);
        app.version = xml_text(&nuspec, "version");
        app.evidence
            .push(format!("Installed with Chocolatey (package {id})"));
        if let Some((url, repo)) = ["projectSourceUrl", "projectUrl"]
            .iter()
            .filter_map(|t| xml_text(&nuspec, t))
            .find_map(|u| Some((u.clone(), git::github_repo(&u)?)))
        {
            app.evidence
                .push(format!("Package metadata links to {url}"));
            app.github = Some(GitHub {
                repo,
                reference: None,
                asset: None,
                confidence: Confidence::Probable,
                origin: Origin::Detected,
            });
        }
        apps.push(app);
    }
    (apps, hidden)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(key: &str, name: &str) -> UninstallEntry {
        UninstallEntry {
            key: key.into(),
            name: name.into(),
            ..Default::default()
        }
    }

    #[test]
    fn installed_programs() {
        let entries = vec![
            UninstallEntry {
                version: Some("131.0".into()),
                publisher: Some("Mozilla".into()),
                location: Some(r"C:\Program Files\Mozilla Firefox".into()),
                size_kb: Some(250_000),
                ..entry(
                    "Mozilla Firefox 131.0 (x64 en-US)",
                    "Mozilla Firefox (x64 en-US)",
                )
            },
            // Same program seen through the 32-bit registry view.
            UninstallEntry {
                version: Some("131.0".into()),
                ..entry(
                    "Mozilla Firefox 131.0 (x64 en-US)",
                    "Mozilla Firefox (x64 en-US)",
                )
            },
            entry(
                "{A1}",
                "Microsoft Visual C++ 2022 X64 Minimum Runtime - 14.40",
            ),
            UninstallEntry {
                system_component: true,
                ..entry("{A2}", "Some Driver Component")
            },
            UninstallEntry {
                child: true,
                ..entry("KB5000001", "Update for Something")
            },
            entry("{A3}", ""),
            UninstallEntry {
                per_user: true,
                urls: vec!["https://github.com/rustdesk/rustdesk/issues".into()],
                ..entry("RustDesk", "RustDesk")
            },
            entry("Microsoft Edge", "Microsoft Edge"),
        ];
        let (apps, hidden) = uninstall_apps(entries);
        assert_eq!(hidden, 1);
        let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(
            names,
            ["Mozilla Firefox (x64 en-US)", "RustDesk", "Microsoft Edge"]
        );
        assert_eq!(apps[0].install_size, Some(256_000_000));
        assert_eq!(apps[0].id, "arp:Mozilla Firefox 131.0 (x64 en-US)");
        assert_eq!(apps[1].github.as_ref().unwrap().repo, "rustdesk/rustdesk");
        assert!(apps[1].evidence[0].contains("per-user"));
        assert!(apps[2].system && !apps[0].system);
    }

    #[test]
    fn store_packages() {
        let json = "\u{feff}[{\"Name\":\"SpotifyAB.SpotifyMusic\",\"PackageFamilyName\":\"SpotifyAB.SpotifyMusic_zpdnekdrzrea0\",\"Version\":\"1.2.3.0\",\"InstallLocation\":\"C:\\\\Program Files\\\\WindowsApps\\\\SpotifyAB\",\"SignatureKind\":\"Store\"},{\"Name\":\"Microsoft.WindowsTerminal\",\"PackageFamilyName\":\"../evil\",\"Version\":\"1.0\",\"InstallLocation\":null,\"SignatureKind\":\"Developer\"}]";
        let rows = parse_appx(json);
        assert_eq!(rows.len(), 2);
        assert!(parse_appx("not json").is_empty());

        let home = tempfile::tempdir().unwrap();
        let state = home
            .path()
            .join("AppData/Local/Packages/SpotifyAB.SpotifyMusic_zpdnekdrzrea0/LocalState");
        fs::create_dir_all(&state).unwrap();
        let a = msix_app(
            &rows[0],
            Some("<Properties><DisplayName>Spotify Music</DisplayName></Properties>"),
            home.path(),
        );
        assert_eq!(a.name, "Spotify Music");
        assert_eq!(a.repository.as_deref(), Some("Microsoft Store"));
        assert_eq!(a.locations.len(), 1);
        assert_eq!(a.locations[0].path, state);
        assert!(!a.system);

        fs::create_dir_all(home.path().join("AppData/Local/evil/Settings")).unwrap();
        let b = msix_app(
            &rows[1],
            Some("<DisplayName>ms-resource:AppName</DisplayName>"),
            home.path(),
        );
        assert_eq!(b.name, "Windows Terminal");
        assert!(
            b.locations.is_empty(),
            "unsafe family name must not become a path"
        );
        assert!(b.system && b.repository.is_none());
    }

    #[test]
    fn scoop_and_chocolatey() {
        let t = tempfile::tempdir().unwrap();
        let scoop = t.path().join("scoop");
        let cur = scoop.join("apps/neovim/current");
        fs::create_dir_all(&cur).unwrap();
        fs::write(cur.join("manifest.json"), r#"{"version":"0.10.2","homepage":"https://github.com/neovim/neovim","bin":"bin\\nvim.exe","notes":"comment"}"#).unwrap();
        fs::write(
            cur.join("install.json"),
            r#"{"bucket":"main","architecture":"64bit"}"#,
        )
        .unwrap();
        fs::create_dir_all(scoop.join("persist/neovim")).unwrap();
        fs::create_dir_all(scoop.join("apps/scoop/current")).unwrap();
        fs::create_dir_all(scoop.join("apps/broken/current")).unwrap();
        let apps = scoop_apps(&scoop);
        assert_eq!(apps.len(), 1);
        let nv = &apps[0];
        assert_eq!(
            (nv.native_id.as_str(), nv.kind),
            ("neovim", Kind::CommandLine)
        );
        assert_eq!(nv.repository.as_deref(), Some("main"));
        assert_eq!(nv.github.as_ref().unwrap().repo, "neovim/neovim");
        assert_eq!(nv.locations[0].path, scoop.join("persist/neovim"));

        let lib = t.path().join("lib");
        for id in [
            "chocolatey",
            "chocolatey-core.extension",
            "KB2919355",
            "vcredist140",
            "git",
            "git.install",
            "7zip.install",
        ] {
            fs::create_dir_all(lib.join(id)).unwrap();
        }
        fs::write(lib.join("git/git.nuspec"), "<package><metadata><id>git</id><version>2.47.0</version><title>Git</title><projectSourceUrl>https://github.com/git-for-windows/git</projectSourceUrl></metadata></package>").unwrap();
        let (apps, hidden) = choco_apps(&lib);
        assert_eq!(hidden, 5);
        let ids: Vec<(&str, &str)> = apps
            .iter()
            .map(|a| (a.native_id.as_str(), a.name.as_str()))
            .collect();
        assert_eq!(ids, [("7zip.install", "7zip"), ("git", "Git")]);
        assert_eq!(apps[1].version.as_deref(), Some("2.47.0"));
        assert_eq!(apps[1].github.as_ref().unwrap().repo, "git-for-windows/git");
        assert_eq!(
            xml_text("<t> a &amp; b </t>", "t").as_deref(),
            Some("a & b")
        );
    }
}
