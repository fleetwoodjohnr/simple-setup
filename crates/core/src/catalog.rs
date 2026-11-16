//! Vetted installer catalogs: winget's community manifests (Windows), Homebrew's cask API
//! (macOS) and GitHub releases. Consulted while a snapshot is built, and during a restore for
//! apps the snapshot holds no installer for. This module only reads catalog answers and chooses
//! an installer; `fetch` downloads it, and the file must match the SHA-256 the catalog publishes
//! before it is used.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::Deserialize;

use crate::manifest::{WinInstaller, silent_args_ok};
use crate::platform::{Arch, OsFamily, Platform};

/// An installer chosen from a catalog.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pick {
    pub url: String,
    /// Lowercase hex; empty when the source publishes no checksum (Homebrew `no_check`, a
    /// GitHub asset with neither a digest nor a checksum list): the file is then taken over
    /// HTTPS and its own SHA-256 is pinned in the snapshot.
    pub sha256: String,
    pub version: String,
    /// Windows installer technology (decides the fixed silent options), if known.
    pub win: Option<WinInstaller>,
    /// What was chosen, for the evidence list (architecture, type, asset name…).
    pub detail: String,
    /// File name of a GitHub release asset (to find it in a checksum list).
    pub name: String,
    /// A checksum list in the same release to read the SHA-256 from, when GitHub publishes no
    /// digest for the asset (`sha256` is then empty until it is read).
    pub sums_url: Option<String>,
    /// A `.tar.gz`, `.tar.xz` or `.zip` archive holding a self-contained program.
    pub archive: bool,
    /// For an `.exe` installer: the switches the catalog lists for an unattended install
    /// (possibly none, for installers that are silent by themselves).
    pub silent: Option<Vec<String>>,
    /// A GitHub asset's API address: the way to download it from a private repository.
    pub api_url: Option<String>,
    /// A bare program file (no installer, no archive).
    pub program: bool,
}

fn sha256_hex(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

fn https(url: &str) -> bool {
    url.starts_with("https://") && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Semantic version order: numbers compared numerically ("10.1" > "9.12"), a prerelease
/// (`1.2.0-rc1`) before its release, build metadata ignored. Unparsable parts count as 0.
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    let split = |v: &str| {
        let v = v.trim().trim_start_matches(['v', 'V']);
        let v = v.split('+').next().unwrap_or_default();
        let (core, pre) = v.split_once('-').map_or((v, None), |(c, p)| (c, Some(p)));
        let nums: Vec<u64> = core
            .split(['.', '_'])
            .map(|n| {
                let digits: String = n.chars().take_while(char::is_ascii_digit).collect();
                digits.parse().unwrap_or(0)
            })
            .collect();
        (nums, pre.map(str::to_string))
    };
    let ((mut na, pa), (mut nb, pb)) = (split(a), split(b));
    let len = na.len().max(nb.len()).max(3);
    na.resize(len, 0);
    nb.resize(len, 0);
    na.cmp(&nb).then_with(|| match (pa, pb) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => {
            let ids = |s: &str| -> Vec<(u64, String)> {
                s.split('.')
                    .map(|p| (p.parse().unwrap_or(u64::MAX), p.to_string()))
                    .collect()
            };
            ids(&x).cmp(&ids(&y))
        }
    })
}

// ---------------------------------------------------------------- winget

/// The repository folder of a winget package ID: `Mozilla.Firefox` → `manifests/m/Mozilla/Firefox`.
pub fn winget_dir(id: &str) -> Option<String> {
    let parts: Vec<&str> = id.split('.').collect();
    let ok = parts.len() >= 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_+".contains(c))
        });
    let first = id.chars().next()?.to_ascii_lowercase();
    ok.then(|| format!("manifests/{first}/{}", parts.join("/")))
}

#[derive(Debug, Deserialize)]
pub struct DirEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

/// Version folders of a winget package, newest first. Other folders are sub-packages (locales,
/// ESR, betas) and are ignored.
pub fn winget_versions(listing: &[DirEntry]) -> Vec<String> {
    let mut v: Vec<String> = listing
        .iter()
        .filter(|e| e.kind == "dir" && e.name.starts_with(|c: char| c.is_ascii_digit()))
        .map(|e| e.name.clone())
        .collect();
    v.sort_by(|a, b| version_cmp(b, a));
    v
}

fn yaml_kv(s: &str) -> Option<(String, String)> {
    let (k, v) = s.split_once(':')?;
    let v = v.trim();
    let v = v
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
        .unwrap_or(v);
    Some((k.trim().to_string(), v.to_string()))
}

/// A line nested under `parent`: `Key: value` is kept as `parent.Key`, and a plain list
/// item is appended to `parent` (comma-separated). Deeper structures are ignored.
fn yaml_child(map: &mut BTreeMap<String, String>, parent: Option<&str>, body: &str) {
    let Some(p) = parent else { return };
    if let Some(item) = body.strip_prefix("- ") {
        if !item.contains(':') {
            let e = map.entry(p.to_string()).or_default();
            if !e.is_empty() {
                e.push(',');
            }
            e.push_str(item.trim());
        }
    } else if let Some((k, v)) = yaml_kv(body).filter(|(_, v)| !v.is_empty()) {
        map.insert(format!("{p}.{k}"), v);
    }
}

/// Top-level keys and the `Installers` entries of a winget installer manifest. Winget
/// manifests are generated YAML with one fixed shape; plain `Key: value` lines are read, plus
/// one nested level (`InstallerSwitches.Silent`, `InstallModes` = `interactive,silent`).
fn winget_yaml(text: &str) -> (BTreeMap<String, String>, Vec<BTreeMap<String, String>>) {
    let (mut top, mut items) = (BTreeMap::new(), Vec::<BTreeMap<String, String>>::new());
    let (mut in_list, mut dash, mut keys) = (false, None, 0);
    let mut parent: Option<String> = None;
    for line in text.lines() {
        let body = line.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let indent = line.len() - body.len();
        if in_list {
            if let Some(rest) = body.strip_prefix("- ")
                && dash.is_none_or(|d| d == indent)
            {
                (dash, keys, parent) = (Some(indent), indent + 2, None);
                items.push(yaml_kv(rest).into_iter().collect());
                continue;
            }
            if dash.is_some()
                && indent >= keys
                && let Some(item) = items.last_mut()
            {
                if indent == keys && !body.starts_with("- ") {
                    match yaml_kv(body) {
                        Some((k, v)) if v.is_empty() => parent = Some(k),
                        Some((k, v)) => {
                            parent = None;
                            item.insert(k, v);
                        }
                        None => {}
                    }
                } else {
                    yaml_child(item, parent.as_deref(), body);
                }
                continue;
            }
            if indent > 0 {
                continue;
            }
            (in_list, parent) = (false, None);
        }
        if indent > 0 || body.starts_with("- ") {
            yaml_child(&mut top, parent.as_deref(), body);
            continue;
        }
        match yaml_kv(body) {
            Some((k, v)) if k == "Installers" && v.is_empty() => {
                (in_list, dash, parent) = (true, None, None);
            }
            Some((k, v)) if v.is_empty() => parent = Some(k),
            Some((k, v)) => {
                parent = None;
                top.insert(k, v);
            }
            None => {}
        }
    }
    (top, items)
}

