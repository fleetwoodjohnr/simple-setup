//! What the user chose to put in a snapshot. Nothing is included by default: an empty
//! selection is an error, and every app part is an explicit opt-in.

use std::collections::HashSet;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::discovery::Inventory;
use crate::platform::Platform;
use crate::{Result, invalid};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub apps: Vec<AppChoice>,
    pub folders: Vec<FolderChoice>,
    /// Secret-flagged files or folders the user explicitly chose to include anyway.
    #[serde(default)]
    pub include_secrets: Vec<PathBuf>,
    pub targets: Vec<Platform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppChoice {
    /// Inventory app ID, e.g. `flatpak:org.mozilla.firefox`.
    pub id: String,
    /// Bundle an installer / reinstall recipe.
    pub installer: bool,
    /// Copy settings locations.
    pub config: bool,
    /// Copy data/profile locations.
    pub data: bool,
    /// Sub-paths of the app's locations to leave out.
    #[serde(default)]
    pub exclude: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FolderChoice {
    pub path: PathBuf,
    /// Set for the standard folders so a restore can map them to the target's equivalent.
    #[serde(default)]
    pub known: Option<KnownFolder>,
    #[serde(default)]
    pub exclude: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
pub enum KnownFolder {
    Documents,
    Desktop,
    Downloads,
    Pictures,
    Videos,
    Music,
}

impl KnownFolder {
    pub const ALL: [Self; 6] = [
        Self::Documents,
        Self::Desktop,
        Self::Downloads,
        Self::Pictures,
        Self::Videos,
        Self::Music,
    ];

    /// The folder's usual name inside the home folder.
    pub fn name(self) -> &'static str {
        match self {
            Self::Documents => "Documents",
            Self::Desktop => "Desktop",
            Self::Downloads => "Downloads",
            Self::Pictures => "Pictures",
            Self::Videos => "Videos",
            Self::Music => "Music",
        }
    }

    /// This computer's location for the folder: the system's setting (XDG user dirs on Linux,
    /// known folders on Windows and macOS), else `~/<name>` (Linux without user-dirs.dirs).
    pub fn path(self) -> Option<PathBuf> {
        let set = match self {
            Self::Documents => dirs::document_dir(),
            Self::Desktop => dirs::desktop_dir(),
            Self::Downloads => dirs::download_dir(),
            Self::Pictures => dirs::picture_dir(),
            Self::Videos => dirs::video_dir(),
            Self::Music => dirs::audio_dir(),
        };
        set.or_else(|| dirs::home_dir().map(|h| h.join(self.name())))
    }
}

impl Selection {
    /// Checks the selection against the current inventory before anything is read or written.
    pub fn validate(&self, inv: &Inventory) -> Result<()> {
        if self.apps.is_empty() && self.folders.is_empty() {
            return Err(invalid(
                "Nothing is selected. Choose at least one application or folder.",
            ));
        }
        if self.targets.is_empty() {
            return Err(invalid("Choose at least one target operating system."));
        }
        let mut seen = HashSet::new();
        for c in &self.apps {
            if !seen.insert(&c.id) {
                return Err(invalid(format!("{} is selected twice.", c.id)));
            }
            if !inv.apps.iter().any(|a| a.id == c.id) {
                return Err(invalid(format!(
                    "{} was not found on this computer. Scan again.",
                    c.id
                )));
            }
            if !(c.installer || c.config || c.data) {
                return Err(invalid(format!(
                    "Choose installer, settings or data for {}.",
                    c.id
                )));
            }
        }
        for p in self
            .folders
            .iter()
            .flat_map(|f| std::iter::once(&f.path).chain(&f.exclude))
            .chain(self.apps.iter().flat_map(|a| &a.exclude))
            .chain(&self.include_secrets)
        {
            if !p.is_absolute() {
                return Err(invalid(format!(
                    "Folder paths must be absolute: {}",
                    p.display()
                )));
            }
        }
        for f in &self.folders {
            if !f.path.exists() {
                return Err(invalid(format!("{} does not exist.", f.path.display())));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_known_folder_has_a_place() {
        // Without user-dirs.dirs (minimal Linux installs, CI) the usual name is used.
        for k in KnownFolder::ALL {
            let p = k.path().unwrap();
            assert!(p.is_absolute(), "{p:?}");
        }
    }
}
