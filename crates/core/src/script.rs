//! Standalone install scripts: what a restore of a snapshot would install on a destination, as
//! a POSIX shell script (Linux, macOS; no Bash needed, so Alpine runs it too) or a PowerShell
//! script (Windows) that runs without Simple Setup.
//! The ways come from the plan the restore itself makes, best first, as fixed command lines:
//! each app is checked first and skipped when it is there, each command is tried three times,
//! and failures are listed at the end without stopping the rest. Ways that need an approval,
//! and the user's own install commands, are written as comments to review and run by hand.
//! Downloads are pinned to the SHA-256 their source publishes (none: not written). Settings and
//! files are not part of it, and no token ever is.

use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::compat::Candidate;
use crate::discovery::BuildTool;
use crate::distros::Adapter as Manager;
use crate::installers::{self, Collect, Elevation};
use crate::manifest::ManifestApp;
use crate::platform::{OsFamily, Platform};
use crate::resolve::{self, Method};
use crate::restore::{self, Target};
use crate::{Result, invalid};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// POSIX `sh`.
    Sh,
    PowerShell,
}

impl Shell {
    pub fn for_os(os: OsFamily) -> Self {
        if os == OsFamily::Windows {
            Self::PowerShell
        } else {
            Self::Sh
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "sh" | "bash" | "posix" => Some(Self::Sh),
            "powershell" | "pwsh" | "ps1" => Some(Self::PowerShell),
            _ => None,
        }
    }
}

/// An argument that names a file in the script's download folder.
const DL: &str = "@dl/";

/// One way to install an app, as fixed command lines.
#[derive(Debug, Default, Clone)]
struct Way {
    label: String,
    /// Succeeds when the app is there this way.
    check: Option<Vec<String>>,
    /// Run in order; the way worked when every one succeeded. `fetch URL SHA256 FILE`
    /// downloads and checks a file.
    run: Vec<Vec<String>>,
    admin: bool,
    /// A program that must be there first.
    needs: Option<String>,
    /// Not run: written as a comment with this reason.
    hold: Option<String>,
}

/// An app as the script installs it.
struct Entry {
    name: String,
    ways: Vec<Way>,
    /// Why nothing can be tried, when there is no way.
    why_none: String,
}

/// The script for restoring `bundle` on `t`. Looks up downloads online (their addresses and
/// checksums are written into the script); nothing is installed or downloaded here.
pub fn export(bundle: &Path, t: &Target, shell: Shell) -> Result<String> {
    let m = crate::manifest::Manifest::open(bundle, t.key.as_ref())?;
    let staging = std::env::temp_dir().join(format!("simple-setup-script-{}", std::process::id()));
    let cancel = AtomicBool::new(false);
    let mut entries = Vec::new();
    for (item, a, candidates) in restore::install_ways(bundle, t)? {
        let mut ways = Vec::new();
        for c in &candidates {
            ways.extend(ways_for(&a, c, t, &staging, &cancel));
        }
        entries.push(Entry {
            name: a.name.clone(),
            ways,
            why_none: item.reason.clone(),
        });
    }
    let _ = std::fs::remove_dir_all(&staging);
    let target = match t.info.profile() {
        Some(p) => format!("{} ({})", p.name, t.device.platform),
        None => t.device.platform.to_string(),
    };
    let header = [
        format!("Installs the apps of snapshot \"{}\" on {target}.", m.name),
        format!(
            "Made by Simple Setup {} on {}.",
            env!("CARGO_PKG_VERSION"),
            crate::now()
        ),
        "Safe to run again: each app is checked first and skipped when it is there.".into(),
        "Settings and personal files are not part of it: restore them with Simple Setup.".into(),
    ];
    Ok(match shell {
        Shell::Sh => sh(&header, &entries),
        Shell::PowerShell => powershell(&header, &entries),
    })
}