/// Chooses the installer for `arch` from a winget installer manifest. Prefers the matching
/// architecture, then installers Simple Setup can run unattended, then per-user ones (no
/// administrator prompt), then English or locale-neutral ones. An `.exe` runs unattended with
/// the manifest's `Silent` switches, or with none when its only install mode is silent.
pub fn winget_installer(yaml: &str, arch: Arch) -> Result<Pick, String> {
    let (top, items) = winget_yaml(yaml);
    // 64-bit Windows runs 32-bit programs natively; ARM64 would need emulation, which is not
    // used on the user's behalf.
    let mut archs: Vec<&str> = arch.info().winget.iter().map(String::as_str).collect();
    archs.push("neutral");
    if arch == Arch::X86_64 {
        archs.push("x86");
    }
    let version = top.get("PackageVersion").cloned().unwrap_or_default();
    let mut best: Option<((usize, u8, u8, u8), Pick)> = None;
    for item in &items {
        let get = |k: &str| item.get(k).or_else(|| top.get(k)).map(String::as_str);
        let Some(ai) = get("Architecture").and_then(|a| archs.iter().position(|x| *x == a)) else {
            continue;
        };
        let kind = match get("InstallerType")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "msi" | "wix" => WinInstaller::Msi,
            "nullsoft" => WinInstaller::Nullsoft,
            "inno" => WinInstaller::Inno,
            "burn" => WinInstaller::Burn,
            "exe" => WinInstaller::Exe,
            "msix" | "appx" => WinInstaller::Msix,
            _ => continue, // zip, portable, fonts: not handled
        };
        let (Some(url), Some(sha)) = (
            get("InstallerUrl").filter(|u| https(u)),
            get("InstallerSha256").and_then(sha256_hex),
        ) else {
            continue;
        };
        let silent = if kind == WinInstaller::Exe {
            match get("InstallerSwitches.Silent").or(get("InstallerSwitches.SilentWithProgress")) {
                Some(s) => Some(s.split_whitespace().map(String::from).collect::<Vec<_>>())
                    .filter(|a| silent_args_ok(a)),
                None => (get("InstallModes") == Some("silent")).then(Vec::new),
            }
        } else {
            None
        };
        let manual = u8::from(match kind {
            WinInstaller::Exe => silent.is_none(),
            WinInstaller::Msix => true,
            _ => false,
        });
        let machine = u8::from(get("Scope") != Some("user"));
        let locale = u8::from(!matches!(get("InstallerLocale"), None | Some("en-US")));
        let rank = (ai, manual, machine, locale);
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            let scope = if machine == 0 { ", per-user" } else { "" };
            let how = match &silent {
                Some(a) if a.is_empty() => ", silent by itself".to_string(),
                Some(a) => format!(", silent with {}", a.join(" ")),
                None => String::new(),
            };
            best = Some((
                rank,
                Pick {
                    url: url.into(),
                    sha256: sha,
                    version: version.clone(),
                    win: Some(kind),
                    detail: format!("{} {kind:?} installer{scope}{how}", archs[ai]),
                    silent,
                    ..Pick::default()
                },
            ));
        }
    }
    best.map(|(_, p)| p).ok_or_else(|| match arch {
        Arch::X86_64 => "The winget catalog lists no suitable installer for it.".into(),
        a => format!(
            "The winget catalog lists no {} installer for it.",
            a.label()
        ),
    })
}

// ---------------------------------------------------------------- Homebrew casks

#[derive(Debug, Deserialize)]
struct CaskJson {
    version: String,
    url: Option<String>,
    sha256: Option<String>,
    #[serde(default)]
    artifacts: Vec<serde_json::Value>,
    #[serde(default)]
    variations: BTreeMap<String, Variation>,
    #[serde(default)]
    depends_on: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct Variation {
    url: Option<String>,
    sha256: Option<String>,
    version: Option<String>,
}

/// What a cask installs: the app bundle's name, or `None` for an installer package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaskPick {
    pub pick: Pick,
    pub app: Option<String>,
}

/// Chooses the download for `arch` from a Homebrew cask API answer. The top-level download is
/// the Apple-silicon one; Intel downloads, when different, are listed as variations named after
/// macOS releases (those pinning an older `version` are for old systems and are skipped).
pub fn cask_pick(json: &str, arch: Arch) -> Result<CaskPick, String> {
    let c: CaskJson =
        serde_json::from_str(json).map_err(|e| format!("Unexpected Homebrew answer: {e}"))?;
    let needs = c
        .depends_on
        .get("arch")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("type").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
        });
    let want = arch
        .info()
        .cask
        .as_deref()
        .ok_or_else(|| format!("Macs do not use {} processors.", arch.label()))?;
    if needs.as_ref().is_some_and(|n| !n.contains(&want)) {
        return Err(format!(
            "Homebrew lists it only for {} Macs.",
            needs.unwrap_or_default().join("/")
        ));
    }
    let intel = c
        .variations
        .iter()
        .find(|(k, v)| !k.starts_with("arm64_") && !k.ends_with("_linux") && v.version.is_none());
    let (url, sha) = match (arch, intel) {
        (Arch::X86_64, Some((_, v))) => (
            v.url.clone().or(c.url.clone()),
            v.sha256.clone().or(c.sha256.clone()),
        ),
        _ => (c.url.clone(), c.sha256.clone()),
    };
    let url = url
        .filter(|u| https(u))
        .ok_or("Homebrew lists no secure download for it.")?;
    // `no_check`: the vendor's link always serves the newest version, so Homebrew cannot pin
    // one. The file is then taken over HTTPS and its own SHA-256 is pinned in the snapshot.
    let sha = match sha.as_deref() {
        Some("no_check") => String::new(),
        s => s
            .and_then(sha256_hex)
            .ok_or("Homebrew lists no valid checksum for this download.")?,
    };
    let first = |key: &str| {
        c.artifacts
            .iter()
            .find_map(|a| a.get(key)?.as_array()?.first()?.as_str().map(String::from))
    };
    let app = first("app");
    if app.is_none() && first("pkg").is_none() {
        return Err("This cask does not install a plain app or installer package.".into());
    }
    let what = app.clone().unwrap_or_else(|| "installer package".into());
    Ok(CaskPick {
        pick: Pick {
            detail: if sha.is_empty() {
                format!("{what}; Homebrew publishes no checksum for it")
            } else {
                what
            },
            url,
            sha256: sha,
            version: c.version,
            ..Pick::default()
        },
        app,
    })
}

// ---------------------------------------------------------------- GitHub releases

#[derive(Debug, Default, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub html_url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    /// `sha256:<hex>`, published by GitHub for assets uploaded since mid-2025.
    pub digest: Option<String>,
    pub browser_download_url: String,
    /// The asset's API address, which private repositories need (with a token).
    #[serde(default)]
    pub url: Option<String>,
}

/// Lowercase name tokens; every architecture spelling (`amd64`, `x86_64`, `arm64`, `armhf`, …)
/// becomes that architecture's canonical name (see `registry/arches.json`).
fn tokens(name: &str) -> Vec<String> {
    // Spellings with separators would otherwise split into other words ("x86", "64").
    let n = name
        .to_ascii_lowercase()
        .replace("x86_64", "x64")
        .replace("x86-64", "x64");
    n.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| Arch::parse(t).map_or_else(|| t.to_string(), |a| a.to_string()))
        .collect()
}

/// Whether a token names an architecture, and whether it names `arch` (macOS release files say
/// "intel" for x86-64).
fn arch_word(t: &str, arch: Arch) -> (bool, bool) {
    if t == "intel" {
        return (true, arch == Arch::X86_64);
    }
    match Arch::parse(t) {
        Some(a) => (true, a == arch),
        None => (false, false),
    }
}

/// What a release asset is for, from its name: the operating system words in it.
fn names_os(lower: &str, os: OsFamily) -> bool {
    let words: &[&str] = match os {
        OsFamily::Linux => &["linux"],
        OsFamily::Windows => &["windows", "win", "win64", "win32", "msvc"],
        OsFamily::Macos => &["macos", "mac", "osx", "darwin", "apple"],
    };
    lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|w| words.contains(&w))
}

/// What kind of file a release asset is, from its name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AssetFormat {
    Deb,
    Rpm,
    AppImage,
    Msi,
    Msix,
    Exe,
    Dmg,
    Pkg,
    /// A macOS app bundle packed as `.app.tar.gz` (Tauri's updater format).
    AppTarball,
    Zip,
    Tarball,
    /// A bare program file (`jq-linux-amd64`).
    Program,
    /// Pacman (`.pkg.tar.zst`), Alpine (`.apk`), Flatpak or snap package files.
    OtherPackage,
    /// Checksums, signatures, update metadata, debug symbols, sources.
    #[default]
    Auxiliary,
}

