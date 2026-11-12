//! Repository-native dependency sets. Collection uses an empty installed database so
//! dependencies present on the source are still included. Restore never uses the network.
use crate::discovery::{App, Source};
use crate::installers::{Collect, run, stored, succeeds};
use crate::manifest::{NativeManager, Package, Recipe};
use crate::{Result, invalid};
use std::{ffi::OsString, fs, process::Command};

pub(crate) fn installed(m: NativeManager, names: &[String]) -> bool {
    names.iter().all(|n| match m {
        NativeManager::Apt => Command::new("dpkg-query")
            .args(["-W", "-f=${Status}", n])
            .output()
            .is_ok_and(|o| {
                o.status.success()
                    && String::from_utf8_lossy(&o.stdout).contains("install ok installed")
            }),
        NativeManager::Pacman => succeeds("pacman", &["-Q", n]),
        NativeManager::Apk => succeeds("apk", &["info", "-e", n]),
    })
}

pub(crate) fn install_args(m: NativeManager) -> (&'static str, Vec<OsString>) {
    let (p, args): (_, &[&str]) = match m {
        NativeManager::Apt => (
            "apt-get",
            &[
                "install",
                "-y",
                "--no-download",
                "--no-remove",
                "--no-install-recommends",
            ],
        ),
        NativeManager::Pacman => ("pacman", &["-U", "--noconfirm", "--needed"]),
        NativeManager::Apk => ("apk", &["add", "--no-network"]),
    };
    (p, args.iter().map(OsString::from).collect())
}

pub(crate) fn collect(app: &App, c: &mut Collect) -> Result<(Recipe, Vec<String>)> {
    let manager = match app.source {
        Source::Dpkg => NativeManager::Apt,
        Source::Pacman => NativeManager::Pacman,
        _ => NativeManager::Apk,
    };
    let distro = crate::platform::distro().ok_or_else(|| invalid("Unknown source distribution"))?;
    let rel = format!("installers/native-{}/packages", app.native_id);
    let dir = c.staging.join(&rel);
    crate::installers::fresh_dir(&dir)?;
    let work = dir
        .parent()
        .ok_or_else(|| invalid("Missing package directory"))?
        .join("solver");
    fs::create_dir_all(&work).map_err(|e| invalid(e.to_string()))?;
    (c.status)(format!(
        "Collecting {} with all dependencies",
        app.native_id
    ));
    match manager {
        NativeManager::Apt => {
            let status = work.join("status");
            fs::write(&status, "").map_err(|e| invalid(e.to_string()))?;
            run(
                Command::new("apt-get")
                    .args([
                        "install",
                        "--download-only",
                        "-y",
                        "--no-install-recommends",
                        "-o",
                        "Debug::NoLocking=true",
                        "-o",
                    ])
                    .arg(format!("Dir::State::status={}", status.display()))
                    .arg("-o")
                    .arg(format!("Dir::Cache::archives={}", dir.display()))
                    .arg(&app.native_id),
                c.cancel,
            )?;
        }
        NativeManager::Pacman => {
            let db = work.join("db");
            fs::create_dir_all(db.join("sync")).map_err(|e| invalid(e.to_string()))?;
            for entry in fs::read_dir("/var/lib/pacman/sync").map_err(|e| invalid(e.to_string()))? {
                let entry = entry.map_err(|e| invalid(e.to_string()))?;
                if entry.path().is_file() {
                    fs::copy(entry.path(), db.join("sync").join(entry.file_name()))
                        .map_err(|e| invalid(e.to_string()))?;
                }
            }
            let urls = run(
                Command::new("pacman")
                    .args(["-Sp", "--noconfirm", "--dbpath"])
                    .arg(&db)
                    .args(["--print-format", "%l", &app.native_id]),
                c.cancel,
            )?;
            for url in urls.lines().filter(|l| l.starts_with("https://")) {
                let name = url
                    .rsplit('/')
                    .next()
                    .ok_or_else(|| invalid("Bad package URL"))?;
                crate::paths::check_rel(name, true)?;
                crate::fetch::download(url, &dir.join(name), None, c.cancel)?;
                crate::fetch::download(
                    &format!("{url}.sig"),
                    &dir.join(format!("{name}.sig")),
                    None,
                    c.cancel,
                )?;
                run(
                    Command::new("pacman-key")
                        .arg("--verify")
                        .arg(dir.join(format!("{name}.sig")))
                        .arg(dir.join(name)),
                    c.cancel,
                )?;
            }
        }
        NativeManager::Apk => {
            run(
                Command::new("apk")
                    .args(["fetch", "--recursive", "--output"])
                    .arg(&dir)
                    .arg(&app.native_id),
                c.cancel,
            )?;
        }
    }
    let mut files = fs::read_dir(&dir)
        .map_err(|e| invalid(e.to_string()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && !p.to_string_lossy().ends_with(".sig")
                && p.file_name().is_some_and(|n| n != "lock")
        })
        .collect::<Vec<_>>();
    files.sort();
    let mut packages = Vec::new();
    for file in files {
        let filename = file
            .file_name()
            .ok_or_else(|| invalid("Missing package filename"))?
            .to_string_lossy();
        let name = match manager {
            NativeManager::Apt => run(
                Command::new("dpkg-deb")
                    .args(["-f"])
                    .arg(&file)
                    .arg("Package"),
                c.cancel,
            )?
            .trim()
            .to_string(),
            NativeManager::Pacman => {
                run(Command::new("pacman").args(["-Qp"]).arg(&file), c.cancel)?
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_string()
            }
            NativeManager::Apk => {
                run(Command::new("apk").arg("verify").arg(&file), c.cancel)?;
                apk_name(&filename)
                    .ok_or_else(|| invalid("Cannot identify APK package"))?
                    .to_string()
            }
        };
        if !crate::manifest::plain_name(&name) {
            return Err(invalid("Invalid package identity"));
        }
        packages.push(Package {
            name,
            file: stored(c, format!("{rel}/{filename}"))?,
        });
        if manager == NativeManager::Pacman {
            // Keep signatures adjacent to their packages for pacman's normal SigLevel policy.
            let sig = format!("{filename}.sig");
            packages.push(Package {
                name: "signature".into(),
                file: stored(c, format!("{rel}/{sig}"))?,
            });
        }
    }
    if !packages.iter().any(|p| p.name == app.native_id) {
        return Err(invalid(
            "Dependency collection did not include the application",
        ));
    }
    Ok((
        Recipe::NativeSet {
            manager,
            distro: distro.clone(),
            requested: vec![app.native_id.clone()],
            packages,
        },
        vec![format!(
            "Complete repository dependency closure for {distro}; package manager verification retained. Restore uses no network."
        )],
    ))
}

