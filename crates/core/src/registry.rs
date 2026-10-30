//! Versioned application identity registry: one canonical app (e.g. `firefox`) mapped to the
//! package IDs that install it on each OS, plus where it keeps settings and data.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::LazyLock;

use serde::Deserialize;

use crate::discovery::{App, Confidence, LocKind, Origin, Source, source_key};
use crate::paths::{join_rel, untilde};
use crate::platform::OsFamily;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema_version: u32,
    pub apps: Vec<Entry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub id: String,
    pub name: String,
    /// Install method (`flatpak`, `rpm`, `winget`, `brew_cask`, …) → package IDs. `arp` holds
    /// Windows installed-apps display names, `app_bundle` macOS bundle identifiers.
    pub packages: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub linux: Paths,
    #[serde(default)]
    pub windows: Paths,
    #[serde(default)]
    pub macos: Paths,
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

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    #[serde(default)]
    pub config: Vec<String>,
    #[serde(default)]
    pub data: Vec<String>,
}

pub static REGISTRY: LazyLock<Registry> = LazyLock::new(|| {
    // Embedded at build time and covered by tests.
    #[allow(clippy::unwrap_used)]
    serde_json::from_str(include_str!("../registry/apps.json")).unwrap()
});

/// The OS family an install method belongs to.
pub fn method_os(method: &str) -> Option<OsFamily> {
    match method {
        "flatpak" | "rpm" | "dpkg" | "pacman" | "snap" | "appimage" => Some(OsFamily::Linux),
        "winget" | "scoop" | "choco" | "msi" | "msix" | "exe" | "arp" => Some(OsFamily::Windows),
        "brew" | "brew_cask" | "macports" | "pkg" | "app_bundle" | "mas" => Some(OsFamily::Macos),
        _ => None,
    }
}

pub fn get(id: &str) -> Option<&'static Entry> {
    REGISTRY.apps.iter().find(|e| e.id == id)
}

pub fn lookup(source: Source, native_id: &str) -> Option<&'static Entry> {
    let key = source_key(source);
    REGISTRY.apps.iter().find(|e| {
        e.packages
            .get(key)
            .is_some_and(|ids| ids.iter().any(|i| i == native_id))
    })
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
pub fn platforms(entry: &Entry) -> Vec<OsFamily> {
    let mut v: Vec<OsFamily> = entry.packages.keys().filter_map(|m| method_os(m)).collect();
    v.sort();
    v.dedup();
    v
}

/// Attaches the canonical identity and settings/data locations for `os`. Registry paths apply to
/// native installs; Flatpak, Snap and MSIX keep their own sandboxed locations.
pub fn annotate(app: &mut App, home: &Path, os: OsFamily) {
    let native = !matches!(
        app.source,
        Source::Flatpak | Source::Snap | Source::GitClone | Source::Msix
    );
    let exact = lookup(app.source, &app.native_id);
    let entry = exact.or_else(|| {
        let n = app.name.to_lowercase();
        match app.source {
            Source::Arp => REGISTRY.apps.iter().find(|e| {
                e.packages
                    .get("arp")
                    .is_some_and(|ns| ns.iter().any(|x| arp_match(&app.name, x)))
            }),
            // Portable installs and bundles without a known ID: match on the displayed name.
            Source::AppImage | Source::Custom | Source::AppBundle => REGISTRY
                .apps
                .iter()
                .find(|e| e.name.to_lowercase() == n || e.id == n),
            _ => None,
        }
    });
    app.available_on = match entry {
        Some(e) => platforms(e),
        None => method_os(source_key(app.source)).into_iter().collect(),
    };
    if let Some(e) = entry {
        app.canonical = Some(e.id.clone());
        app.evidence.push(match exact {
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
        annotate(&mut code, home.path(), OsFamily::Linux);
        assert_eq!(code.canonical.as_deref(), Some("vscode"));
        assert_eq!(code.locations.len(), 1);
        assert_eq!(code.locations[0].origin, Origin::Detected);

        let mut fp = App::new(
            Source::Flatpak,
            "com.visualstudio.code",
            "Code",
            Kind::Desktop,
        );
        annotate(&mut fp, home.path(), OsFamily::Linux);
        assert_eq!(fp.canonical.as_deref(), Some("vscode"));
        assert!(
            fp.locations.is_empty(),
            "flatpak must not pick up native paths"
        );

        let mut other = App::new(Source::Rpm, "mytool", "My Tool", Kind::Desktop);
        annotate(&mut other, home.path(), OsFamily::Linux);
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
        annotate(&mut code, h, OsFamily::Windows);
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
        annotate(&mut ff, h, OsFamily::Macos);
        assert_eq!(ff.canonical.as_deref(), Some("firefox"));
        assert_eq!(ff.locations[0].kind, LocKind::Data);

        let mut tiny = App::new(Source::Arp, "TinyTool", "Tiny Tool", Kind::Desktop);
        annotate(&mut tiny, h, OsFamily::Windows);
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
}
