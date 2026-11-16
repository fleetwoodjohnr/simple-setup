//! Versioned application identity registry: one canonical app (e.g. `firefox`) mapped to the
//! package IDs that install it on each OS, plus where it keeps settings and data.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::discovery::{App, Confidence, LocKind, Origin, Source, source_key};
use crate::paths::{join_rel, untilde};
use crate::platform::OsFamily;

#[derive(Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema_version: u32,
    pub apps: Vec<Entry>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub name: String,
    /// Install method (`flatpak`, `rpm`, `winget`, `brew_cask`, …) → package IDs. `arp` holds
    /// Windows installed-apps display names, `app_bundle` macOS bundle identifiers.
    pub packages: BTreeMap<String, Vec<String>>,
    /// `owner/name` of the app's GitHub repository, for its release downloads on any system.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<String>,
    #[serde(default)]
    pub linux: Paths,
    #[serde(default)]
    pub windows: Paths,
    #[serde(default)]
    pub macos: Paths,
    /// Its settings spell out the home folder (project and file paths): they are rewritten to
    /// the new home folder when restored.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub home_paths: bool,
}

impl Entry {
    pub fn paths(&self, os: OsFamily) -> &Paths {
        match os {
            OsFamily::Linux => &self.linux,
            OsFamily::Windows => &self.windows,
            OsFamily::Macos => &self.macos,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    #[serde(default)]
    pub config: Vec<String>,
    #[serde(default)]
    pub data: Vec<String>,
}

/// The built-in registry, with the user's own entries (`apps.json` in the configuration
/// folder) added or replacing built-in ones by ID. A user file that does not validate is not
/// used at all.
pub static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    // Embedded at build time and covered by tests.
    #[allow(clippy::unwrap_used)]
    let mut r: Registry = serde_json::from_str(include_str!("../registry/apps.json")).unwrap();
    if !cfg!(test)
        && let Some(path) = user_file()
        && path.is_file()
    {
        match read_file(&path) {
            Ok(user) => merge(&mut r, user),
            Err(e) => crate::diag::log(&format!("{} is not used: {e}", path.display())),
        }
    }
    r
});

/// Where the user's own registry entries live.
pub fn user_file() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("simple-setup").join("apps.json"))
}

/// Adds `user`'s entries to `base`, replacing built-in ones with the same ID.
pub fn merge(base: &mut Registry, user: Registry) {
    for e in user.apps {
        match base.apps.iter_mut().find(|b| b.id == e.id) {
            Some(b) => *b = e,
            None => base.apps.push(e),
        }
    }
}

/// Reads and checks a registry file (the user's own entries, or one to import).
pub fn read_file(path: &Path) -> crate::Result<Registry> {
    let meta =
        std::fs::metadata(path).map_err(|e| crate::invalid(format!("{}: {e}", path.display())))?;
    if meta.len() > 1 << 20 {
        return Err(crate::invalid("The app registry file is larger than 1 MB."));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| crate::invalid(format!("{}: {e}", path.display())))?;
    let r: Registry = serde_json::from_str(&text)
        .map_err(|e| crate::invalid(format!("Not an app registry file: {e}")))?;
    let problems = validate(&r);
    if problems.is_empty() {
        Ok(r)
    } else {
        Err(crate::invalid(problems.join(" ")))
    }
}

/// Everything wrong with a registry, in plain sentences (empty: it can be used).
pub fn validate(r: &Registry) -> Vec<String> {
    let mut out = Vec::new();
    if r.schema_version != 1 {
        out.push(format!("Format version {} is not 1.", r.schema_version));
    }
    let mut ids = std::collections::HashSet::new();
    for e in &r.apps {
        let at = |m: String| format!("{}: {m}", e.id);
        if !crate::manifest::plain_name(&e.id) || !ids.insert(e.id.clone()) {
            out.push(format!("{:?} is not a usable or unique app ID.", e.id));
        }
        if e.name.trim().is_empty() || e.name.len() > 100 || e.name.chars().any(char::is_control) {
            out.push(at("the name is empty or not plain text.".into()));
        }
        for (method, names) in &e.packages {
            if method_os(method).is_none() {
                out.push(at(format!(
                    "{method:?} is not an install method Simple Setup knows."
                )));
            }
            for n in names {
                let ok = if method == "arp" {
                    !n.trim().is_empty() && n.len() <= 200 && !n.chars().any(char::is_control)
                } else {
                    crate::manifest::identifier_ok(n)
                };
                if !ok {
                    out.push(at(format!("{n:?} is not a usable {method} name.")));
                }
            }
        }
        if let Some(g) = &e.github
            && crate::store::parse_github(g).ok().as_deref() != Some(g.as_str())
        {
            out.push(at(format!(
                "{g:?} is not a GitHub repository (owner/name)."
            )));
        }
        for os in [OsFamily::Linux, OsFamily::Windows, OsFamily::Macos] {
            let p = e.paths(os);
            for p in p.config.iter().chain(&p.data) {
                if !p.starts_with("~/") || untilde(p, Path::new("/h")).is_err() {
                    out.push(at(format!("{p:?} must be a folder under ~/.")));
                }
            }
        }
    }
    out
}

