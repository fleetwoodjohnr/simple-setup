//! Operating system, CPU architecture, device identity and storage volumes.

use std::fmt;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sysinfo::{Disks, System};

use crate::{Result, invalid};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum OsFamily {
    Linux,
    Windows,
    Macos,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub os: OsFamily,
    pub arch: Arch,
}

impl fmt::Display for OsFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
            Self::Macos => "macos",
        })
    }
}

/// "Linux", "Windows", "macOS".
pub fn os_label(os: OsFamily) -> &'static str {
    match os {
        OsFamily::Linux => "Linux",
        OsFamily::Windows => "Windows",
        OsFamily::Macos => "macOS",
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        })
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.os, self.arch)
    }
}

impl Platform {
    pub fn current() -> Result<Self> {
        let os = match std::env::consts::OS {
            "linux" => OsFamily::Linux,
            "windows" => OsFamily::Windows,
            "macos" => OsFamily::Macos,
            other => return Err(invalid(format!("Unsupported operating system: {other}"))),
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => Arch::X86_64,
            "aarch64" => Arch::Aarch64,
            other => return Err(invalid(format!("Unsupported CPU architecture: {other}"))),
        };
        Ok(Self { os, arch })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Device {
    pub hostname: String,
    /// Human-readable OS name and version, e.g. "Fedora Linux 44".
    pub os_name: String,
    pub platform: Platform,
}

impl Device {
    pub fn detect() -> Result<Self> {
        let os_name = [System::name(), System::os_version()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        Ok(Self {
            hostname: System::host_name().unwrap_or_else(|| "unknown".into()),
            os_name: if os_name.is_empty() {
                std::env::consts::OS.into()
            } else {
                os_name
            },
            platform: Platform::current()?,
        })
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Volume {
    pub name: String,
    pub mount: PathBuf,
    pub file_system: String,
    pub total: u64,
    pub free: u64,
    pub removable: bool,
}

/// Mounted volumes a snapshot could be written to: removable drives and anything mounted
/// under the usual external-media locations. Internal disks are reachable via a folder picker.
pub fn volumes() -> Vec<Volume> {
    let mut out: Vec<Volume> = Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| {
            let m = d.mount_point();
            d.is_removable()
                || cfg!(windows)
                || ["/run/media", "/media", "/mnt", "/Volumes"]
                    .iter()
                    .any(|p| m.starts_with(p))
        })
        .map(|d| Volume {
            name: d.name().to_string_lossy().into_owned(),
            mount: d.mount_point().to_path_buf(),
            file_system: d.file_system().to_string_lossy().into_owned(),
            total: d.total_space(),
            free: d.available_space(),
            removable: d.is_removable(),
        })
        .collect();
    out.sort_by(|a, b| b.removable.cmp(&a.removable).then(a.mount.cmp(&b.mount)));
    out.dedup_by(|a, b| a.mount == b.mount);
    out
}

/// Free bytes on the volume holding `path` (longest matching mount point).
pub fn free_space(path: &Path) -> Option<u64> {
    let path = path.canonicalize().ok()?;
    Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.available_space())
}

/// The fields of `/etc/os-release` Simple Setup uses (see os-release(5)).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OsRelease {
    /// `ID`, lowercase, e.g. `fedora`, `linuxmint`.
    pub id: String,
    /// `ID_LIKE`: distributions this one is derived from or similar to, closest first.
    #[serde(default)]
    pub id_like: Vec<String>,
    #[serde(default)]
    pub version_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub pretty_name: Option<String>,
    /// `VARIANT_ID`, e.g. `silverblue`, `workstation`.
    #[serde(default)]
    pub variant_id: Option<String>,
    #[serde(default)]
    pub version_codename: Option<String>,
}

impl OsRelease {
    pub fn parse(text: &str) -> Option<Self> {
        let get = |k: &str| {
            text.lines()
                .find_map(|l| l.trim_start().strip_prefix(k)?.strip_prefix('='))
                .map(|v| {
                    let v = v.trim();
                    let v = v
                        .strip_prefix('"')
                        .and_then(|v| v.strip_suffix('"'))
                        .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                        .unwrap_or(v);
                    v.replace("\\\"", "\"").replace("\\\\", "\\")
                })
                .filter(|v| !v.is_empty())
        };
        Some(Self {
            id: get("ID")?.to_ascii_lowercase(),
            id_like: get("ID_LIKE")
                .map(|v| v.split_whitespace().map(str::to_ascii_lowercase).collect())
                .unwrap_or_default(),
            version_id: get("VERSION_ID"),
            name: get("NAME"),
            pretty_name: get("PRETTY_NAME"),
            variant_id: get("VARIANT_ID").map(|v| v.to_ascii_lowercase()),
            version_codename: get("VERSION_CODENAME"),
        })
    }

    /// This computer's, from `/etc/os-release` or `/usr/lib/os-release`.
    pub fn read() -> Option<Self> {
        ["/etc/os-release", "/usr/lib/os-release"]
            .iter()
            .find_map(|p| Self::parse(&std::fs::read_to_string(p).ok()?))
    }

    /// `<ID> <VERSION_ID>`, e.g. `fedora 44`: what distribution packages are tied to.
    pub fn release(&self) -> String {
        match &self.version_id {
            Some(v) => format!("{} {v}", self.id),
            None => self.id.clone(),
        }
    }

    /// e.g. "Linux Mint 22".
    pub fn display(&self) -> String {
        let name = self.name.clone().unwrap_or_else(|| self.id.clone());
        match &self.version_id {
            Some(v) if !name.contains(v.as_str()) => format!("{name} {v}"),
            _ => name,
        }
    }
}

/// `<ID> <VERSION_ID>` of this computer's Linux distribution, e.g. `fedora 44`. Distribution
/// package sets are only offered for restore on the same release.
pub fn distro() -> Option<String> {
    Some(OsRelease::read()?.release())
}

/// The `ID` of this computer's Linux distribution, e.g. `fedora` or `linuxmint`.
pub fn distro_id() -> Option<String> {
    Some(OsRelease::read()?.id)
}

pub fn parse_os_release(text: &str) -> Option<String> {
    Some(OsRelease::parse(text)?.release())
}

#[cfg(test)]
mod tests {
    use super::OsRelease;

    #[test]
    fn os_release() {
        let t = "NAME=\"Fedora Linux\"\nID_LIKE=x\nID=fedora\nVERSION=\"44 (Workstation)\"\nVERSION_ID=44\n";
        assert_eq!(super::parse_os_release(t).as_deref(), Some("fedora 44"));
        assert_eq!(
            super::parse_os_release("ID=arch\n").as_deref(),
            Some("arch")
        );
        let mint = OsRelease::parse(
            "NAME=\"Linux Mint\"\nVERSION=\"22 (Wilma)\"\nID=linuxmint\nID_LIKE=\"ubuntu debian\"\nPRETTY_NAME=\"Linux Mint 22\"\nVERSION_ID=\"22\"\nVERSION_CODENAME=wilma\nUBUNTU_CODENAME=noble\n",
        )
        .unwrap();
        assert_eq!(mint.id_like, ["ubuntu", "debian"]);
        assert_eq!(mint.version_codename.as_deref(), Some("wilma"));
        assert_eq!(mint.display(), "Linux Mint 22");
        let sb =
            OsRelease::parse("ID=fedora\nVARIANT_ID=Silverblue\nNAME='Fedora Linux'\n").unwrap();
        assert_eq!(sb.variant_id.as_deref(), Some("silverblue"));
        assert_eq!(sb.name.as_deref(), Some("Fedora Linux"));
        assert!(OsRelease::parse("NAME=x\n").is_none());
    }
}
