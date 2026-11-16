//! Developer tools installed with the language package managers, on any system: `cargo
//! install` (crates.io), pipx and global npm packages. They are reinstalled the same way.

use std::path::Path;

use super::{App, Ctx, Kind, Source};
use crate::platform::OsFamily;

/// Adds the developer tools found to `cx.apps`.
pub fn scan(cx: &mut Ctx) {
    let home = cx.home.to_path_buf();
    let mut found = cargo(&home);
    if let Some(text) = super::run("pipx", &["list", "--json"]) {
        found.extend(pipx(&text));
    }
    if let Some(text) = super::run("npm", &["ls", "-g", "--depth=0", "--json"]) {
        found.extend(npm(&text));
    }
    for (source, name, version) in found {
        let mut app = App::new(source, &name, &name, Kind::CommandLine);
        app.version = Some(version);
        app.available_on = vec![OsFamily::Linux, OsFamily::Windows, OsFamily::Macos];
        app.evidence.push(format!(
            "Installed with {}",
            match source {
                Source::Cargo => "cargo install (crates.io)",
                Source::Pipx => "pipx",
                _ => "npm install -g",
            }
        ));
        cx.apps.push(app);
    }
}

fn plain(name: &str) -> bool {
    crate::manifest::identifier_ok(name) && !name.contains('/')
        || name.starts_with('@') && crate::manifest::identifier_ok(&name[1..])
}

/// Crates installed from crates.io, from Cargo's own record (`~/.cargo/.crates2.json`).
fn cargo(home: &Path) -> Vec<(Source, String, String)> {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| home.join(".cargo"));
    let Ok(text) = std::fs::read_to_string(cargo_home.join(".crates2.json")) else {
        return Vec::new();
    };
    parse_crates2(&text)
}

/// `"<name> <version> (registry+https://github.com/rust-lang/crates.io-index)"` keys; git and
/// path installs are left out (a checkout found in an approved folder covers those).
pub fn parse_crates2(text: &str) -> Vec<(Source, String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out: Vec<(Source, String, String)> = v["installs"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, _)| {
            let mut parts = key.splitn(3, ' ');
            let (name, version, from) = (parts.next()?, parts.next()?, parts.next()?);
            (from.contains("crates.io-index") && plain(name))
                .then(|| (Source::Cargo, name.to_string(), version.to_string()))
        })
        .collect();
    out.sort();
    out
}

/// `pipx list --json`: each venv's main package and version.
pub fn pipx(text: &str) -> Vec<(Source, String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out: Vec<(Source, String, String)> = v["venvs"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, venv)| {
            let main = &venv["metadata"]["main_package"];
            let name = main["package"].as_str()?;
            let version = main["package_version"].as_str().unwrap_or_default();
            plain(name).then(|| (Source::Pipx, name.to_string(), version.to_string()))
        })
        .collect();
    out.sort();
    out
}

/// `npm ls -g --depth=0 --json`, without npm itself and Node's own helpers.
pub fn npm(text: &str) -> Vec<(Source, String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let mut out: Vec<(Source, String, String)> = v["dependencies"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| !matches!(name.as_str(), "npm" | "corepack") && plain(name))
        .map(|(name, d)| {
            (
                Source::Npm,
                name.clone(),
                d["version"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect();
    out.sort();
    out
}

/// The ID of the shell and command-line settings entry.
pub const DOTFILES: &str = "dotfiles";

/// Shell and command-line settings that exist in the home folder (shell start-up files, the
/// SSH client's configuration, editor and terminal settings, personal scripts in `~/bin`),
/// as one settings-only entry. Keys and other secrets in them are still withheld by the
/// secret rules. Git's settings belong to Git.
pub fn dotfiles(home: &Path, os: OsFamily) -> Option<App> {
    let candidates: &[&str] = match os {
        OsFamily::Windows => &[
            "Documents/PowerShell/Microsoft.PowerShell_profile.ps1",
            "Documents/WindowsPowerShell/Microsoft.PowerShell_profile.ps1",
            ".ssh/config",
            ".wslconfig",
            ".editorconfig",
        ],
        _ => &[
            ".bashrc",
            ".bash_profile",
            ".bash_aliases",
            ".profile",
            ".zshrc",
            ".zprofile",
            ".zshenv",
            ".config/fish",
            ".inputrc",
            ".vimrc",
            ".tmux.conf",
            ".config/starship.toml",
            ".ssh/config",
            ".editorconfig",
            "bin",
        ],
    };
    let mut app = App::new(
        Source::Manual,
        DOTFILES,
        "Shell and command-line settings",
        Kind::Settings,
    );
    for c in candidates {
        app.add_location(
            super::LocKind::Config,
            home.join(c),
            super::Origin::Detected,
        );
    }
    app.evidence.push(
        "Shell start-up files, terminal and editor settings and personal scripts in your home folder."
            .into(),
    );
    app.canonical = Some(DOTFILES.into());
    app.category = Some("System".into());
    (!app.locations.is_empty()).then_some(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_package_lists() {
        let crates = r#"{"installs": {
            "ripgrep 14.1.1 (registry+https://github.com/rust-lang/crates.io-index)": {"bins": ["rg"]},
            "mytool 0.1.0 (git+https://github.com/me/mytool#abc)": {"bins": ["mytool"]},
            "local 0.1.0 (path+file:///home/u/local)": {"bins": ["local"]}}}"#;
        assert_eq!(
            parse_crates2(crates),
            vec![(Source::Cargo, "ripgrep".into(), "14.1.1".into())]
        );
        let p = r#"{"venvs": {"black": {"metadata": {"main_package": {"package": "black", "package_version": "24.1.0"}}}}}"#;
        assert_eq!(
            pipx(p),
            vec![(Source::Pipx, "black".into(), "24.1.0".into())]
        );
        let n = r#"{"dependencies": {"npm": {"version": "10"}, "typescript": {"version": "5.6.3"}, "@angular/cli": {"version": "18.0.0"}, "../evil": {"version": "1"}}}"#;
        assert_eq!(
            npm(n),
            vec![
                (Source::Npm, "@angular/cli".into(), "18.0.0".into()),
                (Source::Npm, "typescript".into(), "5.6.3".into())
            ]
        );
        assert!(parse_crates2("not json").is_empty());
    }

    #[test]
    fn dotfiles_only_when_present() {
        let t = tempfile::tempdir().unwrap();
        assert!(dotfiles(t.path(), OsFamily::Linux).is_none());
        std::fs::write(t.path().join(".bashrc"), "export PATH=$HOME/bin:$PATH\n").unwrap();
        std::fs::create_dir_all(t.path().join("bin")).unwrap();
        let d = dotfiles(t.path(), OsFamily::Linux).unwrap();
        assert_eq!(d.id, "manual:dotfiles");
        assert_eq!(d.kind, Kind::Settings);
        assert_eq!(d.locations.len(), 2);
    }
}
