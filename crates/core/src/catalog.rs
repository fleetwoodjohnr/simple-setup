//! Vetted installer catalogs: winget's community manifests (Windows), Homebrew's cask API
//! (macOS) and GitHub releases. Consulted while a snapshot is built, and during a restore for
//! apps the snapshot holds no installer for. This module only reads catalog answers and chooses
//! an installer; `fetch` downloads it, and the file must match the SHA-256 the catalog publishes
//! before it is used.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use serde::Deserialize;

use crate::manifest::WinInstaller;
use crate::platform::{Arch, OsFamily, Platform};

/// An installer chosen from a catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub url: String,
    /// Lowercase hex.
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
}

fn sha256_hex(s: &str) -> Option<String> {
    let s = s.trim();
    (s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())).then(|| s.to_ascii_lowercase())
}

fn https(url: &str) -> bool {
    url.starts_with("https://") && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Compares version strings part by part, numbers numerically ("10.1" > "9.12").
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    let parts = |s: &str| -> Vec<(u64, String)> {
        s.split(['.', '-', '_', '+'])
            .map(|p| {
                let digits: String = p.chars().take_while(char::is_ascii_digit).collect();
                (digits.parse().unwrap_or(0), p[digits.len()..].to_string())
            })
            .collect()
    };
    parts(a).cmp(&parts(b))
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

/// Top-level keys and the `Installers` entries of a winget installer manifest. Winget
/// manifests are generated YAML with one fixed shape; only plain `Key: value` lines are read
/// and anything nested deeper is ignored.
fn winget_yaml(text: &str) -> (BTreeMap<String, String>, Vec<BTreeMap<String, String>>) {
    let kv = |s: &str| {
        let (k, v) = s.split_once(':')?;
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(v);
        Some((k.trim().to_string(), v.to_string()))
    };
    let (mut top, mut items) = (BTreeMap::new(), Vec::<BTreeMap<String, String>>::new());
    let (mut in_list, mut dash, mut keys) = (false, None, 0);
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
                dash = Some(indent);
                keys = indent + 2;
                items.push(kv(rest).into_iter().collect());
                continue;
            }
            if dash.is_some() && indent == keys {
                if let (Some(item), Some((k, v))) = (items.last_mut(), kv(body)) {
                    item.insert(k, v);
                }
                continue;
            }
            if indent > 0 {
                continue;
            }
            in_list = false;
        }
        if indent == 0
            && let Some((k, v)) = kv(body)
        {
            if k == "Installers" && v.is_empty() {
                (in_list, dash) = (true, None);
            } else if !v.is_empty() {
                top.insert(k, v);
            }
        }
    }
    (top, items)
}

/// Chooses the installer for `arch` from a winget installer manifest. Prefers the matching
/// architecture, then installers Simple Setup can run unattended, then per-user ones (no
/// administrator prompt), then English or locale-neutral ones.
pub fn winget_installer(yaml: &str, arch: Arch) -> Result<Pick, String> {
    let (top, items) = winget_yaml(yaml);
    // 64-bit Windows runs 32-bit programs natively; ARM64 would need emulation, which is not
    // used on the user's behalf.
    let archs: &[&str] = match arch {
        Arch::X86_64 => &["x64", "neutral", "x86"],
        Arch::Aarch64 => &["arm64", "neutral"],
    };
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
        let manual = u8::from(matches!(kind, WinInstaller::Exe | WinInstaller::Msix));
        let machine = u8::from(get("Scope") != Some("user"));
        let locale = u8::from(!matches!(get("InstallerLocale"), None | Some("en-US")));
        let rank = (ai, manual, machine, locale);
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            let scope = if machine == 0 { ", per-user" } else { "" };
            best = Some((
                rank,
                Pick {
                    url: url.into(),
                    sha256: sha,
                    version: version.clone(),
                    win: Some(kind),
                    detail: format!("{} {kind:?} installer{scope}", archs[ai]),
                    name: String::new(),
                    sums_url: None,
                    archive: false,
                },
            ));
        }
    }
    best.map(|(_, p)| p).ok_or_else(|| match arch {
        Arch::Aarch64 => "The winget catalog lists no ARM64 installer for it.".into(),
        Arch::X86_64 => "The winget catalog lists no suitable installer for it.".into(),
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
    let want = match arch {
        Arch::X86_64 => "intel",
        Arch::Aarch64 => "arm",
    };
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
    let sha = sha
        .as_deref()
        .and_then(sha256_hex)
        .ok_or("Homebrew publishes no checksum for this download, so it cannot be verified.")?;
    let first = |key: &str| {
        c.artifacts
            .iter()
            .find_map(|a| a.get(key)?.as_array()?.first()?.as_str().map(String::from))
    };
    let app = first("app");
    if app.is_none() && first("pkg").is_none() {
        return Err("This cask does not install a plain app or installer package.".into());
    }
    Ok(CaskPick {
        pick: Pick {
            url,
            sha256: sha,
            version: c.version,
            win: None,
            detail: app.clone().unwrap_or_else(|| "installer package".into()),
            name: String::new(),
            sums_url: None,
            archive: false,
        },
        app,
    })
}

// ---------------------------------------------------------------- GitHub releases

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    /// `sha256:<hex>`, published by GitHub for assets uploaded since mid-2025.
    pub digest: Option<String>,
    pub browser_download_url: String,
}

