//! Updates of Simple Setup itself from its GitHub releases.
//!
//! [`check`] reads the latest published release (drafts and prereleases are never offered),
//! compares versions, and picks the file that matches how this copy was installed (AppImage,
//! deb, rpm, MSI, NSIS installer, macOS app) and this computer's processor. [`download`]
//! fetches it into the cache folder, resuming an interrupted download, and refuses it unless it
//! matches both GitHub's digest and the release's `SHA256SUMS` (whichever are published, at
//! least one). [`install`] replaces the app: AppImages and macOS apps are swapped in place with
//! the old copy kept until the new one has started (and put back if the swap fails); packages
//! and Windows installers go through the system's own installer, which rolls back on failure.
//! Settings, snapshots and profiles live in the user's folders and are never touched.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::catalog::{self, Asset};
use crate::installers::{Elevation, admin_cmd, run, which};
use crate::platform::{Arch, OsFamily, Platform};
use crate::{Result, fetch, invalid};

/// The official repository.
pub const REPO: &str = "fleetwoodjohnr/simple-setup";

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// How this copy of the app was installed, which decides the file an update needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum InstallKind {
    AppImage,
    Deb,
    Rpm,
    Msi,
    Nsis,
    MacApp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UpdateFile {
    pub name: String,
    pub size: u64,
    pub url: String,
    pub sha256: String,
    /// Where the checksum came from: GitHub's digest, the release's SHA256SUMS, or both.
    pub checked_by: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct UpdateInfo {
    pub current: String,
    /// Latest published release, without a leading `v`; `None` before the first release.
    pub latest: Option<String>,
    pub newer: bool,
    /// Release notes (Markdown, as written on GitHub).
    pub notes: String,
    pub published_at: Option<String>,
    pub release_url: Option<String>,
    pub kind: Option<InstallKind>,
    /// The file that updates this copy, when there is a newer release for it.
    pub file: Option<UpdateFile>,
    /// Why this copy cannot update itself, when it cannot.
    pub cannot_install: Option<String>,
    /// One sentence for the person.
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    html_url: String,
    assets: Vec<Asset>,
}

/// Semantic version order: numbers compared numerically, a prerelease (`1.2.0-rc1`) before
/// its release, build metadata ignored. Unparsable parts compare as text.
pub fn version_cmp(a: &str, b: &str) -> Ordering {
    let split = |v: &str| {
        let v = v.trim().trim_start_matches(['v', 'V']);
        let v = v.split('+').next().unwrap_or_default();
        let (core, pre) = v.split_once('-').map_or((v, None), |(c, p)| (c, Some(p)));
        let nums: Vec<u64> = core.split('.').map(|n| n.parse().unwrap_or(0)).collect();
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

/// How the running copy was installed, or why it cannot update itself.
pub fn installed_as() -> std::result::Result<(InstallKind, PathBuf), String> {
    if cfg!(debug_assertions) {
        return Err("This is a development build; it is not updated automatically.".into());
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    match Platform::current().map_err(|e| e.to_string())?.os {
        OsFamily::Linux => {
            if let Some(img) = std::env::var_os("APPIMAGE").map(PathBuf::from)
                && img.is_file()
            {
                return Ok((InstallKind::AppImage, img));
            }
            let owned = |prog: &str, flag: &str| {
                which(prog).is_some()
                    && Command::new(prog)
                        .arg(flag)
                        .arg(&exe)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success())
            };
            if owned("dpkg", "-S") {
                Ok((InstallKind::Deb, exe))
            } else if owned("rpm", "-qf") {
                Ok((InstallKind::Rpm, exe))
            } else {
                Err("This copy was not installed from a Simple Setup package; update it the way you installed it.".into())
            }
        }
        OsFamily::Windows => {
            let dir = exe.parent().unwrap_or(Path::new("."));
            if dir.join("uninstall.exe").is_file() {
                Ok((InstallKind::Nsis, exe))
            } else {
                Ok((InstallKind::Msi, exe))
            }
        }
        OsFamily::Macos => exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .map(|app| (InstallKind::MacApp, app.to_path_buf()))
            .ok_or_else(|| "This copy is not inside an app bundle.".into()),
    }
}

/// The release file for `kind` on `arch`, by the names `scripts/build-release.sh` gives them.
pub fn pick_file(assets: &[Asset], kind: InstallKind, arch: Arch) -> Option<&Asset> {
    let ends = match kind {
        InstallKind::AppImage => ".appimage",
        InstallKind::Deb => ".deb",
        InstallKind::Rpm => ".rpm",
        InstallKind::Msi => ".msi",
        InstallKind::Nsis => "-setup.exe",
        InstallKind::MacApp => "-app.tar.gz",
    };
    let archs: &[&str] = match arch {
        Arch::X86_64 => &["x86_64", "amd64", "x64"],
        Arch::Aarch64 => &["aarch64", "arm64"],
    };
    assets.iter().find(|a| {
        let n = a.name.to_ascii_lowercase();
        let words: Vec<&str> = n
            .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .flat_map(|w| std::iter::once(w).chain(w.split('_')))
            .collect();
        n.starts_with("simple") && n.ends_with(ends) && words.iter().any(|w| archs.contains(w))
    })
}

/// The SHA-256 of `file`: GitHub's digest and the release's SHA256SUMS must agree when both
/// exist; at least one is required.
fn file_sha(
    rel: &Release,
    file: &Asset,
    sums: Option<&str>,
) -> std::result::Result<(String, String), String> {
    let digest = file
        .digest
        .as_deref()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase);
    let listed = sums.and_then(|s| catalog::parse_sums(s, &file.name));
    match (digest, listed) {
        (Some(d), Some(l)) if d == l => {
            Ok((d, "GitHub's digest and the release's SHA256SUMS".into()))
        }
        (Some(_), Some(_)) => Err(format!(
            "Release {} lists two different checksums for {}; it is not installed.",
            rel.tag_name, file.name
        )),
        (Some(d), None) => Ok((d, "GitHub's digest".into())),
        (None, Some(l)) => Ok((l, "the release's SHA256SUMS".into())),
        (None, None) => Err(format!(
            "Release {} publishes no checksum for {}, so it cannot be verified.",
            rel.tag_name, file.name
        )),
    }
}

fn not_found(e: &crate::Error) -> bool {
    e.to_string().contains("error: 404")
}

/// Looks for a newer release. Never installs anything.
pub fn check(cancel: &AtomicBool) -> Result<UpdateInfo> {
    let current = current_version().to_string();
    let rel: Release = match fetch::json(
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        Some("application/vnd.github+json"),
        cancel,
    ) {
        Ok(r) => r,
        Err(e) if not_found(&e) => {
            return Ok(UpdateInfo {
                message: format!(
                    "You have Simple Setup {current}. No release has been published on GitHub yet."
                ),
                current,
                latest: None,
                newer: false,
                notes: String::new(),
                published_at: None,
                release_url: None,
                kind: installed_as().ok().map(|(k, _)| k),
                file: None,
                cannot_install: None,
            });
        }
        Err(e) => return Err(e),
    };
    let latest = rel.tag_name.trim_start_matches(['v', 'V']).to_string();
    let newer = version_cmp(&latest, &current) == Ordering::Greater;
    let installed = installed_as();
    let mut info = UpdateInfo {
        current: current.clone(),
        latest: Some(latest.clone()),
        newer,
        notes: rel.body.clone().unwrap_or_default(),
        published_at: rel.published_at.clone(),
        release_url: Some(rel.html_url.clone()),
        kind: installed.as_ref().ok().map(|(k, _)| *k),
        file: None,
        cannot_install: installed.as_ref().err().cloned(),
        message: if newer {
            format!("Simple Setup {latest} is available (you have {current}).")
        } else {
            format!("You have the latest version, Simple Setup {current}.")
        },
    };
    if !newer {
        return Ok(info);
    }
    let Ok((kind, _)) = installed else {
        return Ok(info);
    };
    let arch = Platform::current()?.arch;
    let Some(asset) = pick_file(&rel.assets, kind, arch) else {
        info.cannot_install = Some(format!(
            "Release {} has no {kind:?} file for {arch}; download it from the release page.",
            rel.tag_name
        ));
        return Ok(info);
    };
    let sums = match rel.assets.iter().find(|a| a.name == "SHA256SUMS") {
        Some(a) => Some(fetch::text(&a.browser_download_url, None, cancel)?),
        None => None,
    };
    match file_sha(&rel, asset, sums.as_deref()) {
        Ok((sha256, checked_by)) => {
            info.file = Some(UpdateFile {
                name: asset.name.clone(),
                size: asset.size,
                url: asset.browser_download_url.clone(),
                sha256,
                checked_by,
            });
        }
        Err(why) => info.cannot_install = Some(why),
    }
    Ok(info)
}

fn cache_dir(version: &str) -> Result<PathBuf> {
    let base = dirs::cache_dir().ok_or_else(|| invalid("No cache folder for this user."))?;
    Ok(base.join("simple-setup").join("updates").join(version))
}

/// The update file's first bytes fit the kind of file it should be.
fn looks_like(p: &Path, kind: InstallKind) -> bool {
    let mut head = [0u8; 8];
    let ok = fs::File::open(p)
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut head))
        .is_ok();
    ok && match kind {
        InstallKind::AppImage => head.starts_with(b"\x7fELF"),
        InstallKind::Deb => head.starts_with(b"!<arch>"),
        InstallKind::Rpm => head.starts_with(&[0xed, 0xab, 0xee, 0xdb]),
        InstallKind::Msi => head.starts_with(&[0xd0, 0xcf, 0x11, 0xe0]),
        InstallKind::Nsis => head.starts_with(b"MZ"),
        InstallKind::MacApp => head.starts_with(&[0x1f, 0x8b]),
    }
}

