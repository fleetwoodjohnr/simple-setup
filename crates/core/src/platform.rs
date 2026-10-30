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

/// `<ID> <VERSION_ID>` from `/etc/os-release`, e.g. `fedora 44`. Distribution packages are
/// only offered for restore on the same release.
pub fn distro() -> Option<String> {
    parse_os_release(&std::fs::read_to_string("/etc/os-release").ok()?)
}

pub fn parse_os_release(text: &str) -> Option<String> {
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_string())
    };
    let id = get("ID")?;
    Some(match get("VERSION_ID") {
        Some(v) => format!("{id} {v}"),
        None => id,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn os_release() {
        let t = "NAME=\"Fedora Linux\"\nID_LIKE=x\nID=fedora\nVERSION=\"44 (Workstation)\"\nVERSION_ID=44\n";
        assert_eq!(super::parse_os_release(t).as_deref(), Some("fedora 44"));
        assert_eq!(
            super::parse_os_release("ID=arch\n").as_deref(),
            Some("arch")
        );
    }
}