/// A release asset read from its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetInfo {
    pub format: AssetFormat,
    /// The operating system it names or its format implies.
    pub os: Option<OsFamily>,
    /// Processors it names (none: it does not say).
    pub arches: Vec<Arch>,
    pub universal: bool,
    /// A distribution it is built for (`ubuntu`, `fedora`, `rhel`, `opensuse`…).
    pub distro: Option<&'static str>,
    pub gnu: bool,
    pub musl: bool,
    /// 32-bit ARM soft-float ABI ("gnueabi"); systems use hard float.
    pub soft_float: bool,
}

/// Reads what a release asset is from its name alone.
pub fn classify(name: &str) -> AssetInfo {
    let lower = name.to_ascii_lowercase();
    let t = tokens(name);
    let ends = |e: &[&str]| e.iter().any(|x| lower.ends_with(x));
    let aux_word = t.iter().any(|x| {
        matches!(
            x.as_str(),
            "sig"
                | "asc"
                | "sha256"
                | "sha256sum"
                | "sha512"
                | "sha256sums"
                | "checksums"
                | "debug"
                | "symbols"
                | "src"
                | "source"
                | "pdb"
                | "blockmap"
                | "dbg"
        )
    });
    let format = if aux_word
        || ends(&[
            ".yml", ".yaml", ".json", ".txt", ".md", ".sig", ".asc", ".pem", ".sbom",
        ]) {
        AssetFormat::Auxiliary
    } else if ends(&[".deb"]) {
        AssetFormat::Deb
    } else if ends(&[".rpm"]) {
        AssetFormat::Rpm
    } else if ends(&[".appimage"]) {
        AssetFormat::AppImage
    } else if ends(&[".msi"]) {
        AssetFormat::Msi
    } else if ends(&[".msix", ".msixbundle", ".appx", ".appxbundle"]) {
        AssetFormat::Msix
    } else if ends(&[".exe"]) {
        AssetFormat::Exe
    } else if ends(&[".dmg"]) {
        AssetFormat::Dmg
    } else if ends(&[".pkg"]) {
        AssetFormat::Pkg
    } else if ends(&[".app.tar.gz", ".app.tgz"])
        || (ends(&["-app.tar.gz"]) && names_os(&lower, OsFamily::Macos))
    {
        AssetFormat::AppTarball
    } else if ends(&[
        ".pkg.tar.zst",
        ".pkg.tar.xz",
        ".apk",
        ".flatpak",
        ".flatpakref",
        ".snap",
    ]) {
        AssetFormat::OtherPackage
    } else if ends(&[".zip"]) {
        AssetFormat::Zip
    } else if ends(&[
        ".tar.gz", ".tgz", ".tar.xz", ".txz", ".tar.zst", ".tar.bz2", ".tbz2",
    ]) {
        AssetFormat::Tarball
    } else if !lower
        .rsplit_once('.')
        .is_some_and(|(_, e)| e.chars().any(|c| c.is_ascii_alphabetic()) && e.len() <= 5)
    {
        // No file extension (a version such as "1.2" does not count as one).
        AssetFormat::Program
    } else {
        AssetFormat::Auxiliary
    };
    let os = match format {
        AssetFormat::Deb | AssetFormat::Rpm | AssetFormat::AppImage | AssetFormat::OtherPackage => {
            Some(OsFamily::Linux)
        }
        AssetFormat::Msi | AssetFormat::Msix | AssetFormat::Exe => Some(OsFamily::Windows),
        AssetFormat::Dmg | AssetFormat::Pkg | AssetFormat::AppTarball => Some(OsFamily::Macos),
        _ => [OsFamily::Linux, OsFamily::Windows, OsFamily::Macos]
            .into_iter()
            .find(|os| names_os(&lower, *os)),
    };
    let mut arches: Vec<Arch> = t
        .iter()
        .filter_map(|x| match x.as_str() {
            "intel" => Some(Arch::X86_64),
            x => Arch::parse(x),
        })
        .collect();
    arches.sort();
    arches.dedup();
    let distro = t.iter().find_map(|x| distro_word(x));
    AssetInfo {
        format,
        os,
        arches,
        universal: t.iter().any(|x| x == "universal" || x == "universal2"),
        distro,
        gnu: t.iter().any(|x| x == "gnu" || x.starts_with("gnueabi")),
        musl: t.iter().any(|x| x.starts_with("musl")),
        soft_float: t.iter().any(|x| x.ends_with("eabi")),
    }
}

/// The distribution profile a word in a file name stands for.
fn distro_word(t: &str) -> Option<&'static str> {
    let numbered = |p: &str| {
        t.strip_prefix(p)
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    };
    Some(match t {
        "ubuntu" => "ubuntu",
        "debian" => "debian",
        "fedora" => "fedora",
        "mint" | "linuxmint" => "linuxmint",
        "opensuse" | "suse" | "sles" | "tumbleweed" | "leap" => "opensuse",
        "rhel" | "centos" | "rocky" | "almalinux" => "rhel",
        "archlinux" => "arch",
        "alpine" => "alpine",
        _ if numbered("el") => "rhel",
        _ if numbered("fc") => "fedora",
        _ if numbered("ubuntu") => "ubuntu",
        _ if numbered("debian") => "debian",
        _ => return None,
    })
}

/// What a destination can use from a release.
#[derive(Debug, Clone)]
pub struct Want {
    pub target: Platform,
    /// Debian packages install there (the profile has the format and the system is writable).
    pub deb: bool,
    /// RPM packages install there.
    pub rpm: bool,
    /// AppImages run there.
    pub appimage: bool,
    /// Processors whose programs run there by emulation (used only when nothing native fits).
    pub emulated: Vec<Arch>,
    /// The distribution and the ones it derives from, closest first (`linuxmint`, `ubuntu`…).
    pub distros: Vec<String>,
    /// The C library is musl: programs built against GNU libc do not run.
    pub musl: bool,
    /// The user's file-name patterns for this target: only matching files are used.
    pub rules: Vec<String>,
}

impl Want {
    /// A destination about which only the platform is known.
    pub fn plain(target: Platform) -> Self {
        Self {
            target,
            deb: false,
            rpm: false,
            appimage: true,
            emulated: Vec::new(),
            distros: Vec::new(),
            musl: false,
            rules: Vec::new(),
        }
    }

    /// What `info` (the destination) can use, for `target` (its platform, or another one the
    /// snapshot also prepares for).
    pub fn for_system(info: &crate::probe::SystemInfo, target: Platform) -> Self {
        let same = info.device.platform == target;
        let profile = info.profile().filter(|_| same);
        Self {
            target,
            deb: same && crate::compat::wants_deb(info),
            rpm: same && crate::installers::wants_rpm(info),
            appimage: profile.is_none_or(|p| p.appimage != crate::distros::Avail::Unsupported),
            emulated: if same {
                info.emulates.clone()
            } else {
                Vec::new()
            },
            distros: profile.map(|p| p.chain.clone()).unwrap_or_default(),
            musl: same && info.libc.as_deref() == Some("musl"),
            rules: Vec::new(),
        }
    }

    /// Formats this destination installs, best first.
    fn formats(&self) -> Vec<AssetFormat> {
        use AssetFormat::*;
        match self.target.os {
            OsFamily::Linux => {
                let mut v = Vec::new();
                if self.deb {
                    v.push(Deb);
                }
                if self.rpm {
                    v.push(Rpm);
                }
                if self.appimage && !self.musl {
                    v.push(AppImage);
                }
                v.extend([Tarball, Program]);
                v
            }
            OsFamily::Windows => vec![Msi, Exe, Zip],
            OsFamily::Macos => vec![Dmg, Zip, Tarball, Pkg, Program],
        }
    }
}

/// Chooses the release asset for `want` (see [`pick_asset`]), for a destination about which
/// only the platform and its package formats are known.
pub fn github_asset(rel: &Release, target: Platform, deb: bool) -> Result<Pick, String> {
    pick_asset(
        rel,
        &Want {
            deb,
            ..Want::plain(target)
        },
    )
}