/// Lowercase name tokens with architecture spellings unified (`amd64` → `x64`, `aarch64` → `arm64`).
fn tokens(name: &str) -> Vec<String> {
    let mut n = name.to_ascii_lowercase();
    for (from, to) in [
        ("x86_64", "x64"),
        ("x86-64", "x64"),
        ("amd64", "x64"),
        ("win64", "x64"),
        ("aarch64", "arm64"),
    ] {
        n = n.replace(from, to);
    }
    n.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
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

/// Chooses the release asset for `target`. In order of preference:
/// - Linux: an AppImage, else a `.tar.gz`/`.tar.xz` naming Linux (a program to unpack);
/// - Windows: an MSI, else an `.exe` installer, else a `.zip` naming Windows;
/// - macOS: a disk image, else a `.zip` naming macOS (an app or a program), else a tarball.
///
/// The name must name the target's architecture, `universal`, or no architecture at all;
/// assets for other architectures are never used. The SHA-256 comes from GitHub's digest or,
/// for older uploads, from a checksum list in the same release; without either the asset is
/// refused.
pub fn github_asset(rel: &Release, target: Platform) -> Result<Pick, String> {
    const ARCHS: [&str; 12] = [
        "x64", "arm64", "x86", "i386", "i686", "386", "win32", "armv7", "armhf", "intel", "s390x",
        "ppc64le",
    ];
    let mine: &[&str] = match target.arch {
        Arch::X86_64 => &["x64", "intel"],
        Arch::Aarch64 => &["arm64"],
    };
    // (architecture fit, file type, GNU build, name length): lower is better.
    type Rank = (u8, u8, u8, usize);
    let mut best: Option<(Rank, &Asset)> = None;
    for a in &rel.assets {
        let lower = a.name.to_ascii_lowercase();
        let t = tokens(&a.name);
        let ext = t.last().map(String::as_str).unwrap_or_default();
        let tarball = [".tar.gz", ".tgz", ".tar.xz"]
            .iter()
            .any(|e| lower.ends_with(e));
        let zip = lower.ends_with(".zip");
        let kind = match target.os {
            OsFamily::Linux if ext == "appimage" => 0,
            OsFamily::Linux if tarball && names_os(&lower, OsFamily::Linux) => 1,
            OsFamily::Windows if ext == "msi" => 0,
            OsFamily::Windows if ext == "exe" => 1,
            OsFamily::Windows if zip && names_os(&lower, OsFamily::Windows) => 2,
            OsFamily::Macos if ext == "dmg" => 0,
            OsFamily::Macos if zip && names_os(&lower, OsFamily::Macos) => 1,
            OsFamily::Macos if tarball && names_os(&lower, OsFamily::Macos) => 2,
            _ => continue,
        };
        if t.iter().any(|x| {
            matches!(
                x.as_str(),
                "sig" | "sha256" | "debug" | "symbols" | "src" | "source" | "pdb"
            )
        }) {
            continue;
        }
        let archy: Vec<&String> = t.iter().filter(|x| ARCHS.contains(&x.as_str())).collect();
        let fit = if t.iter().any(|x| x == "universal")
            || archy.iter().any(|x| mine.contains(&x.as_str()))
        {
            0
        } else if archy.is_empty() {
            1
        } else {
            continue;
        };
        // Static (musl) Linux builds run everywhere; MSVC Windows builds need no MinGW runtime.
        let gnu = u8::from(t.iter().any(|x| x == "gnu"));
        let rank = (fit, kind, gnu, a.name.len());
        if best.is_none_or(|(r, _)| rank < r) {
            best = Some((rank, a));
        }
    }
    let (_, a) =
        best.ok_or_else(|| format!("Release {} has no download for {target}.", rel.tag_name))?;
    let sha = a
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .and_then(sha256_hex);
    let sums_url = match sha {
        Some(_) => None,
        None => Some(
            sums_asset(rel, &a.name)
                .ok_or_else(|| format!("GitHub publishes no checksum for {}, so it cannot be verified; download it by hand.", a.name))?
                .browser_download_url
                .clone(),
        ),
    };
    if !https(&a.browser_download_url) || sums_url.as_deref().is_some_and(|u| !https(u)) {
        return Err(format!("{} has no secure download link.", a.name));
    }
    let lower = a.name.to_ascii_lowercase();
    let archive = [".tar.gz", ".tgz", ".tar.xz", ".zip"]
        .iter()
        .any(|e| lower.ends_with(e));
    let win = match (target.os, tokens(&a.name).last().map(String::as_str)) {
        (OsFamily::Windows, Some("msi")) => Some(WinInstaller::Msi),
        (OsFamily::Windows, Some("exe")) => Some(WinInstaller::Exe),
        _ => None,
    };
    Ok(Pick {
        url: a.browser_download_url.clone(),
        sha256: sha.unwrap_or_default(),
        version: rel.tag_name.clone(),
        win,
        detail: format!("release asset {} ({} MB)", a.name, a.size / 1_000_000),
        name: a.name.clone(),
        sums_url,
        archive,
    })
}

/// A checksum file in the release covering `name`: `<name>.sha256`, or a combined list such
/// as `SHA256SUMS` or `checksums.txt`.
fn sums_asset<'r>(rel: &'r Release, name: &str) -> Option<&'r Asset> {
    let own = [format!("{name}.sha256"), format!("{name}.sha256sum")];
    rel.assets
        .iter()
        .find(|a| own.iter().any(|o| a.name.eq_ignore_ascii_case(o)))
        .or_else(|| {
            rel.assets.iter().find(|a| {
                let l = a.name.to_ascii_lowercase();
                (l.contains("sha256") || l.contains("checksums"))
                    && !l.contains("sha512")
                    && (l.ends_with(".txt") || !l.contains('.'))
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
    let u16_at = |i: usize| Some(u16::from_le_bytes(cd.get(i..i + 2)?.try_into().ok()?) as usize);
    let (mut at, mut names) = (0, Vec::with_capacity(count.min(100_000)));
    for _ in 0..count {
        if !cd.get(at..)?.starts_with(b"PK\x01\x02") {
            return None;
        }
        let (n, x, c) = (u16_at(at + 28)?, u16_at(at + 30)?, u16_at(at + 32)?);
        names.push(String::from_utf8_lossy(cd.get(at + 46..at + 46 + n)?).into_owned());
        at += 46 + n + x + c;
    }
    Some(names)
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
            "github-rustdesk.json" => {
                include_str!("../tests/fixtures/catalog/github-rustdesk.json")
            }
            "github-ripgrep.json" => {
                include_str!("../tests/fixtures/catalog/github-ripgrep.json")
            }
            "github-obsidian.json" => {
                include_str!("../tests/fixtures/catalog/github-obsidian.json")
            }
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

        let no_check = r#"{"version":"1","url":"https://x/a.dmg","sha256":"no_check","artifacts":[{"app":["A.app"]}]}"#;
        assert!(
            cask_pick(no_check, Arch::Aarch64)
                .unwrap_err()
                .contains("checksum")
        );
        let arm_only = r#"{"version":"1","url":"https://x/a.dmg","sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","artifacts":[{"app":["A.app"]}],"depends_on":{"arch":[{"type":"arm","bits":64}]}}"#;
        assert!(cask_pick(arm_only, Arch::X86_64).is_err());
        assert!(cask_pick(arm_only, Arch::Aarch64).is_ok());
    }

    #[test]
    fn github_release_assets() {
        let rel: Release = serde_json::from_str(&fixture("github-rustdesk.json")).unwrap();
        let p = |os, arch| {
            github_asset(&rel, Platform { os, arch })
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
        )
        .unwrap();
        assert_eq!(
            (msi.win, msi.version.as_str()),
            (Some(WinInstaller::Msi), "1.5.0")
        );

        let unsigned: Release = serde_json::from_str(r#"{"tag_name":"v1","assets":[{"name":"t-arm64.AppImage","size":1,"digest":null,"browser_download_url":"https://g/t-arm64.AppImage"},{"name":"t-x86_64.AppImage","size":1,"digest":null,"browser_download_url":"https://g/t"}]}"#).unwrap();
        assert!(
            github_asset(
                &unsigned,
                Platform {
                    os: OsFamily::Linux,
                    arch: Arch::X86_64
                }
            )
            .unwrap_err()
            .contains("checksum")
        );
        assert!(
            github_asset(
                &unsigned,
                Platform {
                    os: OsFamily::Windows,
                    arch: Arch::X86_64
                }
            )
            .is_err()
        );
    }

    fn asset_name(rel: &Release, os: OsFamily, arch: Arch) -> Result<String, String> {
        github_asset(rel, Platform { os, arch }).map(|p| p.name)
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
        )
        .unwrap();
        assert!(p.archive && p.sums_url.is_none() && p.sha256.len() == 64);

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
        let exe = github_asset(
            &ob,
            Platform {
                os: Windows,
                arch: X86_64,
            },
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