/// The OS family an install method belongs to.
pub fn method_os(method: &str) -> Option<OsFamily> {
    match method {
        "flatpak" | "rpm" | "dpkg" | "pacman" | "suse" | "apk" | "snap" | "appimage" => {
            Some(OsFamily::Linux)
        }
        "winget" | "scoop" | "choco" | "msstore" | "msi" | "msix" | "exe" | "arp" => {
            Some(OsFamily::Windows)
        }
        "brew" | "brew_cask" | "macports" | "pkg" | "app_bundle" | "mas" => Some(OsFamily::Macos),
        _ => None,
    }
}

pub fn get(id: &str) -> Option<&'static Entry> {
    REGISTRY.apps.iter().find(|e| e.id == id)
}

pub fn lookup(source: Source, native_id: &str) -> Option<&'static Entry> {
    lookup_in(&[source_key(source).to_string()], native_id)
}

/// The entry listing `native_id` under one of `keys` (a distribution profile's package-name
/// keys, best first: a package name means something only within its distribution family).
pub fn lookup_in(keys: &[String], native_id: &str) -> Option<&'static Entry> {
    keys.iter().find_map(|key| {
        REGISTRY.apps.iter().find(|e| {
            e.packages
                .get(key)
                .is_some_and(|ids| ids.iter().any(|i| i == native_id))
        })
    })
}

/// Registry keys for an app's own ID: the host distribution's package-name keys for native
/// packages (`linux_names`), the source's key otherwise.
pub fn keys_for(source: Source, linux_names: Option<&[String]>) -> Vec<String> {
    match (source, linux_names) {
        (Source::Rpm | Source::Dpkg | Source::Pacman | Source::Apk, Some(n)) if !n.is_empty() => {
            n.to_vec()
        }
        _ => vec![source_key(source).to_string()],
    }
}

/// Windows lists "Mozilla Firefox (x64 en-US)" or "GIMP 2.10.38": the registry name must be
/// followed by nothing, a version or a parenthesis, so "Git" does not match "Git LFS".
pub fn arp_match(display: &str, name: &str) -> bool {
    let (d, n) = (display.to_lowercase(), name.to_lowercase());
    d.strip_prefix(&n).is_some_and(|rest| {
        rest.is_empty()
            || rest.starts_with(" (")
            || rest.starts_with(" version")
            || rest
                .strip_prefix(' ')
                .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
    })
}

/// OS families where `entry` has a known install method.
/// An entry's package and catalog identifiers, for a snapshot to carry (installed-apps display
/// names are not identifiers and stay out).
pub fn identifiers(entry: &Entry) -> BTreeMap<String, Vec<String>> {
    entry
        .packages
        .iter()
        .filter(|(k, _)| k.as_str() != "arp")
        .map(|(k, v)| {
            let ids: Vec<String> = v
                .iter()
                .filter(|i| crate::manifest::identifier_ok(i))
                .cloned()
                .collect();
            (k.clone(), ids)
        })
        .filter(|(_, v)| !v.is_empty())
        .collect()
}

pub fn platforms(entry: &Entry) -> Vec<OsFamily> {
    let mut v: Vec<OsFamily> = entry.packages.keys().filter_map(|m| method_os(m)).collect();
    v.sort();
    v.dedup();
    v
}

