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

/// Processor architectures. How each is spelled by operating systems, package formats and
/// release files is data (`registry/arches.json`, see [`ArchInfo`]); a new architecture needs a
/// variant here because Simple Setup needs a restore engine built for it.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
    #[serde(rename = "i686")]
    I686,
    #[serde(rename = "armv7")]
    Armv7,
    #[serde(rename = "riscv64")]
    Riscv64,
    #[serde(rename = "ppc64le")]
    Ppc64le,
    #[serde(rename = "s390x")]
    S390x,
    #[serde(rename = "loong64")]
    Loong64,
    /// A processor Simple Setup has no name for (only ever this computer): portable formats only.
    #[serde(rename = "unknown")]
    Unknown,
}

/// One architecture's spellings (see `registry/arches.json`).
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArchInfo {
    pub label: String,
    /// Words that name it in release file names and system tools, lowercase.
    pub aliases: Vec<String>,
    /// `std::env::consts::ARCH` of programs built for it.
    pub rust: String,
    /// What `uname -m` prints.
    pub uname: Vec<String>,
    pub elf: Option<ElfMachine>,
    /// PE/COFF machine of Windows programs.
    #[serde(default)]
    pub pe: Option<u16>,
    #[serde(default)]
    pub deb: Option<String>,
    #[serde(default)]
    pub rpm: Option<String>,
    #[serde(default)]
    pub pacman: Option<String>,
    #[serde(default)]
    pub apk: Option<String>,
    #[serde(default)]
    pub flatpak: Option<String>,
    /// winget's installer architecture names.
    #[serde(default)]
    pub winget: Vec<String>,
    /// Homebrew cask's `arch` requirement word.
    #[serde(default)]
    pub cask: Option<String>,
    /// Systems that run its programs through emulation or a compatibility layer they ship.
    #[serde(default)]
    pub runs_on: Vec<RunsOn>,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ElfMachine {
    pub machine: u16,
    pub bits: u8,
    pub endian: Endian,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Endian {
    Little,
    Big,
}

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunsOn {
    pub os: OsFamily,
    pub arch: Arch,
    pub note: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchTable {
    #[allow(dead_code)]
    schema_version: u32,
    #[allow(dead_code)]
    about: String,
    arches: std::collections::BTreeMap<Arch, ArchInfo>,
}

static ARCHES: std::sync::LazyLock<std::collections::BTreeMap<Arch, ArchInfo>> =
    std::sync::LazyLock::new(|| {
        #[allow(clippy::unwrap_used)] // built in; `every_arch_has_a_row` checks it
        let t: ArchTable = serde_json::from_str(include_str!("../registry/arches.json")).unwrap();
        t.arches
    });

static UNKNOWN: std::sync::LazyLock<ArchInfo> = std::sync::LazyLock::new(|| ArchInfo {
    label: "an unknown processor".into(),
    aliases: vec![],
    rust: String::new(),
    uname: vec![],
    elf: None,
    pe: None,
    deb: None,
    rpm: None,
    pacman: None,
    apk: None,
    flatpak: None,
    winget: vec![],
    cask: None,
    runs_on: vec![],
});

impl Arch {
    /// Every named architecture (not [`Arch::Unknown`]).
    pub const ALL: [Self; 8] = [
        Self::X86_64,
        Self::Aarch64,
        Self::I686,
        Self::Armv7,
        Self::Riscv64,
        Self::Ppc64le,
        Self::S390x,
        Self::Loong64,
    ];

    pub fn info(self) -> &'static ArchInfo {
        ARCHES.get(&self).unwrap_or(&UNKNOWN)
    }

    /// "x86-64", "ARM64", …
    pub fn label(self) -> &'static str {
        &self.info().label
    }

    /// The architecture a word names: its canonical name or an alias (`amd64`, `arm64`,
    /// `armhf`, …), any case.
    pub fn parse(word: &str) -> Option<Self> {
        let w = word.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|a| a.to_string() == w || a.info().aliases.contains(&w))
    }

    /// The architecture of programs built with this `std::env::consts::ARCH` (and endianness).
    fn from_rust(arch: &str) -> Self {
        let little = cfg!(target_endian = "little");
        Self::ALL
            .into_iter()
            .find(|a| {
                a.info().rust == arch
                    && a.info()
                        .elf
                        .is_none_or(|e| (e.endian == Endian::Little) == little)
            })
            .unwrap_or(Self::Unknown)
    }

    /// From an ELF header's class (1 = 32-bit, 2 = 64-bit), data encoding (1 = little endian)
    /// and machine.
    pub fn from_elf(class: u8, data: u8, machine: u16) -> Option<Self> {
        let (bits, endian) = (
            if class == 2 { 64 } else { 32 },
            if data == 2 {
                Endian::Big
            } else {
                Endian::Little
            },
        );
        Self::ALL.into_iter().find(|a| {
            a.info()
                .elf
                .is_some_and(|e| e.machine == machine && e.bits == bits && e.endian == endian)
        })
    }

    /// From a PE/COFF header's machine field.
    pub fn from_pe(machine: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.info().pe == Some(machine))
    }

    /// From `uname -m` (or the Windows `PROCESSOR_ARCHITECTURE` words, which are aliases).
    pub fn from_uname(machine: &str) -> Option<Self> {
        let m = machine.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|a| a.info().uname.contains(&m))
            .or_else(|| Self::parse(&m))
    }

    /// Whether programs for this architecture run on `host` (natively, or through emulation the
    /// host system ships), and the note to show when it is emulation.
    pub fn runs_on(self, host: Platform) -> Option<Option<&'static str>> {
        if self == host.arch {
            return Some(None);
        }
        self.info()
            .runs_on
            .iter()
            .find(|r| r.os == host.os && r.arch == host.arch)
            .map(|r| Some(r.note.as_str()))
    }
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
            Self::I686 => "i686",
            Self::Armv7 => "armv7",
            Self::Riscv64 => "riscv64",
            Self::Ppc64le => "ppc64le",
            Self::S390x => "s390x",
            Self::Loong64 => "loong64",
            Self::Unknown => "unknown",
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
        // Any processor runs Simple Setup; one it has no name for gets portable formats only.
        let arch = Arch::from_rust(std::env::consts::ARCH);
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
    /// A network share (SMB, NFS, SSHFS, WebDAV…); its sizes are 0 when the server did not
    /// answer in time.
    #[serde(default)]
    pub network: bool,
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
            network: false,
        })
        .collect();
    let net = network_volumes();
    out.retain(|v| !net.iter().any(|n| n.mount == v.mount));
    out.extend(net);
    out.sort_by(|a, b| b.removable.cmp(&a.removable).then(a.mount.cmp(&b.mount)));
    out.dedup_by(|a, b| a.mount == b.mount);
    out
}