fn apk_name(filename: &str) -> Option<&str> {
    filename.rsplitn(3, '-').nth(2)
}

/// Inspect bounded RPM headers without extracting the payload or invoking a platform tool.
pub(crate) fn rpm_metadata(path: &std::path::Path) -> Result<(String, String)> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(path)
        .map_err(|e| invalid(e.to_string()))?
        .take(16 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| invalid(e.to_string()))?;
    let number = |at: usize| -> Result<usize> {
        let b: [u8; 4] = bytes
            .get(at..at + 4)
            .ok_or_else(|| invalid("Truncated RPM header"))?
            .try_into()
            .map_err(|_| invalid("Invalid RPM field"))?;
        Ok(u32::from_be_bytes(b) as usize)
    };
    if bytes.get(..4) != Some(&[0xed, 0xab, 0xee, 0xdb]) {
        return Err(invalid("Invalid RPM lead"));
    }
    let header = |at: usize| -> Result<(usize, usize, usize)> {
        if bytes.get(at..at + 4) != Some(&[0x8e, 0xad, 0xe8, 1]) {
            return Err(invalid("Invalid RPM header"));
        }
        let count = number(at + 8)?;
        let data = at
            .checked_add(16)
            .and_then(|n| n.checked_add(count.checked_mul(16)?))
            .ok_or_else(|| invalid("Oversized RPM index"))?;
        let end = data
            .checked_add(number(at + 12)?)
            .ok_or_else(|| invalid("Oversized RPM data"))?;
        if end > bytes.len() {
            return Err(invalid("Truncated or oversized RPM metadata"));
        }
        Ok((count, data, end))
    };
    let (_, _, sig_end) = header(96)?;
    let main = (sig_end + 7) & !7;
    let (count, data, end) = header(main)?;
    let field = |tag: usize| -> Result<String> {
        for i in 0..count {
            let at = main + 16 + i * 16;
            if number(at)? == tag {
                if number(at + 4)? != 6 || number(at + 12)? != 1 {
                    return Err(invalid("RPM identity must be a string"));
                }
                let offset = data
                    .checked_add(number(at + 8)?)
                    .ok_or_else(|| invalid("Invalid RPM offset"))?;
                let text = bytes
                    .get(offset..end)
                    .ok_or_else(|| invalid("Invalid RPM identity range"))?;
                let len = text
                    .iter()
                    .position(|b| *b == 0)
                    .ok_or_else(|| invalid("Unterminated RPM identity"))?;
                return String::from_utf8(text[..len].to_vec())
                    .map_err(|_| invalid("Invalid RPM identity encoding"));
            }
        }
        Err(invalid("RPM identity metadata missing"))
    };
    Ok((field(1000)?, field(1022)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rpm_headers_are_bounded_and_match_architecture() {
        let mut bytes = vec![0u8; 96];
        bytes[..4].copy_from_slice(&[0xed, 0xab, 0xee, 0xdb]);
        let header = |count: u32, size: u32| {
            let mut h = vec![0x8e, 0xad, 0xe8, 1, 0, 0, 0, 0];
            h.extend(count.to_be_bytes());
            h.extend(size.to_be_bytes());
            h
        };
        bytes.extend(header(0, 0));
        bytes.extend(header(2, 11));
        for (tag, offset) in [(1000u32, 0u32), (1022, 3)] {
            for value in [tag, 6, offset, 1] {
                bytes.extend(value.to_be_bytes());
            }
        }
        bytes.extend(b"gh\0aarch64\0");
        let temp = tempfile::NamedTempFile::new().expect("fixture");
        fs::write(temp.path(), &bytes).expect("fixture");
        assert_eq!(
            rpm_metadata(temp.path()).expect("header"),
            ("gh".into(), "aarch64".into())
        );
        bytes.truncate(120);
        fs::write(temp.path(), bytes).expect("fixture");
        assert!(rpm_metadata(temp.path()).is_err());
    }
    #[test]
    fn apk_names_keep_hyphens() {
        assert_eq!(apk_name("lib-foo-2.4-r0.apk"), Some("lib-foo"));
    }
    #[test]
    fn offline_commands_do_not_fetch() {
        assert!(
            install_args(NativeManager::Apt)
                .1
                .contains(&"--no-download".into())
        );
        assert!(
            install_args(NativeManager::Apk)
                .1
                .contains(&"--no-network".into())
        );
        assert_eq!(install_args(NativeManager::Pacman).1[0], "-U");
    }
}