/// Attaches the canonical identity and settings/data locations for `os`. Registry paths apply to
/// native installs; Flatpak, Snap and MSIX keep their own sandboxed locations.
pub fn annotate(app: &mut App, home: &Path, os: OsFamily, linux_names: Option<&[String]>) {
    let native = !matches!(
        app.source,
        Source::Flatpak | Source::Snap | Source::GitClone | Source::Msix
    );
    // An identity the user chose wins over detection.
    let chosen = app
        .user_fields
        .iter()
        .any(|f| f == "canonical" || f == "all")
        .then(|| app.canonical.as_deref().and_then(get))
        .flatten();
    let exact = chosen.or_else(|| lookup_in(&keys_for(app.source, linux_names), &app.native_id));
    let entry = exact.or_else(|| {
        let n = app.name.to_lowercase();
        match app.source {
            Source::Arp => REGISTRY.apps.iter().find(|e| {
                e.packages
                    .get("arp")
                    .is_some_and(|ns| ns.iter().any(|x| arp_match(&app.name, x)))
            }),
            // Portable installs and bundles without a known ID: match on the displayed name.
            Source::AppImage | Source::Custom | Source::AppBundle | Source::Manual => REGISTRY
                .apps
                .iter()
                .find(|e| e.name.to_lowercase() == n || e.id == n),
            _ => None,
        }
    });
    app.available_on = match entry {
        Some(e) => platforms(e),
        // Developer tools install the same way on every system.
        None if crate::resolve::dev_tool(app.source).is_some() => {
            vec![OsFamily::Linux, OsFamily::Windows, OsFamily::Macos]
        }
        None => method_os(source_key(app.source)).into_iter().collect(),
    };
    if let Some(e) = entry {
        app.canonical = Some(e.id.clone());
        app.evidence.push(match exact {
            Some(_) if chosen.is_some() => format!("You chose registry entry \"{}\"", e.id),
            Some(_) => format!("Package ID matches registry entry \"{}\"", e.id),
            None => format!("Name matches registry entry \"{}\" (unconfirmed)", e.id),
        });
        if native {
            let paths = e.paths(os);
            for (kind, list) in [
                (LocKind::Config, &paths.config),
                (LocKind::Data, &paths.data),
            ] {
                for p in list.iter().filter_map(|p| untilde(p, home).ok()) {
                    app.add_location(kind, p, Origin::Detected);
                }
            }
        }
    }
    if native && app.locations.is_empty() {
        let lower = app.name.to_lowercase();
        let mut names = vec![
            app.native_id.clone(),
            lower.clone(),
            lower.replace(' ', "-"),
            lower.replace(' ', ""),
            app.name.clone(),
        ];
        names.retain(|n| !n.is_empty() && !n.contains(['/', '\\']) && !n.starts_with('.'));
        names.dedup();
        let guesses: &[(LocKind, &str)] = match os {
            OsFamily::Linux => &[
                (LocKind::Config, ".config"),
                (LocKind::Data, ".local/share"),
            ],
            OsFamily::Windows => &[
                (LocKind::Config, "AppData/Roaming"),
                (LocKind::Data, "AppData/Local"),
            ],
            OsFamily::Macos => &[
                (LocKind::Data, "Library/Application Support"),
                (LocKind::Config, ".config"),
            ],
        };
        for (kind, base) in guesses {
            let Ok(dir) = join_rel(home, base, false) else {
                continue;
            };
            let entries: Vec<String> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            for n in &names {
                // Windows and macOS folders ignore case: use the folder's real name, once.
                let hit = entries.iter().find(|e| *e == n).or_else(|| {
                    let n = n.to_lowercase();
                    entries
                        .iter()
                        .find(|e| os != OsFamily::Linux && e.to_lowercase() == n)
                });
                if let Some(e) = hit {
                    app.add_location(*kind, dir.join(e), Origin::Inferred);
                }
            }
        }
        if app.locations.iter().any(|l| l.origin == Origin::Inferred) {
            app.evidence
                .push("Settings folders guessed from the app name; please check them".into());
            app.confidence = app.confidence.min(Confidence::Probable);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Kind;

    #[test]
    fn registry_is_consistent() {
        assert!(validate(&REGISTRY).is_empty(), "{:?}", validate(&REGISTRY));
        assert_eq!(REGISTRY.schema_version, 1);
        let mut ids = std::collections::HashSet::new();
        for e in &REGISTRY.apps {
            assert!(ids.insert(&e.id), "duplicate {}", e.id);
            for m in e.packages.keys() {
                assert!(method_os(m).is_some(), "{}: unknown method {m}", e.id);
            }
            for os in [OsFamily::Linux, OsFamily::Windows, OsFamily::Macos] {
                let p = e.paths(os);
                for p in p.config.iter().chain(&p.data) {
                    assert!(
                        p.starts_with("~/") && untilde(p, Path::new("/h")).is_ok(),
                        "{}: {p}",
                        e.id
                    );
                }
            }
        }
        assert_eq!(
            platforms(get("firefox").unwrap()),
            vec![OsFamily::Linux, OsFamily::Windows, OsFamily::Macos]
        );
    }

    #[test]
    fn annotates_native_and_sandboxed_installs() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".config/Code/User")).unwrap();
        std::fs::create_dir_all(home.path().join(".config/mytool")).unwrap();

        let mut code = App::new(Source::Rpm, "code", "Visual Studio Code", Kind::Desktop);
        annotate(&mut code, home.path(), OsFamily::Linux, None);
        assert_eq!(code.canonical.as_deref(), Some("vscode"));
        assert_eq!(code.locations.len(), 1);
        assert_eq!(code.locations[0].origin, Origin::Detected);

        let mut fp = App::new(
            Source::Flatpak,
            "com.visualstudio.code",
            "Code",
            Kind::Desktop,
        );
        annotate(&mut fp, home.path(), OsFamily::Linux, None);
        assert_eq!(fp.canonical.as_deref(), Some("vscode"));
        assert!(
            fp.locations.is_empty(),
            "flatpak must not pick up native paths"
        );

        let mut other = App::new(Source::Rpm, "mytool", "My Tool", Kind::Desktop);
        annotate(&mut other, home.path(), OsFamily::Linux, None);
        assert_eq!(other.locations[0].origin, Origin::Inferred);
        assert_eq!(other.confidence, Confidence::Probable);
    }

    #[test]
    fn windows_and_macos_identities() {
        assert!(arp_match("Mozilla Firefox (x64 en-US)", "Mozilla Firefox"));
        assert!(arp_match("GIMP 2.10.38", "GIMP"));
        assert!(arp_match("Git", "Git"));
        assert!(arp_match(
            "Telegram Desktop version 5.2",
            "Telegram Desktop"
        ));
        assert!(!arp_match("Git LFS version 3.4.0", "Git"));
        assert!(!arp_match("GitHub CLI", "Git"));
        assert!(!arp_match("Signalling Tool", "Signal"));

        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        std::fs::create_dir_all(h.join("AppData/Roaming/Code/User")).unwrap();
        std::fs::create_dir_all(h.join("Library/Application Support/Firefox")).unwrap();
        std::fs::create_dir_all(h.join("AppData/Roaming/Tiny Tool")).unwrap();

        let mut code = App::new(
            Source::Arp,
            "{771FD6B0}_is1",
            "Microsoft Visual Studio Code (User)",
            Kind::Desktop,
        );
        annotate(&mut code, h, OsFamily::Windows, None);
        assert_eq!(code.canonical.as_deref(), Some("vscode"));
        assert_eq!(code.locations[0].path, h.join("AppData/Roaming/Code/User"));
        assert_eq!(
            code.available_on,
            vec![OsFamily::Linux, OsFamily::Windows, OsFamily::Macos]
        );

        let mut ff = App::new(
            Source::AppBundle,
            "org.mozilla.firefox",
            "Firefox",
            Kind::Desktop,
        );
        annotate(&mut ff, h, OsFamily::Macos, None);
        assert_eq!(ff.canonical.as_deref(), Some("firefox"));
        assert_eq!(ff.locations[0].kind, LocKind::Data);

        let mut tiny = App::new(Source::Arp, "TinyTool", "Tiny Tool", Kind::Desktop);
        annotate(&mut tiny, h, OsFamily::Windows, None);
        assert_eq!(tiny.canonical, None);
        assert_eq!(tiny.available_on, vec![OsFamily::Windows]);
        assert_eq!(tiny.locations[0].origin, Origin::Inferred);
        assert_eq!(
            tiny.locations.len(),
            1,
            "one folder, found under several spellings"
        );
        assert!(tiny.locations[0].path.ends_with("Tiny Tool"));
    }

    /// The user's own entries add apps and replace built-in ones; broken files are refused.
    #[test]
    fn user_entries() {
        let t = tempfile::tempdir().unwrap();
        let f = t.path().join("apps.json");
        std::fs::write(
            &f,
            r#"{"schema_version": 1, "apps": [
                {"id": "my-tool", "name": "My Tool", "github": "me/my-tool",
                 "packages": {"rpm": ["my-tool"], "winget": ["Me.MyTool"]},
                 "linux": {"config": ["~/.config/my-tool"]}},
                {"id": "jq", "name": "jq (mine)", "packages": {"dpkg": ["jq"]}}]}"#,
        )
        .unwrap();
        let user = read_file(&f).unwrap();
        let mut base: Registry =
            serde_json::from_str(include_str!("../registry/apps.json")).unwrap();
        let before = base.apps.len();
        merge(&mut base, user);
        assert_eq!(base.apps.len(), before + 1);
        assert_eq!(
            base.apps.iter().find(|e| e.id == "jq").unwrap().name,
            "jq (mine)"
        );
        for bad in [
            r#"{"schema_version": 1, "apps": [{"id": "x", "name": "X", "packages": {"rpm": ["../x"]}}]}"#,
            r#"{"schema_version": 1, "apps": [{"id": "x", "name": "X", "packages": {"nope": ["x"]}}]}"#,
            r#"{"schema_version": 1, "apps": [{"id": "x", "name": "X", "packages": {}, "linux": {"data": ["/etc"]}}]}"#,
            r#"{"schema_version": 1, "apps": [{"id": "x", "name": "X", "packages": {}, "github": "not a repo"}]}"#,
            r#"{"schema_version": 2, "apps": []}"#,
            r#"{"schema_version": 1, "apps": [], "extra": 1}"#,
        ] {
            std::fs::write(&f, bad).unwrap();
            assert!(read_file(&f).is_err(), "{bad}");
        }
    }
}