/// File systems of network shares.
const NETWORK_FS: &[&str] = &[
    "cifs",
    "smb3",
    "smbfs",
    "nfs",
    "nfs4",
    "fuse.sshfs",
    "sshfs",
    "davfs",
    "webdav",
    "fuse.rclone",
    "afpfs",
    "9p",
];

/// Total and free bytes of `mount`, asked with `df` so a share whose server does not answer
/// cannot hold up the list (given up after two seconds).
#[cfg(unix)]
fn space_within(mount: &Path) -> Option<(u64, u64)> {
    let (tx, rx) = std::sync::mpsc::channel();
    let m = mount.to_path_buf();
    std::thread::spawn(move || {
        let out = std::process::Command::new("df").arg("-kP").arg(&m).output();
        let _ = tx.send(out);
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let w: Vec<&str> = text.lines().nth(1)?.split_whitespace().collect();
    Some((
        w.get(1)?.parse::<u64>().ok()? * 1024,
        w.get(3)?.parse::<u64>().ok()? * 1024,
    ))
}

/// Mounted network shares: from the mount table on Linux and macOS, mapped drives on Windows.
fn network_volumes() -> Vec<Volume> {
    let make = |name: &str, mount: PathBuf, fs: &str, space: Option<(u64, u64)>| Volume {
        name: name.to_string(),
        mount,
        file_system: fs.to_string(),
        total: space.map_or(0, |s| s.0),
        free: space.map_or(0, |s| s.1),
        removable: false,
        network: true,
    };
    #[cfg(target_os = "linux")]
    {
        let text = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
        parse_mounts(&text)
            .into_iter()
            .map(|(dev, mount, fs)| {
                let space = space_within(&mount);
                make(&dev, mount, &fs, space)
            })
            .collect()
    }
    #[cfg(target_os = "macos")]
    {
        // "//me@server/share on /Volumes/share (smbfs, nodev, nosuid, mounted by me)"
        let out = std::process::Command::new("mount").output();
        let text = out
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        text.lines()
            .filter_map(|l| {
                let (dev, rest) = l.split_once(" on ")?;
                let (mount, opts) = rest.rsplit_once(" (")?;
                let fs = opts.split([',', ')']).next()?.trim();
                NETWORK_FS.contains(&fs).then(|| {
                    let mount = PathBuf::from(mount);
                    let space = space_within(&mount);
                    make(dev, mount, fs, space)
                })
            })
            .collect()
    }
    #[cfg(windows)]
    {
        // Mapped network drives, asked in a thread so a lost server cannot hold up the list.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let out = std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    "Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=4' | Select-Object DeviceID,ProviderName,Size,FreeSpace | ConvertTo-Json -Compress",
                ])
                .output();
            let _ = tx.send(out);
        });
        let Ok(Ok(out)) = rx.recv_timeout(std::time::Duration::from_secs(4)) else {
            return Vec::new();
        };
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
        let list = match v {
            serde_json::Value::Array(a) => a,
            o @ serde_json::Value::Object(_) => vec![o],
            _ => Vec::new(),
        };
        list.iter()
            .filter_map(|d| {
                let id = d["DeviceID"].as_str()?;
                let name = d["ProviderName"].as_str().unwrap_or(id);
                let space = Some((
                    d["Size"].as_u64().unwrap_or(0),
                    d["FreeSpace"].as_u64().unwrap_or(0),
                ));
                Some(make(
                    name,
                    PathBuf::from(format!("{id}\\")),
                    "network",
                    space,
                ))
            })
            .collect()
    }
}