/// Downloads and verifies the update; `progress` hears (bytes so far, total).
pub fn download(
    info: &UpdateInfo,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf> {
    let (Some(file), Some(kind), Some(version)) = (&info.file, info.kind, &info.latest) else {
        return Err(invalid(
            info.cannot_install
                .clone()
                .unwrap_or_else(|| "There is no update to download.".into()),
        ));
    };
    if file.name.contains(['/', '\\']) || file.name.starts_with('.') {
        return Err(invalid("Unexpected update file name."));
    }
    let dir = cache_dir(version)?;
    fs::create_dir_all(&dir).map_err(|e| invalid(format!("Creating {}: {e}", dir.display())))?;
    let dest = dir.join(&file.name);
    let total = file.size;
    fetch::download_resumable(&file.url, &dest, &file.sha256, cancel, &mut |done| {
        progress(done, total)
    })?;
    if !looks_like(&dest, kind) {
        let _ = fs::remove_file(&dest);
        return Err(invalid(
            "The downloaded file is not the expected kind of installer; it was discarded.",
        ));
    }
    Ok(dest)
}

/// What happens after [`install`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AfterInstall {
    /// The new version has been started; this one should quit now.
    Relaunched,
    /// The system installer is running; this one should quit so its files can be replaced.
    QuitForInstaller,
}

