//! What the computer being restored onto offers: operating system, processor, memory, free
//! space, the package managers and build tools on its PATH, administrator rights and internet
//! access. Read-only; the internet check is one short request to GitHub.

use std::path::Path;
use std::process::{Command, Stdio};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sysinfo::{CpuRefreshKind, MemoryRefreshKind, RefreshKind, System};

use crate::distros::{Avail, Distros, Matched, Resolved};
use crate::installers::which;
use crate::platform::{Arch, Device, OsFamily, OsRelease, Platform, free_space};

/// Package managers and build tools Simple Setup can install with, by command name.
pub const TOOLS: &[&str] = &[
    "winget",
    "brew",
    "apt-get",
    "dnf",
    "zypper",
    "pacman",
    "apk",
    "rpm-ostree",
    "transactional-update",
    "flatpak",
    "snap",
    "cargo",
    "go",
    "pipx",
    "curl",
];

/// What a Linux computer is: its os-release, the distribution profile that describes it and
/// what it can install.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LinuxHost {
    pub os_release: OsRelease,
    /// For diagnostics only; compatibility never depends on it.
    #[serde(default)]
    pub kernel: Option<String>,
    /// Processor as the kernel names it (`x86_64`, `aarch64`, `armv7l`…).
    pub machine: String,
    pub profile: Resolved,
    pub matched: Matched,
    /// One sentence on how the profile was chosen.
    pub explanation: String,
    /// The system files are read-only (image-based: ostree, transactional updates…).
    pub immutable: bool,
    /// AppImages can be mounted (FUSE); without it they are started unpacked.
    pub fuse: bool,
}

impl LinuxHost {
    /// Works out the profile for `os` (or the user's `override_id`) and what this system is.
    pub fn from_parts(
        os: OsRelease,
        distros: &Distros,
        override_id: Option<&str>,
        kernel: Option<String>,
        machine: String,
        image_based: bool,
        fuse: bool,
    ) -> Self {
        let (profile, matched) = match override_id.and_then(|id| distros.resolve(id).ok()) {
            Some(r) => (r, Matched::Override),
            None => distros.detect(&os),
        };
        let explanation = matched.explain(&profile, &os);
        Self {
            immutable: image_based || profile.immutable,
            kernel,
            machine,
            explanation,
            profile,
            matched,
            fuse,
            os_release: os,
        }
    }
}

/// This computer's distribution and profile (Linux only), honouring the override in Settings.
pub fn local_linux(distros: &Distros) -> Option<LinuxHost> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let override_id = crate::store::Store::open_default()
        .ok()
        .and_then(|s| s.settings().ok())
        .and_then(|s| s.distro_override);
    Some(LinuxHost::from_parts(
        OsRelease::read().unwrap_or_else(|| OsRelease {
            id: "linux".into(),
            ..Default::default()
        }),
        distros,
        override_id.as_deref(),
        System::kernel_version(),
        System::cpu_arch(),
        image_based(),
        crate::installers::fuse_available(),
    ))
}

/// The system is image-based: Fedora Atomic / CoreOS (ostree) or openSUSE's transactional
/// editions. Read-only checks only.
fn image_based() -> bool {
    Path::new("/run/ostree-booted").exists() || which("transactional-update").is_some()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SystemInfo {
    pub device: Device,
    /// `<ID> <VERSION_ID>` of a Linux distribution.
    pub distro: Option<String>,
    /// Distribution details and profile (Linux only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linux: Option<LinuxHost>,
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

    /// The distribution profile, for Linux computers.
    pub fn profile(&self) -> Option<&Resolved> {
        self.linux.as_ref().map(|l| &l.profile)
    }

    /// A computer that is not here, described only by its system, processor and (Linux)
    /// distribution profile: it is assumed to have what that system has out of the box (its
    /// package manager, Flatpak or snapd where they are standard) and to be online.
    pub fn assumed(platform: Platform, profile: Option<Resolved>) -> Self {
        let mut tools: Vec<String> = vec!["curl".into()];
        let mut flathub = false;
        let linux = match (platform.os, profile) {
            (OsFamily::Linux, p) => {
                let p = p.unwrap_or_else(|| {
                    #[allow(clippy::unwrap_used)] // built in and tested
                    Distros::builtin().resolve(crate::distros::ROOT).unwrap()
                });
                if let Some(t) = p.package_manager.tool() {
                    tools.push(t.into());
                }
                if p.flatpak == Avail::Default {
                    tools.push("flatpak".into());
                    flathub = true;
                }
                if p.snap == Avail::Default {
                    tools.push("snap".into());
                }
                let os = OsRelease {
                    id: p.id.clone(),
                    name: Some(p.name.clone()),
                    ..Default::default()
                };
                Some(LinuxHost {
                    explanation: format!("Assumed to be a standard {} installation.", p.name),
                    machine: platform.arch.to_string(),
                    kernel: None,
                    immutable: p.immutable,
                    fuse: p.appimage == Avail::Default,
                    matched: Matched::Exact,
                    os_release: os,
                    profile: p,
                })
            }
            (OsFamily::Windows, _) => {
                tools.push("winget".into());
                None
            }
            (OsFamily::Macos, _) => None,
        };
        let os_name = match &linux {
            Some(l) => l.profile.name.clone(),
            None => match platform.os {
                OsFamily::Windows => "Windows".into(),
                _ => "macOS".into(),
            },
        };
        Self {
            device: Device {
                hostname: "the new computer".into(),
                os_name,
                platform,
            },
            distro: linux.as_ref().map(|l| l.profile.id.clone()),
            linux,
            cpu: match platform.arch {
                Arch::X86_64 => "x86-64".into(),
                Arch::Aarch64 => "ARM64".into(),
            },
            cores: 0,
            memory_bytes: 0,
            free_bytes: None,
            tools,
            flathub,
            elevated: false,
            online: true,
        }
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
    let device = Device::detect()?;
    let linux = local_linux(&Distros::load_default());
    Ok(SystemInfo {
        device,
        distro: crate::platform::distro(),
        linux,
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