/// Network shares in a Linux mount table (`/proc/self/mounts`): device, mount point (with the
/// table's octal escapes undone) and file system.
pub fn parse_mounts(text: &str) -> Vec<(String, PathBuf, String)> {
    let unescape = |s: &str| {
        let mut out = Vec::new();
        let b = s.as_bytes();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'\\'
                && i + 3 < b.len()
                && let Ok(n) = u8::from_str_radix(&s[i + 1..i + 4], 8)
            {
                out.push(n);
                i += 4;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    text.lines()
        .filter_map(|l| {
            let mut w = l.split(' ');
            let (dev, mount, fs) = (w.next()?, w.next()?, w.next()?);
            NETWORK_FS.contains(&fs).then(|| {
                (
                    unescape(dev),
                    PathBuf::from(unescape(mount)),
                    fs.to_string(),
                )
            })
        })
        .collect()
}

/// The file system of the volume holding `path` (`vfat`, `exfat`, `ntfs`, `FAT32`…).
pub fn file_system(path: &Path) -> Option<String> {
    let path = path.canonicalize().ok()?;
    Disks::new_with_refreshed_list()
        .iter()
        .filter(|d| path.starts_with(d.mount_point()))
        .max_by_key(|d| d.mount_point().as_os_str().len())
        .map(|d| d.file_system().to_string_lossy().into_owned())
}

/// FAT file systems (FAT32, not exFAT) hold no file of 4 GiB or more.
pub fn fat_limited(file_system: &str) -> bool {
    matches!(
        file_system.to_ascii_lowercase().as_str(),
        "vfat" | "fat" | "fat32" | "fat16" | "msdos"
    )
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
    use super::*;

    #[test]
    fn every_arch_has_a_row() {
        for a in Arch::ALL {
            let i = a.info();
            assert!(
                !i.label.is_empty() && i.aliases.contains(&a.to_string()),
                "{a}"
            );
            assert_eq!(Arch::parse(&a.to_string()), Some(a));
            let json = serde_json::to_string(&a).unwrap();
            assert_eq!(json, format!("\"{a}\""));
        }
        // No word names two architectures.
        let mut seen = std::collections::HashMap::new();
        for a in Arch::ALL {
            for w in &a.info().aliases {
                assert_eq!(*seen.entry(w.clone()).or_insert(a), a, "{w}");
            }
        }
        assert_eq!(Arch::from_elf(2, 1, 62), Some(Arch::X86_64));
        assert_eq!(Arch::from_elf(1, 1, 40), Some(Arch::Armv7));
        assert_eq!(Arch::from_elf(2, 1, 243), Some(Arch::Riscv64));
        assert_eq!(
            Arch::from_elf(1, 1, 243),
            None,
            "32-bit RISC-V is not riscv64"
        );
        assert_eq!(Arch::from_pe(0xAA64), Some(Arch::Aarch64));
        assert_eq!(Arch::from_uname("armv7l"), Some(Arch::Armv7));
        assert_eq!(Arch::from_uname("arm64"), Some(Arch::Aarch64));
        assert_eq!(Arch::parse("AMD64"), Some(Arch::X86_64));
        let win_arm = Platform {
            os: OsFamily::Windows,
            arch: Arch::Aarch64,
        };
        assert!(Arch::X86_64.runs_on(win_arm).is_some_and(|n| n.is_some()));
        let linux_arm = Platform {
            os: OsFamily::Linux,
            arch: Arch::Aarch64,
        };
        assert!(Arch::X86_64.runs_on(linux_arm).is_none());
        assert_eq!(
            Platform::current()
                .unwrap()
                .arch
                .runs_on(Platform::current().unwrap()),
            Some(None)
        );
    }

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

    #[test]
    fn network_mounts_from_the_mount_table() {
        let text = "/dev/nvme0n1p3 / btrfs rw 0 0\n//nas/Family\\040Photos /mnt/nas\\040photos cifs rw 0 0\nserver:/export /srv/nfs nfs4 rw 0 0\nme@box:/home /mnt/box fuse.sshfs rw 0 0\ntmpfs /tmp tmpfs rw 0 0\n";
        let m = parse_mounts(text);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].0, "//nas/Family Photos");
        assert_eq!(m[0].1, PathBuf::from("/mnt/nas photos"));
        assert_eq!(m[2].2, "fuse.sshfs");
        assert!(fat_limited("vfat") && fat_limited("FAT32") && !fat_limited("exfat"));
    }
}