/// Next to `p`: `<name>.<suffix>`.
fn beside(p: &Path, suffix: &str) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{suffix}"));
    p.with_file_name(name)
}

/// Puts `new` where `target` is, keeping the old copy as `<target>.old` until the next start
/// (see [`cleanup`]). If moving the new one in fails, the old one is put back.
fn swap(target: &Path, new: &Path) -> Result<()> {
    let old = beside(target, "old");
    if old.exists() {
        let _ = if old.is_dir() {
            fs::remove_dir_all(&old)
        } else {
            fs::remove_file(&old)
        };
    }
    fs::rename(target, &old)
        .map_err(|e| invalid(format!("Could not move the current version aside: {e}")))?;
    if let Err(e) = fs::rename(new, target) {
        let _ = fs::rename(&old, target);
        return Err(invalid(format!(
            "Could not put the new version in place ({e}); the current version was kept."
        )));
    }
    Ok(())
}

/// Installs the downloaded update for this copy. Asks for an administrator password where the
/// system requires it (packages, per-machine Windows installers).
pub fn install(file: &Path, kind: InstallKind, elev: Elevation) -> Result<AfterInstall> {
    let (installed, target) = installed_as().map_err(invalid)?;
    if installed != kind || !looks_like(file, kind) {
        return Err(invalid(
            "This update does not fit how Simple Setup is installed here.",
        ));
    }
    let never = AtomicBool::new(false);
    match kind {
        InstallKind::AppImage => {
            let new = beside(&target, "new");
            fs::copy(file, &new).map_err(|e| invalid(format!("Copying the update: {e}")))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&new, fs::Permissions::from_mode(0o755))
                    .map_err(|e| invalid(e.to_string()))?;
            }
            swap(&target, &new)?;
            spawn(&mut Command::new(&target))?;
            Ok(AfterInstall::Relaunched)
        }
        InstallKind::Deb | InstallKind::Rpm => {
            let (prog, args): (&str, Vec<&str>) = match kind {
                InstallKind::Deb => ("apt-get", vec!["install", "-y"]),
                _ if which("dnf").is_some() => ("dnf", vec!["install", "-y"]),
                _ if which("zypper").is_some() => ("zypper", vec!["--non-interactive", "install"]),
                _ => ("rpm", vec!["-U"]),
            };
            run(admin_cmd(prog, true, elev).args(args).arg(file), &never)?;
            spawn(&mut Command::new(&target))?;
            Ok(AfterInstall::Relaunched)
        }
        InstallKind::Msi => {
            spawn(
                Command::new("msiexec")
                    .arg("/i")
                    .arg(file)
                    .args(["/passive", "/norestart"]),
            )?;
            Ok(AfterInstall::QuitForInstaller)
        }
        InstallKind::Nsis => {
            // Tauri's installer: /P passive, /UPDATE keep shortcuts, /R start the app again.
            // It asks Windows for administrator rights itself (per-machine install).
            spawn(
                Command::new("powershell")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "Start-Process -FilePath $env:SS_PROGRAM -ArgumentList '/P','/UPDATE','/R' -Verb RunAs",
                    ])
                    .env("SS_PROGRAM", file),
            )?;
            Ok(AfterInstall::QuitForInstaller)
        }
        InstallKind::MacApp => {
            let parent = target.parent().unwrap_or(Path::new("/Applications"));
            let unpack = parent.join(format!(".simple-setup-update-{}", std::process::id()));
            fs::create_dir_all(&unpack).map_err(|e| {
                invalid(format!(
                    "Cannot write to {} ({e}); install the update from the disk image instead.",
                    parent.display()
                ))
            })?;
            let result = (|| {
                run(
                    Command::new("tar")
                        .arg("-xzf")
                        .arg(file)
                        .arg("-C")
                        .arg(&unpack),
                    &never,
                )?;
                let app = fs::read_dir(&unpack)
                    .map_err(|e| invalid(e.to_string()))?
                    .flatten()
                    .map(|e| e.path())
                    .find(|p| p.extension().is_some_and(|e| e == "app"))
                    .ok_or_else(|| invalid("The update holds no app."))?;
                swap(&target, &app)
            })();
            let _ = fs::remove_dir_all(&unpack);
            result?;
            spawn(Command::new("open").arg("-n").arg(&target))?;
            Ok(AfterInstall::Relaunched)
        }
    }
}

