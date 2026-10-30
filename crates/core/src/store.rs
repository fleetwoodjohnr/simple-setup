//! Local app state: saved profiles, settings, user-entered apps, and the list of snapshots
//! this computer created or opened. Plain JSON files, written atomically.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::discovery::{App, Confidence, Download, GitHub, Location, Origin, Source, git};
use crate::selection::Selection;
use crate::{IoContext, Result, invalid};

pub struct Store {
    root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub saved_at: String,
    pub selection: Selection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Folders approved for finding source checkouts and portable apps.
    #[serde(default)]
    pub source_dirs: Vec<PathBuf>,
}

/// User corrections to a discovered app. `None` keeps the detected value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppEdit {
    pub name: Option<String>,
    pub version: Option<String>,
    pub canonical: Option<String>,
    /// GitHub URL or `owner/name`.
    pub github: Option<String>,
    pub github_ref: Option<String>,
    #[serde(default)]
    pub add_locations: Vec<Location>,
    #[serde(default)]
    pub remove_locations: Vec<PathBuf>,
    /// Replaces the app's installer downloads (`None` keeps them).
    #[serde(default)]
    pub downloads: Option<Vec<Download>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UserApps {
    #[serde(default)]
    pub custom: Vec<App>,
    #[serde(default)]
    pub edits: BTreeMap<String, AppEdit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRef {
    pub path: PathBuf,
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub last_verified: Option<String>,
    pub last_verify_ok: Option<bool>,
}

fn check_name(name: &str) -> Result<()> {
    if name.trim().is_empty() || name.len() > 80 || !crate::paths::portable_component(name) {
        return Err(invalid(
            "Use a short name without / \\ : * ? \" < > | characters.",
        ));
    }
    Ok(())
}

pub fn parse_github(input: &str) -> Result<String> {
    let s = input.trim();
    let url = if s.contains("://") || s.starts_with("git@") {
        s.to_string()
    } else {
        format!("https://github.com/{s}")
    };
    git::github_repo(&url).ok_or_else(|| invalid(format!("Not a GitHub repository: {s}")))
}

impl UserApps {
    pub fn apply(&self, apps: &mut Vec<App>) {
        apps.extend(self.custom.iter().cloned());
        for app in apps.iter_mut() {
            let Some(e) = self.edits.get(&app.id) else {
                continue;
            };
            let mut mark = |f: &str| app.user_fields.push(f.into());
            if let Some(v) = &e.name {
                app.name = v.clone();
                mark("name");
            }
            if let Some(v) = &e.version {
                app.version = Some(v.clone());
                mark("version");
            }
            if let Some(v) = &e.canonical {
                app.canonical = Some(v.clone());
                mark("canonical");
            }
            if let Some(repo) = e.github.as_deref().and_then(|g| parse_github(g).ok()) {
                app.github = Some(GitHub {
                    repo,
                    reference: e.github_ref.clone(),
                    asset: None,
                    confidence: Confidence::Confirmed,
                    origin: Origin::User,
                });
                app.user_fields.push("github".into());
            }
            app.locations
                .retain(|l| !e.remove_locations.contains(&l.path));
            for l in &e.add_locations {
                if !app.locations.iter().any(|x| x.path == l.path) {
                    app.locations.push(Location {
                        origin: Origin::User,
                        ..l.clone()
                    });
                }
            }
            if !e.add_locations.is_empty() || !e.remove_locations.is_empty() {
                app.user_fields.push("locations".into());
            }
            if let Some(d) = &e.downloads {
                app.downloads = d.clone();
                app.user_fields.push("downloads".into());
            }
        }
    }
}

impl Store {
    pub fn open_default() -> Result<Self> {
        let base =
            dirs::config_dir().ok_or_else(|| invalid("No configuration folder for this user."))?;
        Ok(Self::at(base.join("simple-setup")))
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn read<T: DeserializeOwned + Default>(&self, name: &str) -> Result<T> {
        let p = self.root.join(name);
        match std::fs::read(&p) {
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| invalid(format!("{} is damaged: {e}", p.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(e) => Err(e).ctx(|| format!("Reading {}", p.display())),
        }
    }

    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let p = self.root.join(name);
        let dir = p.parent().unwrap_or(&self.root);
        std::fs::create_dir_all(dir).ctx(|| format!("Creating {}", dir.display()))?;
        let tmp = p.with_extension("tmp");
        let json = serde_json::to_vec_pretty(value).map_err(|e| invalid(e.to_string()))?;
        std::fs::write(&tmp, json).ctx(|| format!("Writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &p).ctx(|| format!("Saving {}", p.display()))
    }

    pub fn settings(&self) -> Result<Settings> {
        self.read("settings.json")
    }

    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        self.write("settings.json", s)
    }

    pub fn user_apps(&self) -> Result<UserApps> {
        self.read("user-apps.json")
    }

    pub fn save_edit(&self, id: &str, edit: AppEdit) -> Result<()> {
        if let Some(g) = &edit.github {
            parse_github(g)?;
        }
        for d in edit.downloads.iter().flatten() {
            d.check()?;
        }
        let mut u = self.user_apps()?;
        if edit == AppEdit::default() {
            u.edits.remove(id);
        } else {
            u.edits.insert(id.to_string(), edit);
        }
        self.write("user-apps.json", &u)
    }

    /// Adds or replaces a hand-entered app. Its ID is derived from the name.
    pub fn save_custom_app(&self, mut app: App) -> Result<App> {
        if app.name.trim().is_empty() {
            return Err(invalid("Give the application a name."));
        }
        let slug: String = app
            .name
            .to_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        app.source = Source::Manual;
        app.native_id = slug.clone();
        app.id = format!("manual:{slug}");
        app.user_fields = vec!["all".into()];
        for l in &mut app.locations {
            l.origin = Origin::User;
            if !l.path.is_absolute() {
                return Err(invalid(format!(
                    "Locations must be absolute paths: {}",
                    l.path.display()
                )));
            }
        }
        if let Some(g) = &mut app.github {
            g.repo = parse_github(&g.repo)?;
            g.origin = Origin::User;
        }
        for d in &app.downloads {
            d.check()?;
        }
        let mut u = self.user_apps()?;
        u.custom.retain(|a| a.id != app.id);
        u.custom.push(app.clone());
        self.write("user-apps.json", &u)?;
        Ok(app)
    }

    pub fn delete_custom_app(&self, id: &str) -> Result<()> {
        let mut u = self.user_apps()?;
        u.custom.retain(|a| a.id != id);
        u.edits.remove(id);
        self.write("user-apps.json", &u)
    }

    pub fn profiles(&self) -> Result<Vec<Profile>> {
        let mut v: Vec<Profile> = self.read("profiles.json")?;
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    pub fn save_profile(&self, name: &str, selection: &Selection) -> Result<Profile> {
        check_name(name)?;
        let p = Profile {
            name: name.trim().into(),
            saved_at: crate::now(),
            selection: selection.clone(),
        };
        let mut v = self.profiles()?;
        v.retain(|x| x.name != p.name);
        v.push(p.clone());
        self.write("profiles.json", &v)?;
        Ok(p)
    }

    pub fn delete_profile(&self, name: &str) -> Result<()> {
        let mut v = self.profiles()?;
        v.retain(|x| x.name != name);
        self.write("profiles.json", &v)
    }

    pub fn snapshots(&self) -> Result<Vec<SnapshotRef>> {
        self.read("snapshots.json")
    }

    pub fn remember_snapshot(&self, r: SnapshotRef) -> Result<()> {
        let mut v = self.snapshots()?;
        v.retain(|x| x.path != r.path);
        v.insert(0, r);
        self.write("snapshots.json", &v)
    }

    pub fn update_snapshot(&self, path: &Path, f: impl FnOnce(&mut SnapshotRef)) -> Result<()> {
        let mut v = self.snapshots()?;
        if let Some(r) = v.iter_mut().find(|x| x.path == path) {
            f(r);
            self.write("snapshots.json", &v)?;
        }
        Ok(())
    }

    pub fn forget_snapshot(&self, path: &Path) -> Result<()> {
        let mut v = self.snapshots()?;
        v.retain(|x| x.path != path);
        self.write("snapshots.json", &v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::Kind;

    #[test]
    fn profiles_and_edits_round_trip() {
        let t = tempfile::tempdir().unwrap();
        let s = Store::at(t.path());
        assert!(s.profiles().unwrap().is_empty());
        s.save_profile("Work Setup", &Selection::default()).unwrap();
        s.save_profile("Work Setup", &Selection::default()).unwrap();
        assert_eq!(s.profiles().unwrap().len(), 1);
        assert!(s.save_profile("a/b", &Selection::default()).is_err());

        s.save_edit(
            "rpm:x",
            AppEdit {
                github: Some("https://github.com/o/r".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            s.save_edit(
                "rpm:x",
                AppEdit {
                    github: Some("not a repo/at/all!".into()),
                    ..Default::default()
                }
            )
            .is_err()
        );
        let mut apps = vec![App::new(Source::Rpm, "x", "X", Kind::Desktop)];
        s.user_apps().unwrap().apply(&mut apps);
        assert_eq!(apps[0].github.as_ref().unwrap().repo, "o/r");
        assert_eq!(apps[0].github.as_ref().unwrap().origin, Origin::User);

        let custom = s
            .save_custom_app(App::new(Source::Manual, "", "My Tool 2", Kind::CommandLine))
            .unwrap();
        assert_eq!(custom.id, "manual:my-tool-2");
    }
}
