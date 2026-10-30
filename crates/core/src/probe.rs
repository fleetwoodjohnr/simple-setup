//! What the computer being restored onto offers: operating system, processor, memory, free
//! space, the package managers and build tools on its PATH, administrator rights and internet
//! access. Read-only; the internet check is one short request to GitHub.

use std::path::Path;
use std::process::{Command, Stdio};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::installers::which;
use crate::platform::{Device, free_space};

/// Package managers and build tools Simple Setup can install with, by command name.
pub const TOOLS: &[&str] = &[
    "winget", "brew", "apt-get", "dnf", "zypper", "pacman", "flatpak", "snap", "cargo", "go",
    "pipx", "curl",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SystemInfo {
    pub device: Device,
    /// `<ID> <VERSION_ID>` of a Linux distribution.
    pub distro: Option<String>,
    pub cpu: String,
    pub cores: usize,
    pub memory_bytes: u64,
    /// Free space in the home folder's volume.
    pub free_bytes: Option<u64>,
    /// Entries of [`TOOLS`] found on this computer.
    pub tools: Vec<String>,
    /// Flatpak already knows the Flathub remote (otherwise it is added for this user).
    pub flathub: bool,
    /// Running with administrator rights (root, or an elevated Windows process).
    pub elevated: bool,
    /// GitHub answered, so downloads are possible.
    pub online: bool,
}

impl SystemInfo {
    pub fn has(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == tool)
    }
}

fn quiet(prog: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(prog)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn elevated() -> bool {
    if cfg!(windows) {
        // High (or system) mandatory level: the process was started "as administrator".
        quiet("whoami", &["/groups"])
            .is_some_and(|g| g.contains("S-1-16-12288") || g.contains("S-1-16-16384"))
    } else {
        quiet("id", &["-u"]).is_some_and(|u| u.trim() == "0")
    }
}

/// Looks at this computer. `network` decides whether to test internet access (a restore that
/// is offline by choice skips it).
pub fn detect(home: &Path, network: bool) -> crate::Result<SystemInfo> {
    let sys = System::new_with_specifics(
        RefreshKind::nothing()
            .with_memory(MemoryRefreshKind::nothing().with_ram())
            .with_cpu(CpuRefreshKind::nothing()),
    );
    let tools: Vec<String> = TOOLS
        .iter()
        .filter(|t| which(t).is_some())
        .map(|t| t.to_string())
        .collect();
    let flathub = tools.iter().any(|t| t == "flatpak")
        && quiet("flatpak", &["remotes", "--columns=name"])
            .is_some_and(|r| r.lines().any(|l| l.trim() == "flathub"));
    Ok(SystemInfo {
        device: Device::detect()?,
        distro: crate::platform::distro(),
        cpu: sys
            .cpus()
            .first()
            .map(|c| c.brand().trim().to_string())
            .filter(|b| !b.is_empty())
            .unwrap_or_else(System::cpu_arch),
        cores: sys.cpus().len(),
        memory_bytes: sys.total_memory(),
        free_bytes: free_space(home),
        tools,
        flathub,
        elevated: elevated(),
        online: network && crate::fetch::online(),
    })
}