fn spawn(cmd: &mut Command) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| invalid(format!("Could not start {:?}: {e}", cmd.get_program())))
}

/// After an update has started successfully: removes the previous version kept beside it.
pub fn cleanup() {
    if let Ok((InstallKind::AppImage | InstallKind::MacApp, target)) = installed_as() {
        let old = beside(&target, "old");
        let _ = if old.is_dir() {
            fs::remove_dir_all(&old)
        } else {
            fs::remove_file(&old)
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_versions() {
        use Ordering::*;
        for (a, b, want) in [
            ("0.2.0", "0.1.0", Greater),
            ("v0.10.0", "0.9.9", Greater),
            ("1.0.0", "1.0", Equal),
            ("1.0.0-rc1", "1.0.0", Less),
            ("1.0.0-rc.2", "1.0.0-rc.10", Less),
            ("1.0.0+build5", "1.0.0", Equal),
            ("0.1.0", "0.1.1", Less),
        ] {
            assert_eq!(version_cmp(a, b), want, "{a} vs {b}");
        }
    }

    fn asset(name: &str) -> Asset {
        serde_json::from_value(serde_json::json!({
            "name": name, "size": 1, "digest": null,
            "browser_download_url": format!("https://github.com/x/{name}")
        }))
        .unwrap()
    }

    #[test]
    fn release_files_by_kind_and_processor() {
        // The names scripts/build-release.sh and the Tauri bundler give each package.
        let assets: Vec<Asset> = [
            "Simple Setup_0.2.0_amd64.AppImage",
            "Simple Setup_0.2.0_aarch64.AppImage",
            "Simple Setup_0.2.0_amd64.deb",
            "Simple Setup_0.2.0_arm64.deb",
            "Simple Setup-0.2.0-1.x86_64.rpm",
            "Simple Setup-0.2.0-1.aarch64.rpm",
            "Simple Setup_0.2.0_x64_en-US.msi",
            "Simple Setup_0.2.0_x64-setup.exe",
            "Simple Setup_0.2.0_arm64-setup.exe",
            "Simple Setup_0.2.0_aarch64.dmg",
            "Simple-Setup-0.2.0-macos-aarch64-app.tar.gz",
            "Simple-Setup-0.2.0-macos-x86_64-app.tar.gz",
            "simple-setup-0.2.0-linux-x86_64",
            "SHA256SUMS",
        ]
        .into_iter()
        .map(asset)
        .collect();
        let pick = |k, a| pick_file(&assets, k, a).map(|x| x.name.as_str());
        use Arch::*;
        use InstallKind::*;
        assert_eq!(
            pick(AppImage, X86_64),
            Some("Simple Setup_0.2.0_amd64.AppImage")
        );
        assert_eq!(
            pick(AppImage, Aarch64),
            Some("Simple Setup_0.2.0_aarch64.AppImage")
        );
        assert_eq!(pick(Deb, Aarch64), Some("Simple Setup_0.2.0_arm64.deb"));
        assert_eq!(pick(Rpm, X86_64), Some("Simple Setup-0.2.0-1.x86_64.rpm"));
        assert_eq!(pick(Msi, X86_64), Some("Simple Setup_0.2.0_x64_en-US.msi"));
        assert_eq!(pick(Msi, Aarch64), None, "no ARM64 MSI is built");
        assert_eq!(
            pick(Nsis, Aarch64),
            Some("Simple Setup_0.2.0_arm64-setup.exe")
        );
        assert_eq!(
            pick(MacApp, Aarch64),
            Some("Simple-Setup-0.2.0-macos-aarch64-app.tar.gz")
        );
        assert_eq!(
            pick(MacApp, X86_64),
            Some("Simple-Setup-0.2.0-macos-x86_64-app.tar.gz")
        );
    }

    #[test]
    fn checksums_must_agree() {
        let rel = Release {
            tag_name: "v1".into(),
            body: None,
            published_at: None,
            html_url: String::new(),
            assets: vec![],
        };
        let a = "a".repeat(64);
        let mut f = asset("x.deb");
        assert!(file_sha(&rel, &f, None).is_err());
        let sums = format!("{a}  x.deb\n");
        assert_eq!(file_sha(&rel, &f, Some(&sums)).unwrap().0, a);
        f.digest = Some(format!("sha256:{a}"));
        assert!(file_sha(&rel, &f, Some(&sums)).unwrap().1.contains("and"));
        f.digest = Some(format!("sha256:{}", "b".repeat(64)));
        assert!(file_sha(&rel, &f, Some(&sums)).is_err());
    }

    #[test]
    fn swap_keeps_and_restores_the_old_copy() {
        let t = tempfile::tempdir().unwrap();
        let target = t.path().join("Simple Setup.AppImage");
        let new = t.path().join("new");
        fs::write(&target, "old version").unwrap();
        fs::write(&new, "new version").unwrap();
        swap(&target, &new).unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "new version");
        assert_eq!(
            fs::read_to_string(beside(&target, "old")).unwrap(),
            "old version"
        );
        // The new file is missing: the current version stays in place.
        assert!(swap(&target, &t.path().join("missing")).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "new version");
    }

    /// Asks the real GitHub API: `cargo test -p simple-setup-core -- --ignored live`.
    #[test]
    #[ignore = "uses the network"]
    fn live_check_against_the_repository() {
        let info = check(&AtomicBool::new(false)).unwrap();
        println!("{info:#?}");
        assert_eq!(info.current, current_version());
        assert!(!info.message.is_empty());
    }
}