fn argv(c: &std::process::Command) -> Vec<String> {
    std::iter::once(c.get_program())
        .chain(c.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

/// The ways one candidate gives (a snap may need `--classic`, so it gives two).
fn ways_for(
    a: &ManifestApp,
    c: &Candidate,
    t: &Target,
    staging: &Path,
    cancel: &AtomicBool,
) -> Vec<Way> {
    let os = t.device.platform.os;
    let mut w = Way {
        label: c.method.label(os),
        hold: c.review.clone().filter(|_| c.own_review),
        ..Way::default()
    };
    match &c.method {
        Method::Flathub(id) => {
            w.check = Some(s(&["flatpak", "info", id]));
            w.needs = Some("flatpak".into());
            w.run = vec![
                s(&[
                    "flatpak",
                    "remote-add",
                    "--user",
                    "--if-not-exists",
                    "flathub",
                    "https://dl.flathub.org/repo/flathub.flatpakrepo",
                ]),
                s(&[
                    "flatpak",
                    "install",
                    "--user",
                    "-y",
                    "--noninteractive",
                    "flathub",
                    id,
                ]),
            ];
        }
        Method::System(mgr, p) => {
            w.check = system_check(*mgr, p, t);
            let cx = resolve::Ctx {
                home: &t.home,
                platform: t.device.platform,
                info: &t.info,
                elev: Elevation::None,
                downloads: staging,
                cancel,
                unverified: false,
            };
            // pacman's command depends on whether the computer has package lists yet: decided
            // there, not from the computer making the script.
            if *mgr == Manager::Pacman {
                w.run = vec![s(&["pacman_install", p])];
                return vec![w];
            }
            let Some(cmd) = resolve::install_command(*mgr, &[p], &cx) else {
                return Vec::new();
            };
            w.run = vec![argv(&cmd)];
            w.admin = match mgr {
                Manager::Custom => t
                    .info
                    .profile()
                    .and_then(|p| p.custom.as_ref())
                    .is_none_or(|c| c.admin),
                _ => true,
            };
            if mgr.needs_restart() {
                w.label += " (takes effect after a restart)";
            }
        }
        Method::Snap(n) => {
            w.check = Some(s(&["snap", "list", n]));
            w.needs = Some("snap".into());
            w.admin = true;
            w.run = vec![s(&["snap", "install", n])];
            let mut classic = w.clone();
            classic.label += " (classic confinement)";
            classic.run = vec![s(&["snap", "install", "--classic", n])];
            return vec![w, classic];
        }
        Method::Brew { name, cask } => {
            let kind = if *cask { "--cask" } else { "--formula" };
            w.check = Some(s(&["brew", "list", kind, name]));
            w.needs = Some("brew".into());
            w.run = vec![if *cask {
                s(&["brew", "install", "--cask", name])
            } else {
                s(&["brew", "install", name])
            }];
        }
        Method::Winget(id) | Method::Store(id) => {
            let store = matches!(c.method, Method::Store(_));
            let mut check = s(&["winget", "list", "--id", id, "--exact"]);
            let mut run = s(&["winget", "install", "--id", id, "--exact"]);
            if store {
                check.extend(s(&["--source", "msstore"]));
                run.extend(s(&["--source", "msstore"]));
            }
            check.extend(s(&[
                "--accept-source-agreements",
                "--disable-interactivity",
            ]));
            run.extend(s(&[
                "--silent",
                "--accept-package-agreements",
                "--accept-source-agreements",
                "--disable-interactivity",
            ]));
            w.check = Some(check);
            w.run = vec![run];
            w.needs = Some("winget".into());
        }
        Method::Scoop(n) => {
            w.check = Some(s(&["scoop", "prefix", n]));
            w.run = vec![s(&["scoop", "install", n])];
            w.needs = Some("scoop".into());
        }
        Method::Choco(n) => {
            w.check = Some(s(&[
                "listed",
                &format!("{n}|"),
                "choco",
                "list",
                "--exact",
                n,
                "--limit-output",
            ]));
            w.run = vec![s(&["choco", "install", n, "-y", "--no-progress"])];
            w.needs = Some("choco".into());
            w.admin = true;
        }
        Method::DevPackage { tool, name } => {
            let prog = resolve::tool_command(*tool);
            w.needs = Some(prog.into());
            match tool {
                BuildTool::Cargo => {
                    w.check = Some(s(&[
                        "listed",
                        &format!("{name} v"),
                        prog,
                        "install",
                        "--list",
                    ]));
                    w.run = vec![s(&[prog, "install", "--locked", name])];
                }
                BuildTool::Pipx => {
                    w.check = Some(s(&["listed", &format!("{name} "), prog, "list", "--short"]));
                    w.run = vec![s(&[prog, "install", name])];
                }
                BuildTool::Npm => {
                    w.check = Some(s(&[prog, "ls", "-g", name, "--depth=0"]));
                    w.run = vec![s(&[prog, "install", "-g", name])];
                }
                BuildTool::Go => return Vec::new(),
            }
        }
        Method::Download => return download(a, t, staging, cancel, w).into_iter().collect(),
        Method::Source { repo, .. } | Method::SourceLookup { repo, .. } => {
            w.hold = Some(format!(
                "Built from its source on the computer: clone https://github.com/{repo} and follow its build instructions."
            ));
        }
        Method::Custom { argv, admin } => {
            w.hold = Some("Your own install command: review it, then run it by hand.".into());
            w.run = vec![argv.clone()];
            w.admin = *admin;
        }
    }
    vec![w]
}

/// A command that succeeds when package `p` is installed.
fn system_check(mgr: Manager, p: &str, t: &Target) -> Option<Vec<String>> {
    Some(match mgr {
        Manager::Apt => s(&["deb_installed", p]),
        Manager::Dnf | Manager::Zypper | Manager::RpmOstree | Manager::TransactionalUpdate => {
            s(&["rpm", "-q", p])
        }
        Manager::Pacman => s(&["pacman", "-Q", p]),
        Manager::Apk => s(&["apk", "info", "-e", p]),
        Manager::Custom => {
            let c = t.info.profile()?.custom.as_ref()?;
            let (prog, args) = c.query.split_first()?;
            std::iter::once(prog.clone())
                .chain(
                    resolve::expand(args, "{package}", &[p])
                        .into_iter()
                        .map(|a| a.to_string_lossy().into_owned()),
                )
                .collect()
        }
        Manager::None => return None,
    })
}

/// A download, looked up now: its address and checksum go into the script.
fn download(
    a: &ManifestApp,
    t: &Target,
    staging: &Path,
    cancel: &AtomicBool,
    mut w: Way,
) -> Option<Way> {
    let target = t.device.platform;
    let app = resolve::as_app(a);
    let mut shared = installers::Shared::new();
    let mut quiet = |_: String| {};
    let mut c = Collect {
        staging,
        home: &t.home,
        cancel,
        shared: &mut shared,
        status: &mut quiet,
        to: &t.info,
        from: None,
        allow_unverified: false,
    };
    let found = match installers::remote_pick(&app, target, &mut c) {
        Ok(f) => f,
        Err(e) => {
            w.hold = Some(format!("The download could not be looked up: {e}"));
            return Some(w);
        }
    };
    let pick = found.pick;
    let url = pick.url.clone();
    if pick.sha256.is_empty() {
        w.hold = Some(format!(
            "{url} publishes no SHA-256 checksum, so the script does not download it."
        ));
        return Some(w);
    }
    w.label = format!("download {url}");
    let kind = installers::download_kind(target, &pick);
    let file = format!(
        "{DL}{}.{kind}",
        installers::portable_name(&format!("{}-{}", a.name, pick.version))
    );
    let fetch = vec![
        "fetch".to_string(),
        url.clone(),
        pick.sha256.clone(),
        file.clone(),
    ];
    let profile = t.info.profile();
    let install: Vec<String> = match (target.os, kind) {
        (OsFamily::Linux, "deb") if profile.is_some_and(|p| p.package_manager == Manager::Apt) => {
            w.admin = true;
            s(&["apt-get", "install", "-y", &file])
        }
        (OsFamily::Linux, "rpm") => match profile.map(|p| p.package_manager) {
            Some(Manager::Dnf) => {
                w.admin = true;
                s(&["dnf", "install", "-y", &file])
            }
            Some(Manager::Zypper) => {
                w.admin = true;
                s(&[
                    "zypper",
                    "--non-interactive",
                    "install",
                    "--allow-unsigned-rpm",
                    &file,
                ])
            }
            _ => return hold_by_hand(w, &url, &pick.sha256),
        },
        (OsFamily::Linux, "AppImage") => {
            let dest = format!(
                "~/Applications/{}.AppImage",
                installers::portable_name(&a.name)
            );
            w.check = Some(s(&["test", "-x", &dest]));
            s(&["put", &file, &dest])
        }
        (os, "program") => {
            let cmd = installers::program_command(&app);
            let dest = match os {
                OsFamily::Windows => format!("~/AppData/Local/Programs/{cmd}/{cmd}.exe"),
                _ => format!("~/.local/bin/{cmd}"),
            };
            w.label += &format!(" (the program itself, put at {dest})");
            w.check = Some(s(&["test", "-x", &dest]));
            s(&["put", &file, &dest])
        }
        (OsFamily::Macos, _) if url.to_ascii_lowercase().ends_with(".pkg") => {
            w.admin = true;
            s(&["installer", "-pkg", &file, "-target", "/"])
        }
        (OsFamily::Macos, _) if url.to_ascii_lowercase().ends_with(".dmg") => {
            if let Some(b) = &found.app_bundle {
                w.check = Some(s(&["test", "-d", &format!("/Applications/{b}")]));
            }
            s(&["install_dmg", &file])
        }
        (OsFamily::Windows, "msix") => s(&["Add-AppxPackage", &file]),
        // Named like a program, not an installer, with no unattended switches listed: the
        // program itself, as a restore treats it (which also checks the file's contents).
        (OsFamily::Windows, "exe")
            if pick.win == Some(crate::manifest::WinInstaller::Exe)
                && pick.silent.is_none()
                && installers::portable_exe(&pick.name) =>
        {
            let folder = installers::portable_name(&a.name);
            let dest = format!("~/AppData/Local/Programs/{folder}/{folder}.exe");
            w.label += &format!(" (the program itself, put at {dest})");
            w.check = Some(s(&["test", "-x", &dest]));
            s(&["put", &file, &dest])
        }
        (OsFamily::Windows, "msi" | "exe") => {
            let Some(kind) = pick.win else {
                return hold_by_hand(w, &url, &pick.sha256);
            };
            match installers::win_command(kind, pick.silent.as_deref(), Path::new(&file)) {
                Some((prog, args)) => std::iter::once(prog)
                    .chain(args)
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
                None => return hold_by_hand(w, &url, &pick.sha256),
            }
        }
        _ => return hold_by_hand(w, &url, &pick.sha256),
    };
    w.run = vec![fetch, install];
    Some(w)
}

fn hold_by_hand(mut w: Way, url: &str, sha: &str) -> Option<Way> {
    w.hold = Some(format!(
        "Download {url} (SHA-256 {sha}) and install it by hand."
    ));
    w.run.clear();
    Some(w)
}

// ---------------------------------------------------------------- POSIX sh

/// One word for the shell: single-quoted, or `"$HOME"/…` and `"$dl"/…` for the two folder marks.
fn bq(a: &str) -> String {
    let lit = |x: &str| format!("'{}'", x.replace('\'', r"'\''"));
    if let Some(rest) = a.strip_prefix("~/") {
        format!("\"$HOME\"/{}", lit(rest))
    } else if let Some(rest) = a.strip_prefix(DL) {
        format!("\"$dl\"/{}", lit(rest))
    } else {
        lit(a)
    }
}

/// Comment text on one line (a newline would end the comment and start code).
fn line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

fn sh_cmd(argv: &[String], admin: bool) -> String {
    let words: Vec<String> = argv.iter().map(|a| bq(a)).collect();
    let words = words.join(" ");
    if admin {
        format!("as_root {words}")
    } else {
        words
    }
}

const SH_PRELUDE: &str = r#"set -u
export NONINTERACTIVE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_ENV_HINTS=1
dl="${TMPDIR:-/tmp}/simple-setup-downloads"
mkdir -p "$dl"
failed='' manual=
log() { printf '%s\n' "$*"; }
quiet() { "$@" >/dev/null 2>&1; }
as_root() { if [ "$(id -u)" -eq 0 ]; then "$@"; else sudo -- "$@"; fi; }
retry() { local n=1; until "$@"; do [ "$n" -ge 3 ] && return 1; sleep $((n * 5)); n=$((n + 1)); done; }
have() { command -v "$1" >/dev/null 2>&1 || { log "  $1 is not installed, so this way is skipped."; return 1; }; }
deb_installed() { [ "$(dpkg-query -W -f='${db:Status-Status}' "$1" 2>/dev/null)" = installed ]; }
listed() { p=$1; shift; "$@" 2>/dev/null | (while IFS= read -r l; do case $l in "$p"*) exit 0 ;; esac; done; exit 1); }
put() { mkdir -p "$(dirname "$2")" && cp -f "$1" "$2" && chmod 755 "$2"; }
pacman_install() {
  if ls /var/lib/pacman/sync/*.db >/dev/null 2>&1; then as_root pacman -S --needed --noconfirm "$@"; return; fi
  log "  pacman has no package lists yet; Arch refreshes them only with a full upgrade, so every installed package is upgraded first (pacman -Syu)."
  as_root pacman -Syu --needed --noconfirm "$@"
}
sha_ok() { local h; if command -v sha256sum >/dev/null 2>&1; then h=$(sha256sum "$2"); else h=$(shasum -a 256 "$2"); fi; [ "${h%% *}" = "$1" ]; }
fetch() { [ -f "$3" ] && sha_ok "$2" "$3" && return 0; curl -fsSL -o "$3.part" "$1" && sha_ok "$2" "$3.part" && mv -f "$3.part" "$3"; }
install_dmg() { local m r; m=$(mktemp -d) && hdiutil attach -nobrowse -quiet -mountpoint "$m" "$1" || return 1; cp -R "$m"/*.app /Applications/; r=$?; hdiutil detach -quiet "$m"; return $r; }
"#;

fn sh(header: &[String], entries: &[Entry]) -> String {
    let mut o = String::from("#!/bin/sh\n");
    for h in header {
        o += &format!("# {}\n", line(h));
    }
    o += SH_PRELUDE;
    for (i, e) in entries.iter().enumerate() {
        let n = i + 1;
        let name = bq(&line(&e.name));
        o += &format!("\n# {}\napp_{n}() {{\n", line(&e.name));
        let checks: Vec<String> = e
            .ways
            .iter()
            .filter_map(|w| w.check.as_ref())
            .map(|c| format!("quiet {}", sh_cmd(c, false)))
            .collect();
        if !checks.is_empty() {
            o += &format!(
                "  if {}; then\n    log {name}': already installed'; return 0\n  fi\n",
                checks.join(" || ")
            );
        }
        let mut tried = false;
        for w in &e.ways {
            o += &format!("  # {}\n", line(&w.label));
            let mut cond: Vec<String> = Vec::new();
            if let Some(p) = &w.needs {
                cond.push(format!("have {}", bq(p)));
            }
            cond.extend(
                w.run
                    .iter()
                    .map(|r| format!("retry {}", sh_cmd(r, w.admin))),
            );
            if cond.is_empty() {
                if let Some(why) = &w.hold {
                    o += &format!("  # {}\n", line(why));
                }
                continue;
            }
            let block = format!(
                "if {}; then\n    log {name}': installed ({})'; return 0\n  fi\n",
                cond.join(" && "),
                line(&w.label).replace('\'', "")
            );
            match &w.hold {
                Some(why) => {
                    o += &format!(
                        "  # Needs your approval: {}\n  # Review it, then remove the # signs to use it:\n",
                        line(why)
                    );
                    for l in block.lines() {
                        o += &format!("  # {}\n", line(l));
                    }
                }
                None => {
                    tried = true;
                    o += &format!("  {block}");
                }
            }
        }
        if !tried && e.ways.is_empty() {
            o += &format!("  # No way to install it is known: {}\n", line(&e.why_none));
        }
        o += &if tried {
            format!("  log {name}': not installed'; return 1\n}}\n")
        } else {
            format!("  log {name}': install it by hand (see this script)'; return 2\n}}\n")
        };
        o += &format!(
            "app_{n}; case $? in 1) failed=\"${{failed:+$failed, }}\"{name} ;; 2) manual=\"${{manual:+$manual, }}\"{name} ;; esac\n"
        );
    }
    o += r#"
[ -z "$manual" ] || log "Install by hand: $manual"
if [ -n "$failed" ]; then log "Not installed: $failed"; exit 1; fi
log "Done."
"#;
    o
}

// ---------------------------------------------------------------- PowerShell

/// One argument for PowerShell: single-quoted (its four single-quote characters doubled), or
/// a path joined to `$HOME` or the download folder.
fn pq(a: &str) -> String {
    let lit = |x: &str| {
        let mut out = String::from("'");
        for c in x.chars() {
            if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
                out.push(c);
            }
            out.push(c);
        }
        out + "'"
    };
    if let Some(rest) = a.strip_prefix("~/") {
        format!("(Join-Path $HOME {})", lit(rest))
    } else if let Some(rest) = a.strip_prefix(DL) {
        format!("(Join-Path $dl {})", lit(rest))
    } else {
        lit(a)
    }
}

fn ps_array(argv: &[String]) -> String {
    let words: Vec<String> = argv.iter().map(|a| pq(a)).collect();
    format!("@({})", words.join(", "))
}

const PS_PRELUDE: &str = r#"$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$dl = Join-Path ([IO.Path]::GetTempPath()) 'simple-setup-downloads'
New-Item -ItemType Directory -Force -Path $dl | Out-Null
$failed = @(); $manual = @()
$IsAdmin = try { ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator) } catch { $false }
# Success, restart needed (3010, 1641), another version already there (1638), and winget's
# "already installed" (0x8A15002B) and "restart to finish" (0x8A150109).
$OkCodes = @(0, 3010, 1641, 1638, -1978335189, -1978334967)
function Invoke-Step([string[]]$Cmd) {
  $prog = $Cmd[0]; $rest = @($Cmd | Select-Object -Skip 1)
  $c = Get-Command $prog -ErrorAction SilentlyContinue
  if ($c -and $c.CommandType -eq 'Cmdlet') {
    try { & $prog @rest -ErrorAction Stop | Out-Host; return $true } catch { Write-Host "  $_"; return $false }
  }
  if ($prog -eq 'msiexec' -or $prog -like '*.exe') {
    $quoted = $rest | ForEach-Object { if ($_ -match '\s') { '"' + $_ + '"' } else { $_ } }
    $p = Start-Process -FilePath $prog -ArgumentList $quoted -Wait -PassThru
    return ($p.ExitCode -in $OkCodes)
  }
  $global:LASTEXITCODE = 0
  & $prog @rest | Out-Host
  return ($LASTEXITCODE -in $OkCodes)
}
function Test-Quiet([string[]]$Cmd) {
  $global:LASTEXITCODE = 0
  $prog = $Cmd[0]; $rest = @($Cmd | Select-Object -Skip 1)
  try { & $prog @rest *> $null } catch { return $false }
  return ($LASTEXITCODE -eq 0)
}
function listed([string]$Prefix, [string[]]$Cmd) {
  $prog = $Cmd[0]; $rest = @($Cmd | Select-Object -Skip 1)
  $out = & $prog @rest 2> $null
  $global:LASTEXITCODE = if (@($out | Where-Object { "$_".StartsWith($Prefix, 'OrdinalIgnoreCase') }).Count) { 0 } else { 1 }
}
function test([string]$Flag, [string]$Path) { $global:LASTEXITCODE = if (Test-Path $Path) { 0 } else { 1 } }
function put([string]$From, [string]$To) {
  try {
    New-Item -ItemType Directory -Force -Path (Split-Path $To) -ErrorAction Stop | Out-Null
    Copy-Item -Force -Path $From -Destination $To -ErrorAction Stop; $global:LASTEXITCODE = 0
  } catch { Write-Host "  $_"; $global:LASTEXITCODE = 1 }
}
function Invoke-Retry([scriptblock]$Do) {
  for ($i = 1; $i -le 3; $i++) { if (& $Do) { return $true }; if ($i -lt 3) { Start-Sleep -Seconds (5 * $i) } }
  return $false
}
function have([string]$Name) {
  if (Get-Command $Name -ErrorAction SilentlyContinue) { return $true }
  Write-Host "  $Name is not installed, so this way is skipped."; return $false
}
function fetch([string]$Url, [string]$Sha, [string]$File) {
  if ((Test-Path $File) -and (Get-FileHash $File -Algorithm SHA256).Hash -eq $Sha) { return $true }
  try { Invoke-WebRequest -Uri $Url -OutFile "$File.part" -UseBasicParsing } catch { return $false }
  if ((Get-FileHash "$File.part" -Algorithm SHA256).Hash -ne $Sha) { Remove-Item -Force "$File.part"; return $false }
  Move-Item -Force "$File.part" $File; return $true
}
"#;

fn ps_step(argv: &[String]) -> String {
    match argv.first().map(String::as_str) {
        Some("fetch") => {
            let words: Vec<String> = argv[1..].iter().map(|a| pq(a)).collect();
            format!("(Invoke-Retry {{ fetch {} }})", words.join(" "))
        }
        _ => format!("(Invoke-Retry {{ Invoke-Step {} }})", ps_array(argv)),
    }
}

fn powershell(header: &[String], entries: &[Entry]) -> String {
    let mut o = String::new();
    for h in header {
        o += &format!("# {}\n", line(h));
    }
    o += PS_PRELUDE;
    for (i, e) in entries.iter().enumerate() {
        let n = i + 1;
        let name = pq(&line(&e.name));
        o += &format!("\n# {}\nfunction Install-App{n} {{\n", line(&e.name));
        let checks: Vec<String> = e
            .ways
            .iter()
            .filter_map(|w| w.check.as_ref())
            .map(|c| format!("(Test-Quiet {})", ps_array(c)))
            .collect();
        if !checks.is_empty() {
            o += &format!(
                "  if ({}) {{ Write-Host ({name} + ': already installed'); return 0 }}\n",
                checks.join(" -or ")
            );
        }
        let mut tried = false;
        for w in &e.ways {
            o += &format!("  # {}\n", line(&w.label));
            let mut cond: Vec<String> = Vec::new();
            if let Some(p) = &w.needs {
                cond.push(format!("(have {})", pq(p)));
            }
            if w.admin {
                cond.push("$IsAdmin".into());
            }
            cond.extend(w.run.iter().map(|r| ps_step(r)));
            if w.run.is_empty() {
                if let Some(why) = &w.hold {
                    o += &format!("  # {}\n", line(why));
                }
                continue;
            }
            let block = format!(
                "if ({}) {{ Write-Host ({name} + ': installed ({})'); return 0 }}{}\n",
                cond.join(" -and "),
                line(&w.label).replace(['\'', '\u{2018}', '\u{2019}', '\u{201A}', '\u{201B}'], ""),
                if w.admin {
                    " elseif (-not $IsAdmin) { Write-Host '  This way needs PowerShell run as administrator.' }"
                } else {
                    ""
                }
            );
            match &w.hold {
                Some(why) => {
                    o += &format!(
                        "  # Needs your approval: {}\n  # Review it, then remove the # sign to use it:\n  # {}",
                        line(why),
                        line(&block)
                    );
                    o += "\n";
                }
                None => {
                    tried = true;
                    o += &format!("  {block}");
                }
            }
        }
        if !tried && e.ways.is_empty() {
            o += &format!("  # No way to install it is known: {}\n", line(&e.why_none));
        }
        o += &if tried {
            format!("  Write-Host ({name} + ': not installed'); return 1\n}}\n")
        } else {
            format!(
                "  Write-Host ({name} + ': install it by hand (see this script)'); return 2\n}}\n"
            )
        };
        o += &format!(
            "switch (Install-App{n}) {{ 1 {{ $failed += {name} }} 2 {{ $manual += {name} }} }}\n"
        );
    }
    o += r#"
if ($manual.Count) { Write-Host ('Install by hand: ' + ($manual -join ', ')) }
if ($failed.Count) { Write-Host ('Not installed: ' + ($failed -join ', ')); exit 1 }
Write-Host 'Done.'
"#;
    o
}

/// Refuses a script for a platform it cannot run on.
pub fn check_shell(shell: Shell, target: Platform) -> Result<()> {
    match (shell, target.os) {
        (Shell::PowerShell, OsFamily::Windows) | (Shell::Sh, OsFamily::Linux | OsFamily::Macos) => {
            Ok(())
        }
        (Shell::PowerShell, _) => Err(invalid("PowerShell scripts are made for Windows.")),
        (Shell::Sh, _) => Err(invalid("Shell scripts are made for Linux and macOS.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries() -> Vec<Entry> {
        let hostile = "Evil'; rm -rf ~ #\n$(reboot)\u{2019}x";
        vec![
            Entry {
                name: "jq".into(),
                ways: vec![
                    Way {
                        label: "APT package jq".into(),
                        check: Some(s(&["deb_installed", "jq"])),
                        run: vec![s(&["apt-get", "install", "-y", "jq"])],
                        admin: true,
                        ..Way::default()
                    },
                    Way {
                        label: "download https://example.org/jq".into(),
                        check: Some(s(&["test", "-x", "~/.local/bin/jq"])),
                        run: vec![
                            s(&[
                                "fetch",
                                "https://example.org/jq",
                                &"a".repeat(64),
                                "@dl/jq-1.8",
                            ]),
                            s(&["put", "@dl/jq-1.8", "~/.local/bin/jq"]),
                        ],
                        ..Way::default()
                    },
                ],
                why_none: String::new(),
            },
            Entry {
                name: hostile.into(),
                ways: vec![Way {
                    label: hostile.into(),
                    run: vec![s(&["sh", "-c", hostile])],
                    hold: Some(format!("Your own command\n{hostile}")),
                    ..Way::default()
                }],
                why_none: String::new(),
            },
            Entry {
                name: "Nothing".into(),
                ways: vec![],
                why_none: "No version for this system.\nreboot".into(),
            },
        ]
    }

    /// The shell script parses, quotes hostile names, and keeps comments on one line.
    #[test]
    fn sh_is_valid_and_quoted() {
        let text = sh(&["Header\nreboot".into()], &entries());
        for l in text.lines() {
            assert!(
                l.trim() != "reboot" && !l.starts_with("$(reboot)"),
                "a newline escaped a comment or quote: {l:?}"
            );
        }
        assert!(text.contains(r#"'Evil'\''; rm -rf ~ #"#));
        assert!(text.contains(r#""$HOME"/'.local/bin/jq'"#));
        assert!(text.contains("retry as_root 'apt-get' 'install' '-y' 'jq'"));
        // Parsed by every POSIX shell at hand (dash is Debian's sh, busybox Alpine's); not on
        // Windows, where `bash` may be WSL's launcher without a distribution.
        for shell in ["sh", "dash", "bash", "busybox"]
            .into_iter()
            .filter(|_| cfg!(unix))
        {
            let args: &[&str] = if shell == "busybox" {
                &["sh", "-n", "-c"]
            } else {
                &["-n", "-c"]
            };
            if let Ok(o) = std::process::Command::new(shell)
                .args(args)
                .arg(&text)
                .output()
            {
                assert!(
                    o.status.success(),
                    "{shell}: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
            }
        }
    }

    /// PowerShell quoting doubles every single-quote character, typographic ones included.
    #[test]
    fn powershell_quotes_and_parses() {
        assert_eq!(pq("it's"), "'it''s'");
        assert_eq!(pq("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert_eq!(pq("~/x y"), "(Join-Path $HOME 'x y')");
        let text = powershell(&["Header".into()], &entries());
        for l in text.lines() {
            assert!(l.trim() != "reboot", "a newline escaped a comment: {l:?}");
        }
        // Parsed by PowerShell itself where it is installed (the Windows runner).
        let ps = if cfg!(windows) { "powershell" } else { "pwsh" };
        let file = std::env::temp_dir().join(format!("ss-script-{}.ps1", std::process::id()));
        std::fs::write(&file, &text).unwrap();
        let check = format!(
            "$e = $null; [void][System.Management.Automation.Language.Parser]::ParseFile('{}', [ref]$null, [ref]$e); if ($e.Count) {{ $e | ForEach-Object {{ $_.ToString() }}; exit 1 }}",
            file.display()
        );
        if let Ok(o) = std::process::Command::new(ps)
            .args(["-NoProfile", "-NonInteractive", "-Command", &check])
            .output()
        {
            assert!(
                o.status.success(),
                "{}{}",
                String::from_utf8_lossy(&o.stdout),
                String::from_utf8_lossy(&o.stderr)
            );
        }
        let _ = std::fs::remove_file(file);
    }
}