/// Chooses the release asset a destination can use, never one it cannot:
/// - its operating system: named in the name, or implied by the format (`.deb`, `.msi`, `.dmg`…);
///   archives and bare programs must name it;
/// - its processor, `universal`, or none named; failing that, a processor it runs by emulation
///   (Rosetta 2, Windows on ARM), said in the detail;
/// - a format it installs, in its order of preference (Linux: its own package format, then an
///   AppImage, an archive, a bare program; Windows: MSI, `.exe`, zip; macOS: disk image, zip,
///   archive, installer package);
/// - then a build for its own distribution (`ubuntu`, `el9`…), hard-float ARM, static (musl)
///   Linux builds, the shortest name.
///
/// On a musl system (Alpine) GNU builds and AppImages are left out. The user's patterns
/// (`want.rules`), when there are any, limit the choice to matching names. The SHA-256 comes
/// from GitHub's digest; a checksum list in the same release is also given (`sums_url`) so
/// the two can be compared. Without either, `sha256` is empty and the download's own checksum
/// is pinned when it is taken (which a restore does only with approval).
pub fn pick_asset(rel: &Release, want: &Want) -> Result<Pick, String> {
    let target = want.target;
    let formats = want.formats();
    let rules: Vec<globset::GlobMatcher> = want
        .rules
        .iter()
        .filter_map(|r| {
            globset::GlobBuilder::new(r)
                .case_insensitive(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        })
        .collect();
    // (processor fit, format, distribution, soft-float, GNU build, name length): lower is better.
    type Rank = (u8, usize, usize, u8, u8, usize);
    let mut best: Option<(Rank, &Asset, Option<Arch>)> = None;
    let mut refused = Vec::new();
    for a in &rel.assets {
        let i = classify(&a.name);
        if i.format == AssetFormat::Auxiliary {
            continue;
        }
        if !rules.is_empty() && !rules.iter().any(|g| g.is_match(&a.name)) {
            continue;
        }
        if i.os != Some(target.os) {
            continue;
        }
        let Some(format) = formats.iter().position(|f| *f == i.format) else {
            refused.push(format!("{}: not a format this system installs", a.name));
            continue;
        };
        let (fit, emulated) = if i.universal || i.arches.contains(&target.arch) {
            (0, None)
        } else if i.arches.is_empty() {
            (1, None)
        } else if let Some(e) = i.arches.iter().find(|a| want.emulated.contains(a)) {
            (2, Some(*e))
        } else {
            let names: Vec<&str> = i.arches.iter().map(|a| a.label()).collect();
            refused.push(format!("{}: for {}", a.name, names.join("/")));
            continue;
        };
        if want.musl && (i.gnu || i.format == AssetFormat::AppImage) {
            refused.push(format!(
                "{}: built for GNU libc, this system uses musl",
                a.name
            ));
            continue;
        }
        // The closest distribution in the chain first (Linux Mint: Ubuntu builds before
        // Debian ones), then builds naming none, then builds for others.
        let n = want.distros.len();
        let distro = match i.distro {
            Some(d) => want.distros.iter().position(|x| x == d).unwrap_or(n + 1),
            None => n,
        };
        let rank = (
            fit,
            format,
            distro,
            u8::from(i.soft_float),
            u8::from(i.gnu),
            a.name.len(),
        );
        if best.as_ref().is_none_or(|(r, _, _)| rank < *r) {
            best = Some((rank, a, emulated));
        }
    }
    let Some((_, a, emulated)) = best else {
        let mut why = format!("Release {} has no download for {target}", rel.tag_name);
        if !want.rules.is_empty() {
            why += &format!(" matching your pattern {}", want.rules.join(" or "));
        }
        if !refused.is_empty() {
            why += &format!(
                " ({}{})",
                refused
                    .iter()
                    .take(4)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; "),
                if refused.len() > 4 { "; …" } else { "" }
            );
        }
        return Err(why + ".");
    };
    let sha = a
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .and_then(sha256_hex);
    let sums_url = sums_asset(rel, &a.name, target).map(|s| s.browser_download_url.clone());
    if !https(&a.browser_download_url) || sums_url.as_deref().is_some_and(|u| !https(u)) {
        return Err(format!("{} has no secure download link.", a.name));
    }
    let format = classify(&a.name).format;
    let archive = matches!(format, AssetFormat::Zip | AssetFormat::Tarball);
    let win = match format {
        AssetFormat::Msi => Some(WinInstaller::Msi),
        AssetFormat::Exe => Some(WinInstaller::Exe),
        AssetFormat::Msix => Some(WinInstaller::Msix),
        _ => None,
    };
    let unchecked = sha.is_none() && sums_url.is_none();
    Ok(Pick {
        url: a.browser_download_url.clone(),
        api_url: a
            .url
            .clone()
            .filter(|u| u.starts_with(crate::fetch::GITHUB_API)),
        sha256: sha.unwrap_or_default(),
        version: rel.tag_name.clone(),
        win,
        detail: format!(
            "release asset {} ({} MB){}{}",
            a.name,
            a.size / 1_000_000,
            emulated
                .map(|e| format!(
                    "; built for {}, which this computer runs by emulation",
                    e.label()
                ))
                .unwrap_or_default(),
            if unchecked {
                "; the release publishes no checksum for it"
            } else {
                ""
            }
        ),
        name: a.name.clone(),
        sums_url,
        archive,
        silent: None,
        program: format == AssetFormat::Program,
    })
}

/// The SHA-256 to trust for a release file: GitHub's digest and the release's checksum list
/// must agree when both exist; `what` says which were used.
pub fn verified_sha(
    name: &str,
    digest: Option<&str>,
    list: Option<&str>,
) -> Result<(String, &'static str), String> {
    let digest = digest
        .and_then(|d| d.strip_prefix("sha256:").or(Some(d)))
        .and_then(sha256_hex);
    let listed = list.and_then(|l| parse_sums(l, name));
    match (digest, listed) {
        (Some(d), Some(l)) if d == l => Ok((d, "GitHub's digest and the release's checksum list")),
        (Some(_), Some(_)) => Err(format!(
            "GitHub's digest and the release's checksum list disagree about {name}; it is not used."
        )),
        (Some(d), None) => Ok((d, "GitHub's digest")),
        (None, Some(l)) => Ok((l, "the release's checksum list")),
        (None, None) => Err(format!("No checksum is published for {name}.")),
    }
}

/// A checksum file in the release covering `name`: `<name>.sha256`, or a combined list such
/// as `SHA256SUMS` or `checksums.txt`; of several lists (`SHA256SUMS.Linux.x64.txt`, …), the
/// one naming the target's system and processor.
fn sums_asset<'r>(rel: &'r Release, name: &str, target: Platform) -> Option<&'r Asset> {
    let own = [format!("{name}.sha256"), format!("{name}.sha256sum")];
    rel.assets
        .iter()
        .find(|a| own.iter().any(|o| a.name.eq_ignore_ascii_case(o)))
        .or_else(|| {
            rel.assets
                .iter()
                .filter(|a| {
                    let l = a.name.to_ascii_lowercase();
                    (l.contains("sha256") || l.contains("checksums"))
                        && !l.contains("sha512")
                        && (l.ends_with(".txt") || !l.contains('.'))
                })
                .min_by_key(|a| {
                    let l = a.name.to_ascii_lowercase();
                    let arch = tokens(&a.name).iter().any(|t| arch_word(t, target.arch).1);
                    (!names_os(&l, target.os), !arch)
                })
        })
}

/// The SHA-256 of `name` in a checksum list (`<hex>  <name>`, `<hex> *<name>`), or the only
/// hash in a single-file `.sha256`.
pub fn parse_sums(text: &str, name: &str) -> Option<String> {
    let mut lone = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next().and_then(sha256_hex) else {
            continue;
        };
        match parts.next() {
            Some(file) => {
                let file = file.trim_start_matches('*').trim_start_matches("./");
                if file == name || file.rsplit('/').next() == Some(name) {
                    return Some(hash);
                }
            }
            None => lone.push(hash),
        }
    }
    (lone.len() == 1).then(|| lone.remove(0))
}

/// The unattended-capable installer technology of a Windows `.exe`, from marks its builder
/// leaves in the file (the first few megabytes are enough).
pub fn sniff_win_installer(head: &[u8]) -> Option<WinInstaller> {
    let has = |m: &[u8]| head.windows(m.len()).any(|w| w == m);
    if has(b"NullsoftInst") || has(b"Nullsoft Install System") {
        Some(WinInstaller::Nullsoft)
    } else if has(b"Inno Setup Setup Data") || has(b"InnoSetupLdrWindow") {
        Some(WinInstaller::Inno)
    } else if has(b".wixburn") {
        Some(WinInstaller::Burn)
    } else {
        None
    }
}

