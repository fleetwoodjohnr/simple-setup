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
    "scoop",
    "choco",
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
    /// Operating system version as the system reports it ("11 (26100)", "15.1", "44").
    #[serde(default)]
    pub os_version: Option<String>,
    /// Linux C library: "glibc 2.40" or "musl" (programs built for one may not run on the other).
    #[serde(default)]
    pub libc: Option<String>,
    /// Administrator rights can be asked for (sudo or pkexec; Windows' consent prompt).
    #[serde(default)]
    pub can_elevate: bool,
    /// Other processors' programs this computer runs (Rosetta 2, Windows' x86 emulation).
    #[serde(default)]
    pub emulates: Vec<Arch>,
    /// Installer formats and package managers this computer can use right now.
    #[serde(default)]
    pub formats: Vec<String>,
    /// Things worth knowing about this computer's compatibility, in plain language.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl SystemInfo {
    pub fn has(&self, tool: &str) -> bool {
        self.tools.iter().any(|t| t == tool)
    }

    /// Fills in what follows from the platform, profile and tools: emulation, usable formats
    /// and notes. `rosetta`: Rosetta 2 is installed (Apple Silicon only).
    fn derive(&mut self, rosetta: bool) {
        let p = self.device.platform;
        self.emulates = Arch::ALL
            .into_iter()
            .filter(|a| matches!(a.runs_on(p), Some(Some(_))))
            .filter(|_| !(p.os == OsFamily::Macos && !rosetta))
            .collect();
        let has = |t: &str| self.tools.iter().any(|x| x == t);
        let mut f: Vec<String> = match p.os {
            OsFamily::Windows => ["msi", "exe", "msix", "zip"].map(String::from).to_vec(),
            OsFamily::Macos => ["dmg", "pkg", "app", "zip"].map(String::from).to_vec(),
            OsFamily::Linux => Vec::new(),
        };
        if let Some(l) = &self.linux {
            let pr = &l.profile;
            if !l.immutable {
                f.extend(pr.formats.iter().map(|x| x.label().to_ascii_lowercase()));
            }
            if pr.appimage != Avail::Unsupported {
                f.push("appimage".into());
            }
            f.push("tar.gz".into());
            if !pr.arch.is_empty() && !pr.arch.contains(&p.arch.to_string()) {
                self.notes.push(format!(
                    "{} is not made for {} processors; only portable programs built for {} are used.",
                    pr.name,
                    p.arch.label(),
                    p.arch.label()
                ));
            }
        }
        for t in ["flatpak", "snap", "winget", "brew", "scoop", "choco"] {
            if has(t) {
                f.push(t.into());
            }
        }
        self.formats = f;
        if p.arch == Arch::Unknown {
            self.notes.push(
                "Simple Setup does not know this processor; only apps from package managers and portable programs can be installed."
                    .into(),
            );
        }
        if p.os == OsFamily::Macos && p.arch == Arch::Aarch64 && !rosetta {
            self.notes.push(
                "Rosetta 2 is not installed, so apps made only for Intel Macs are not used.".into(),
            );
        }
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
        let mut info = Self {
            device: Device {
                hostname: "the new computer".into(),
                os_name,
                platform,
            },
            // `<ID>` alone: without a release, release-bound package sets never match.
            distro: linux.as_ref().map(|l| l.os_release.release()),
            linux,
            cpu: platform.arch.label().into(),
            cores: 0,
            memory_bytes: 0,
            free_bytes: None,
            tools,
            flathub,
            elevated: false,
            online: true,
            os_version: None,
            libc: None,
            can_elevate: true,
            emulates: Vec::new(),
            formats: Vec::new(),
            notes: Vec::new(),
        };
        // A Mac described from elsewhere is assumed to have Rosetta, as most do.
        info.derive(true);
        info
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
    let mut notes = Vec::new();
    let mut tools = tools;
    // The snap command can be installed while the snapd service is not running (containers,
    // systems where it was switched off): snaps cannot be installed then.
    if tools.iter().any(|t| t == "snap")
        && quiet("snap", &["version"]).is_none_or(|v| v.contains("unavailable"))
    {
        tools.retain(|t| t != "snap");
        notes.push(
            "The snap command is here but snapd is not running, so snaps are not used.".into(),
        );
    }
    let mut info = SystemInfo {
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
        os_version: System::os_version(),
        libc: libc(),
        can_elevate: elevated()
            || cfg!(windows)
            || which("sudo").is_some()
            || which("pkexec").is_some(),
        emulates: Vec::new(),
        formats: Vec::new(),
        notes,
    };
    info.derive(Path::new("/Library/Apple/usr/libexec/oah/libRosettaRuntime").exists());
    Ok(info)
}

/// "glibc 2.40" or "musl" on Linux.
fn libc() -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    if let Some(v) = quiet("getconf", &["GNU_LIBC_VERSION"]) {
        return Some(v.trim().to_string());
    }
    let musl = ["/lib", "/usr/lib"].iter().any(|d| {
        std::fs::read_dir(d).is_ok_and(|r| {
            r.flatten()
                .any(|e| e.file_name().to_string_lossy().starts_with("ld-musl-"))
        })
    });
    musl.then(|| "musl".into())
}