/// From the end of a zip file: (entries, offset and size of the central directory). ZIP64
/// archives are not read.
pub fn zip_eocd(tail: &[u8]) -> Option<(usize, u64, usize)> {
    let at = (0..=tail.len().checked_sub(22)?)
        .rev()
        .find(|&i| tail[i..].starts_with(b"PK\x05\x06"))?;
    let u16_at = |i: usize| Some(u16::from_le_bytes(tail.get(i..i + 2)?.try_into().ok()?) as usize);
    let u32_at = |i: usize| Some(u32::from_le_bytes(tail.get(i..i + 4)?.try_into().ok()?));
    let (count, size, offset) = (u16_at(at + 10)?, u32_at(at + 12)?, u32_at(at + 16)?);
    (offset != u32::MAX).then_some((count, u64::from(offset), size as usize))
}

/// Entry names in a zip central directory.
pub fn zip_central_names(cd: &[u8], count: usize) -> Option<Vec<String>> {
    Some(
        zip_central_entries(cd, count)?
            .into_iter()
            .map(|(n, _)| n)
            .collect(),
    )
}

/// Entry names and Unix modes (0 when the zip was not made on Unix) in a zip central directory.
pub fn zip_central_entries(cd: &[u8], count: usize) -> Option<Vec<(String, u32)>> {
    let u16_at = |i: usize| Some(u16::from_le_bytes(cd.get(i..i + 2)?.try_into().ok()?) as usize);
    let u32_at = |i: usize| Some(u32::from_le_bytes(cd.get(i..i + 4)?.try_into().ok()?));
    let (mut at, mut out) = (0, Vec::with_capacity(count.min(100_000)));
    for _ in 0..count {
        if !cd.get(at..)?.starts_with(b"PK\x01\x02") {
            return None;
        }
        let (n, x, c) = (u16_at(at + 28)?, u16_at(at + 30)?, u16_at(at + 32)?);
        // "Version made by" high byte 3 = Unix: the external attributes then hold st_mode.
        let unix = cd.get(at + 5) == Some(&3);
        let mode = if unix { u32_at(at + 38)? >> 16 } else { 0 };
        out.push((
            String::from_utf8_lossy(cd.get(at + 46..at + 46 + n)?).into_owned(),
            mode,
        ));
        at += 46 + n + x + c;
    }
    Some(out)
}

/// Why a zip entry would be unsafe to unpack: outside the folder, or a link.
pub fn zip_entry_problem(name: &str, mode: u32) -> Option<&'static str> {
    if mode & 0o170000 == 0o120000 {
        return Some("is a link");
    }
    let bad = name.starts_with(['/', '\\'])
        || name.contains('\\')
        || name.as_bytes().get(1) == Some(&b':')
        || name.split('/').any(|c| c == "..");
    bad.then_some("would be written outside the folder")
}

/// Entry names of a whole zip file held in memory.
pub fn zip_names(data: &[u8]) -> Option<Vec<String>> {
    let (count, offset, size) = zip_eocd(data)?;
    let start = usize::try_from(offset).ok()?;
    zip_central_names(data.get(start..start.checked_add(size)?)?, count)
}

/// The app bundle at the top of a zip listing (`Foo.app/…`), if there is exactly one.
pub fn zip_app(names: &[String]) -> Option<String> {
    let mut apps: Vec<&str> = names
        .iter()
        .filter_map(|n| n.split('/').next())
        .filter(|n| n.ends_with(".app") && !n.starts_with('.'))
        .collect();
    apps.dedup();
    (apps.len() == 1).then(|| apps[0].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real catalog answers, embedded so the tests also run where the sources are not present.
    fn fixture(name: &str) -> String {
        match name {
            "winget-firefox-dirs.json" => {
                include_str!("../tests/fixtures/catalog/winget-firefox-dirs.json")
            }
            "winget-firefox.installer.yaml" => {
                include_str!("../tests/fixtures/catalog/winget-firefox.installer.yaml")
            }
            "cask-firefox.json" => include_str!("../tests/fixtures/catalog/cask-firefox.json"),
            "cask-vlc.json" => include_str!("../tests/fixtures/catalog/cask-vlc.json"),
            "cask-visual-studio-code.json" => {
                include_str!("../tests/fixtures/catalog/cask-visual-studio-code.json")
            }
            "github-simple-cell.json" => {
                include_str!("../tests/fixtures/catalog/github-simple-cell.json")
            }
            "github-simplecad.json" => {
                include_str!("../tests/fixtures/catalog/github-simplecad.json")
            }
            "github-auto-manager.json" => {
                include_str!("../tests/fixtures/catalog/github-auto-manager.json")
            }
            "github-simple-setup.json" => {
                include_str!("../tests/fixtures/catalog/github-simple-setup.json")
            }
            "github-rustdesk.json" => {
                include_str!("../tests/fixtures/catalog/github-rustdesk.json")
            }
            "github-ripgrep.json" => {
                include_str!("../tests/fixtures/catalog/github-ripgrep.json")
            }
            "github-obsidian.json" => {
                include_str!("../tests/fixtures/catalog/github-obsidian.json")
            }
            "winget-brave.installer.yaml" => {
                include_str!("../tests/fixtures/catalog/winget-brave.installer.yaml")
            }
            "winget-spotify.installer.yaml" => {
                include_str!("../tests/fixtures/catalog/winget-spotify.installer.yaml")
            }
            "cask-spotify.json" => include_str!("../tests/fixtures/catalog/cask-spotify.json"),
            _ => unreachable!("{name}"),
        }
        .to_string()
    }

    #[test]
    fn winget_manifests() {
        assert_eq!(
            winget_dir("Mozilla.Firefox").as_deref(),
            Some("manifests/m/Mozilla/Firefox")
        );
        assert_eq!(
            winget_dir("OpenJS.NodeJS.LTS").as_deref(),
            Some("manifests/o/OpenJS/NodeJS/LTS")
        );
        assert_eq!(winget_dir("../x"), None);
        assert_eq!(winget_dir("NoDot"), None);

        let dirs: Vec<DirEntry> =
            serde_json::from_str(&fixture("winget-firefox-dirs.json")).unwrap();
        let v = winget_versions(&dirs);
        assert_eq!(v[0], "157.0.1");
        assert!(
            v.iter()
                .all(|x| x.starts_with(|c: char| c.is_ascii_digit())),
            "locale folders skipped"
        );
        assert_eq!(version_cmp("100.0.1", "99.0"), Ordering::Greater);

        let y = fixture("winget-firefox.installer.yaml");
        let x64 = winget_installer(&y, Arch::X86_64).unwrap();
        assert!(x64.url.contains("/win64/"), "{x64:?}");
        assert_eq!(
            x64.sha256,
            "71dc620e7749bb65be6df67b99261a303805a804f9c787b76b81c095cfa7d9ca"
        );
        assert_eq!(
            (x64.win, x64.version.as_str()),
            (Some(WinInstaller::Nullsoft), "157.0.1")
        );
        assert!(
            winget_installer(&y, Arch::Aarch64)
                .unwrap()
                .url
                .contains("win64-aarch64")
        );

        // Per-entry types and scopes override the defaults; nested lists are not entries.
        let y = "PackageVersion: 2.1\nInstallerType: exe\nInstallers:\n\
            - Architecture: x64\n  InstallerUrl: https://e/x64.exe\n  InstallerSha256: 'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'\n  AppsAndFeaturesEntries:\n  - DisplayName: Tool\n\
            - Architecture: x64\n  InstallerType: wix\n  Scope: user\n  InstallerUrl: https://e/user.msi\n  InstallerSha256: BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\n\
            - Architecture: x86\n  InstallerType: msi\n  InstallerUrl: http://insecure/x86.msi\n  InstallerSha256: CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC\n\
            ManifestType: installer\n";
        let p = winget_installer(y, Arch::X86_64).unwrap();
        assert_eq!(
            (p.url.as_str(), p.win),
            ("https://e/user.msi", Some(WinInstaller::Msi))
        );
        assert!(
            winget_installer(y, Arch::Aarch64).is_err(),
            "no emulated x64 on ARM"
        );
    }

    #[test]
    fn winget_exe_silent_switches() {
        // Brave: per-user installers that are silent by themselves win over the machine-wide
        // ones (which need /silent /install and an administrator).
        let y = fixture("winget-brave.installer.yaml");
        let x64 = winget_installer(&y, Arch::X86_64).unwrap();
        assert!(
            x64.url.ends_with("/BraveBrowserStandaloneSilentSetup.exe"),
            "{x64:?}"
        );
        assert_eq!(
            (x64.win, x64.silent),
            (Some(WinInstaller::Exe), Some(vec![]))
        );
        let arm = winget_installer(&y, Arch::Aarch64).unwrap();
        assert!(arm.url.ends_with("SilentSetupArm64.exe"), "{arm:?}");

        // Spotify: switches from the manifest's top level.
        let y = fixture("winget-spotify.installer.yaml");
        let p = winget_installer(&y, Arch::X86_64).unwrap();
        assert!(p.url.ends_with("SpotifyFullSetupX64.exe"));
        assert_eq!(
            p.silent,
            Some(vec!["/silent".to_string(), "/skip-app-launch".to_string()])
        );

        // Switches with placeholders or quotes are not used; an interactive exe stays manual.
        let one = |extra: &str| {
            format!(
                "PackageVersion: 1\nInstallerType: exe\n{extra}Installers:\n- Architecture: x64\n  InstallerUrl: https://e/x.exe\n  InstallerSha256: {}\n",
                "A".repeat(64)
            )
        };
        let silent = |extra: &str| winget_installer(&one(extra), Arch::X86_64).unwrap().silent;
        assert_eq!(silent(""), None);
        assert_eq!(silent("InstallModes:\n- interactive\n- silent\n"), None);
        assert_eq!(
            silent("InstallerSwitches:\n  Silent: /S /LOG=<LOGPATH>\n"),
            None
        );
        assert_eq!(
            silent("InstallerSwitches:\n  Silent: '/D=\"C:\\x\"'\n"),
            None
        );
        assert_eq!(
            silent("InstallerSwitches:\n  SilentWithProgress: --silent\n"),
            Some(vec!["--silent".into()])
        );
        // Nested values of other keys do not leak into the ones read.
        let (top, items) = winget_yaml(&fixture("winget-brave.installer.yaml"));
        assert_eq!(top.get("InstallModes"), None);
        assert_eq!(items[1]["InstallModes"], "interactive,silent");
        assert_eq!(items[1]["InstallerSwitches.Silent"], "/silent /install");
    }

    #[test]
    fn homebrew_casks() {
        let ff = cask_pick(&fixture("cask-firefox.json"), Arch::X86_64).unwrap();
        assert_eq!(ff.app.as_deref(), Some("Firefox.app"));
        assert_eq!(
            ff.pick,
            cask_pick(&fixture("cask-firefox.json"), Arch::Aarch64)
                .unwrap()
                .pick,
            "universal"
        );

        let arm = cask_pick(&fixture("cask-vlc.json"), Arch::Aarch64).unwrap();
        let intel = cask_pick(&fixture("cask-vlc.json"), Arch::X86_64).unwrap();
        assert!(arm.pick.url.ends_with("arm64.dmg") && intel.pick.url.ends_with("intel64.dmg"));
        assert_eq!(
            intel.pick.sha256,
            "1ef6c903e2dc026d4e58ddf7b2a9ed0eb6f8caf80a9a9f46f96490340b962707"
        );

        // VS Code's Intel download for current systems, not the one pinned for Big Sur.
        let code = cask_pick(&fixture("cask-visual-studio-code.json"), Arch::X86_64).unwrap();
        assert!(code.pick.url.contains("1.144.0/darwin/"), "{:?}", code.pick);

        // Spotify's links always serve the newest version, so Homebrew pins no checksum: the
        // download is taken anyway (its own SHA-256 is pinned when it is collected).
        let arm = cask_pick(&fixture("cask-spotify.json"), Arch::Aarch64).unwrap();
        let intel = cask_pick(&fixture("cask-spotify.json"), Arch::X86_64).unwrap();
        assert_eq!(arm.app.as_deref(), Some("Spotify.app"));
        assert!(
            arm.pick.url.ends_with("SpotifyARM64.dmg") && intel.pick.url.ends_with("/Spotify.dmg")
        );
        assert!(arm.pick.sha256.is_empty() && intel.pick.sha256.is_empty());
        assert!(arm.pick.detail.contains("no checksum"));
        let bad = r#"{"version":"1","url":"https://x/a.dmg","sha256":"zzz","artifacts":[{"app":["A.app"]}]}"#;
        assert!(cask_pick(bad, Arch::Aarch64).is_err());
        let arm_only = r#"{"version":"1","url":"https://x/a.dmg","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","artifacts":[{"app":["A.app"]}],"depends_on":{"arch":[{"type":"arm","bits":64}]}}"#;
        assert!(cask_pick(arm_only, Arch::X86_64).is_err());
        assert!(cask_pick(arm_only, Arch::Aarch64).is_ok());
    }

    #[test]
    fn github_release_assets() {
        let rel: Release = serde_json::from_str(&fixture("github-rustdesk.json")).unwrap();
        let p = |os, arch| {
            github_asset(&rel, Platform { os, arch }, false)
                .map(|p| p.url.rsplit('/').next().unwrap().to_string())
        };
        assert_eq!(
            p(OsFamily::Linux, Arch::X86_64).unwrap(),
            "rustdesk-1.5.0-x86_64.AppImage"
        );
        assert_eq!(
            p(OsFamily::Linux, Arch::Aarch64).unwrap(),
            "rustdesk-1.5.0-aarch64.AppImage"
        );
        assert_eq!(
            p(OsFamily::Windows, Arch::X86_64).unwrap(),
            "rustdesk-1.5.0-x86_64.msi"
        );
        assert_eq!(
            p(OsFamily::Macos, Arch::Aarch64).unwrap(),
            "rustdesk-1.5.0-aarch64.dmg"
        );
        let msi = github_asset(
            &rel,
            Platform {
                os: OsFamily::Windows,
                arch: Arch::X86_64,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            (msi.win, msi.version.as_str()),
            (Some(WinInstaller::Msi), "1.5.0")
        );

        // Neither a digest nor a checksum list: taken without a published checksum.
        let unsigned: Release = serde_json::from_str(r#"{"tag_name":"v1","assets":[{"name":"t-arm64.AppImage","size":1,"digest":null,"browser_download_url":"https://g/t-arm64.AppImage"},{"name":"t-x86_64.AppImage","size":1,"digest":null,"browser_download_url":"https://g/t"},{"name":"t-armv7l.AppImage","size":1,"digest":null,"browser_download_url":"https://g/t7"},{"name":"T.pkg","size":1,"digest":null,"browser_download_url":"https://g/T.pkg"}]}"#).unwrap();
        let p = github_asset(
            &unsigned,
            Platform {
                os: OsFamily::Linux,
                arch: Arch::X86_64,
            },
            false,
        )
        .unwrap();
        assert!(p.sha256.is_empty() && p.sums_url.is_none() && p.detail.contains("no checksum"));
        assert_eq!(
            asset_name(&unsigned, OsFamily::Linux, Arch::Aarch64).unwrap(),
            "t-arm64.AppImage"
        );
        assert_eq!(
            asset_name(&unsigned, OsFamily::Macos, Arch::Aarch64).unwrap(),
            "T.pkg"
        );

        // balenaEtcher splits its checksums per platform: the Linux x64 list is the one read.
        let split: Release = serde_json::from_str(r#"{"tag_name":"v2","assets":[{"name":"SHA256SUMS.Darwin.arm64.txt","size":1,"digest":null,"browser_download_url":"https://g/mac"},{"name":"SHA256SUMS.Linux.x64.txt","size":1,"digest":null,"browser_download_url":"https://g/linux"},{"name":"e-x64.AppImage","size":1,"digest":null,"browser_download_url":"https://g/e"}]}"#).unwrap();
        let p = github_asset(
            &split,
            Platform {
                os: OsFamily::Linux,
                arch: Arch::X86_64,
            },
            false,
        )
        .unwrap();
        assert_eq!(p.sums_url.as_deref(), Some("https://g/linux"));
        assert!(
            github_asset(
                &unsigned,
                Platform {
                    os: OsFamily::Windows,
                    arch: Arch::X86_64
                },
                false
            )
            .is_err()
        );
    }

    /// Architectures beyond x86-64 and ARM64 pick their own files and never another's.
    #[test]
    fn more_architectures() {
        use Arch::*;
        use OsFamily::*;
        let rg: Release = serde_json::from_str(&fixture("github-ripgrep.json")).unwrap();
        let v = &rg.tag_name;
        assert_eq!(
            asset_name(&rg, Linux, Armv7).unwrap(),
            format!("ripgrep-{v}-armv7-unknown-linux-musleabihf.tar.gz")
        );
        assert_eq!(
            asset_name(&rg, Windows, I686).unwrap(),
            format!("ripgrep-{v}-i686-pc-windows-msvc.zip")
        );
        assert_eq!(
            asset_name(&rg, Linux, S390x).unwrap(),
            format!("ripgrep-{v}-s390x-unknown-linux-gnu.tar.gz")
        );
        assert!(asset_name(&rg, Linux, Riscv64).is_err());
        // Electron names its Windows builds "win32-<arch>": that is the system, not x86.
        let rel = Release {
            tag_name: "1.0".into(),
            assets: ["App-win32-x64.zip", "App-win32-arm64.zip"]
                .iter()
                .map(|n| Asset {
                    name: (*n).into(),
                    size: 1,
                    digest: Some(format!("sha256:{}", "a".repeat(64))),
                    browser_download_url: format!("https://example.org/{n}"),
                    ..Asset::default()
                })
                .collect(),
            ..Release::default()
        };
        assert_eq!(
            asset_name(&rel, Windows, X86_64).unwrap(),
            "App-win32-x64.zip"
        );
        assert!(asset_name(&rel, Windows, I686).is_err());
    }

    /// The spec's migrations, on real releases of the Simple Suite (fixtures saved from
    /// GitHub): the destination's own file, never the source's.
    #[test]
    fn simple_suite_release_scenarios() {
        use Arch::*;
        use OsFamily::*;
        let rel = |n: &str| -> Release {
            serde_json::from_str(&fixture(&format!("github-{n}.json"))).unwrap()
        };
        let (cell, cad, am, setup) = (
            rel("simple-cell"),
            rel("simplecad"),
            rel("auto-manager"),
            rel("simple-setup"),
        );
        let p = |os, arch| Platform { os, arch };
        let mint = Want {
            deb: true,
            distros: vec![
                "linuxmint".into(),
                "ubuntu".into(),
                "debian".into(),
                "linux".into(),
            ],
            ..Want::plain(p(Linux, X86_64))
        };
        let pick = |r: &Release, w: &Want| pick_asset(r, w).map(|p| p.name);
        // Fedora x64 → Linux Mint x64: a .deb, never the source's .rpm.
        assert_eq!(pick(&cell, &mint).unwrap(), "simple-cell_0.1.0_amd64.deb");
        assert_eq!(
            pick(&cad, &mint).unwrap(),
            "SimpleCAD-0.3.0-linux-x86_64.deb"
        );
        assert_eq!(
            pick(&am, &mint).unwrap(),
            "linux-x86_64-Auto.Manager_0.2.0_amd64.deb"
        );
        assert_eq!(pick(&setup, &mint).unwrap(), "Simple.Setup_0.4.1_amd64.deb");
        // Windows x64 → macOS on Apple Silicon: the ARM64 disk image.
        let mac_arm = Want {
            emulated: vec![X86_64],
            ..Want::plain(p(Macos, Aarch64))
        };
        assert_eq!(
            pick(&cell, &mac_arm).unwrap(),
            "Simple-Cell-0.1.0-arm64.dmg"
        );
        assert_eq!(
            pick(&cad, &mac_arm).unwrap(),
            "SimpleCAD-0.3.0-macos-arm64.dmg"
        );
        assert_eq!(
            pick(&am, &mac_arm).unwrap(),
            "darwin-aarch64-Auto.Manager_0.2.0_aarch64.dmg"
        );
        assert_eq!(
            pick(&setup, &mac_arm).unwrap(),
            "Simple.Setup_0.4.1_aarch64.dmg"
        );
        // Ubuntu ARM64 → Fedora ARM64: an ARM64 RPM; an x86-64 one never.
        let fedora_arm = Want {
            rpm: true,
            distros: vec!["fedora".into(), "linux".into()],
            ..Want::plain(p(Linux, Aarch64))
        };
        assert_eq!(
            pick(&am, &fedora_arm).unwrap(),
            "linux-aarch64-Auto.Manager-0.2.0-1.aarch64.rpm"
        );
        assert_eq!(
            pick(&setup, &fedora_arm).unwrap(),
            "Simple.Setup-0.4.1-1.aarch64.rpm"
        );
        assert_ne!(
            pick(&cell, &fedora_arm).unwrap(),
            "simple-cell-0.1.0.x86_64.rpm"
        );
        // Windows ARM64 → Windows x64: the x64 installer.
        let win = Want::plain(p(Windows, X86_64));
        assert_eq!(
            pick(&am, &win).unwrap(),
            "windows-x86_64-Auto.Manager_0.2.0_x64-setup.exe"
        );
        assert_eq!(
            pick(&setup, &win).unwrap(),
            "Simple.Setup_0.4.1_x64_en-US.msi"
        );
        // Intel Mac → Windows x64.
        assert_eq!(
            pick(&cad, &win).unwrap(),
            "SimpleCAD-0.3.0-windows-x86_64-setup.exe"
        );
        assert_eq!(pick(&cell, &win).unwrap(), "Simple-Cell-Setup-0.1.0.exe");
        // Linux x64 → Linux ARM64 (any distribution): the desktop app, not the CLI next to it.
        let linux_arm = Want::plain(p(Linux, Aarch64));
        assert_eq!(
            pick(&setup, &linux_arm).unwrap(),
            "Simple.Setup_0.4.1_aarch64.AppImage"
        );
        // Windows on ARM runs x64 programs: used when nothing is built for ARM64, and said.
        let win_arm = Want {
            emulated: vec![X86_64, I686],
            ..Want::plain(p(Windows, Aarch64))
        };
        let e = pick_asset(&cad, &win_arm).unwrap();
        assert_eq!(e.name, "SimpleCAD-0.3.0-windows-x86_64-setup.exe");
        assert!(e.detail.contains("emulation"), "{}", e.detail);
        assert_eq!(
            pick(&am, &win_arm).unwrap(),
            "windows-aarch64-Auto.Manager_0.2.0_arm64-setup.exe"
        );
        let no_emulation = pick(&cad, &Want::plain(p(Windows, Aarch64))).unwrap_err();
        assert!(no_emulation.contains("x86-64"), "{no_emulation}");
        // The user's pattern wins over the automatic choice.
        let rule = Want {
            rules: vec!["*.AppImage".into()],
            ..mint.clone()
        };
        assert_eq!(pick(&cell, &rule).unwrap(), "Simple-Cell-0.1.0.AppImage");
        let none = Want {
            rules: vec!["*.flatpak".into()],
            ..mint.clone()
        };
        assert!(pick(&cell, &none).unwrap_err().contains("*.flatpak"));
        // Update metadata, block maps and signatures are never chosen.
        for a in &am.assets {
            if a.name.ends_with(".sig") || a.name.ends_with(".json") {
                assert_eq!(
                    classify(&a.name).format,
                    AssetFormat::Auxiliary,
                    "{}",
                    a.name
                );
            }
        }
        assert_eq!(classify("latest-linux.yml").format, AssetFormat::Auxiliary);
        assert_eq!(
            classify("Simple-Cell-0.1.0.dmg.blockmap").format,
            AssetFormat::Auxiliary
        );
        assert_eq!(classify("jq-linux-amd64").format, AssetFormat::Program);
        assert_eq!(
            classify("simple-setup-0.4.1-linux-x86_64").format,
            AssetFormat::Program
        );
    }

    /// Builds for a named distribution go to it; musl systems get static builds only.
    #[test]
    fn distribution_and_libc_hints() {
        let rel = |names: &[&str]| Release {
            tag_name: "1".into(),
            assets: names
                .iter()
                .map(|n| Asset {
                    name: (*n).into(),
                    size: 1,
                    digest: Some(format!("sha256:{}", "a".repeat(64))),
                    browser_download_url: format!("https://example.org/{n}"),
                    ..Asset::default()
                })
                .collect(),
            ..Release::default()
        };
        let linux = Platform {
            os: OsFamily::Linux,
            arch: Arch::X86_64,
        };
        let debs = rel(&[
            "app_1.0_debian12_amd64.deb",
            "app_1.0_ubuntu24.04_amd64.deb",
        ]);
        let on = |chain: &[&str]| Want {
            deb: true,
            distros: chain.iter().map(|s| s.to_string()).collect(),
            ..Want::plain(linux)
        };
        assert_eq!(
            pick_asset(&debs, &on(&["linuxmint", "ubuntu", "debian", "linux"]))
                .unwrap()
                .name,
            "app_1.0_ubuntu24.04_amd64.deb"
        );
        assert_eq!(
            pick_asset(&debs, &on(&["debian", "linux"])).unwrap().name,
            "app_1.0_debian12_amd64.deb"
        );
        let rpms = rel(&["app-1.0.el9.x86_64.rpm", "app-1.0.fc44.x86_64.rpm"]);
        let fedora = Want {
            rpm: true,
            distros: vec!["fedora".into(), "linux".into()],
            ..Want::plain(linux)
        };
        assert_eq!(
            pick_asset(&rpms, &fedora).unwrap().name,
            "app-1.0.fc44.x86_64.rpm"
        );
        let tools = rel(&[
            "tool-x86_64-unknown-linux-gnu.tar.gz",
            "tool-x86_64-unknown-linux-musl.tar.gz",
            "Tool-x86_64.AppImage",
        ]);
        let alpine = Want {
            musl: true,
            ..Want::plain(linux)
        };
        assert_eq!(
            pick_asset(&tools, &alpine).unwrap().name,
            "tool-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(
            pick_asset(&tools, &Want::plain(linux)).unwrap().name,
            "Tool-x86_64.AppImage"
        );
        let gnu_only = rel(&["tool-x86_64-unknown-linux-gnu.tar.gz"]);
        assert!(pick_asset(&gnu_only, &alpine).unwrap_err().contains("musl"));
    }

    fn asset_name(rel: &Release, os: OsFamily, arch: Arch) -> Result<String, String> {
        github_asset(rel, Platform { os, arch }, false).map(|p| p.name)
    }

    #[test]
    fn github_archives_and_installers() {
        use Arch::*;
        use OsFamily::*;
        let rg: Release = serde_json::from_str(&fixture("github-ripgrep.json")).unwrap();
        let v = &rg.tag_name;
        for (os, arch, want) in [
            (
                Linux,
                X86_64,
                format!("ripgrep-{v}-x86_64-unknown-linux-musl.tar.gz"),
            ),
            (
                Linux,
                Aarch64,
                format!("ripgrep-{v}-aarch64-unknown-linux-musl.tar.gz"),
            ),
            (
                Windows,
                X86_64,
                format!("ripgrep-{v}-x86_64-pc-windows-msvc.zip"),
            ),
            (
                Windows,
                Aarch64,
                format!("ripgrep-{v}-aarch64-pc-windows-msvc.zip"),
            ),
            (
                Macos,
                Aarch64,
                format!("ripgrep-{v}-aarch64-apple-darwin.tar.gz"),
            ),
            (
                Macos,
                X86_64,
                format!("ripgrep-{v}-x86_64-apple-darwin.tar.gz"),
            ),
        ] {
            assert_eq!(asset_name(&rg, os, arch).unwrap(), want, "{os} {arch}");
        }
        let p = github_asset(
            &rg,
            Platform {
                os: Linux,
                arch: X86_64,
            },
            false,
        )
        .unwrap();
        // The digest is used, and the asset's own .sha256 file is offered for comparison.
        assert!(p.archive && p.sha256.len() == 64);
        assert!(
            p.sums_url
                .as_deref()
                .is_some_and(|u| u.ends_with(".tar.gz.sha256"))
        );

        // Older uploads have no digest: the checksum file next to the asset is used.
        let mut old = rg;
        for a in &mut old.assets {
            a.digest = None;
        }
        let p = github_asset(
            &old,
            Platform {
                os: Linux,
                arch: X86_64,
            },
            false,
        )
        .unwrap();
        assert_eq!(p.sha256, "");
        assert!(p.sums_url.unwrap().ends_with(&format!("{}.sha256", p.name)));

        let ob: Release = serde_json::from_str(&fixture("github-obsidian.json")).unwrap();
        let v = ob.tag_name.trim_start_matches('v');
        assert_eq!(
            asset_name(&ob, Linux, X86_64).unwrap(),
            format!("Obsidian-{v}.AppImage")
        );
        assert_eq!(
            asset_name(&ob, Linux, Aarch64).unwrap(),
            format!("Obsidian-{v}-arm64.AppImage")
        );
        assert_eq!(
            asset_name(&ob, Macos, Aarch64).unwrap(),
            format!("Obsidian-{v}.dmg")
        );
        // Debian-family targets take the release's .deb when it has one for the processor.
        let deb = |rel: &Release, arch| {
            github_asset(rel, Platform { os: Linux, arch }, true)
                .unwrap()
                .name
        };
        assert_eq!(deb(&ob, X86_64), format!("obsidian_{v}_amd64.deb"));
        assert_eq!(deb(&ob, Aarch64), format!("Obsidian-{v}-arm64.AppImage"));
        let rd: Release = serde_json::from_str(&fixture("github-rustdesk.json")).unwrap();
        assert_eq!(deb(&rd, X86_64), "rustdesk-1.5.0-x86_64.deb");
        assert_eq!(deb(&rd, Aarch64), "rustdesk-1.5.0-aarch64.deb");
        let exe = github_asset(
            &ob,
            Platform {
                os: Windows,
                arch: X86_64,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            (exe.name.as_str(), exe.win),
            (
                format!("Obsidian-{v}.exe").as_str(),
                Some(WinInstaller::Exe)
            )
        );
    }

    #[test]
    fn checksum_lists() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let list = format!("{a}  x.tar.gz\n{b} *dist/y.zip\n");
        assert_eq!(parse_sums(&list, "x.tar.gz"), Some(a.clone()));
        assert_eq!(parse_sums(&list, "y.zip"), Some(b.to_lowercase()));
        assert_eq!(parse_sums(&list, "z"), None);
        assert_eq!(parse_sums(&format!("{a}\n"), "anything"), Some(a));
        assert_eq!(parse_sums("not a hash  x\n", "x"), None);
    }

    #[test]
    fn windows_installer_marks() {
        let with = |m: &[u8]| [&[0u8; 300][..], m, &[1u8; 50][..]].concat();
        assert_eq!(
            sniff_win_installer(&with(b"\xef\xbe\xad\xdeNullsoftInst")),
            Some(WinInstaller::Nullsoft)
        );
        assert_eq!(
            sniff_win_installer(&with(b"Inno Setup Setup Data (6.2.0)")),
            Some(WinInstaller::Inno)
        );
        assert_eq!(
            sniff_win_installer(&with(b".wixburn")),
            Some(WinInstaller::Burn)
        );
        assert_eq!(sniff_win_installer(&with(b"MZ plain program")), None);
    }

    #[test]
    fn app_inside_zip() {
        let names = zip_names(include_bytes!("../tests/fixtures/catalog/app.zip")).unwrap();
        assert_eq!(names.len(), 3);
        assert_eq!(zip_app(&names).as_deref(), Some("Foo.app"));
        assert_eq!(zip_names(b"not a zip"), None);
        assert_eq!(zip_app(&["a.app/x".into(), "b.app/y".into()]), None);
    }
}
