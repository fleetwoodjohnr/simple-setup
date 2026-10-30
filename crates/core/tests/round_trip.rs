//! End to end on fixtures: select two apps' settings and one folder → build → verify →
//! restore into a different home. Unselected content must never reach the snapshot.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use simple_setup_core::build::{self, BuildRequest, Engines, Progress};
use simple_setup_core::discovery::{App, Inventory, Kind, LocKind, Location, Origin, Source};
use simple_setup_core::installers::Elevation;
use simple_setup_core::manifest::{Manifest, Recipe, Status};
use simple_setup_core::platform::{Device, Platform};
use simple_setup_core::probe::SystemInfo;
use simple_setup_core::restore::{self, Conflict, Outcome, RestoreChoice, Target};
use simple_setup_core::selection::{AppChoice, FolderChoice, KnownFolder, Selection};
use simple_setup_core::snapshot;

/// A computer with no package managers and no internet: only the snapshot itself is used.
fn offline(platform: Platform) -> SystemInfo {
    SystemInfo {
        device: Device {
            hostname: "new-pc".into(),
            os_name: "Test OS".into(),
            platform,
        },
        distro: None,
        cpu: String::new(),
        cores: 1,
        memory_bytes: 0,
        free_bytes: None,
        tools: Vec::new(),
        flathub: false,
        elevated: false,
        online: false,
    }
}

fn write(p: &Path, s: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, s).unwrap();
}

fn app(id: &str, locs: &[(LocKind, PathBuf)]) -> App {
    let mut a: App = serde_json::from_value(serde_json::json!({
        "id": format!("rpm:{id}"), "name": id, "version": "1.0", "source": "rpm", "native_id": id,
        "kind": "desktop", "canonical": null, "repository": null, "system": false, "install_size": null,
        "path": null, "locations": [], "github": null, "confidence": "confirmed", "evidence": [], "user_fields": []
    }))
    .unwrap();
    a.locations = locs
        .iter()
        .map(|(k, p)| Location {
            kind: *k,
            path: p.clone(),
            origin: Origin::Detected,
        })
        .collect();
    let _ = (Source::Rpm, Kind::Desktop);
    a
}

fn all_files(root: &Path) -> Vec<String> {
    walkdir(root)
        .into_iter()
        .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
        .collect()
}

fn walkdir(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(root).unwrap().flatten() {
        let p = e.path();
        if e.file_type().unwrap().is_dir() {
            out.extend(walkdir(&p));
        } else {
            out.push(p);
        }
    }
    out
}

#[test]
fn selected_data_round_trip() {
    // Windows' temp folder (AppData/Local/Temp) is itself skipped as temporary files.
    let t = if cfg!(windows) {
        tempfile::tempdir_in(".")
    } else {
        tempfile::tempdir()
    }
    .unwrap();
    let src = t.path().join("src-home");
    write(
        &src.join(".config/app-a/settings.json"),
        "{\"theme\":\"dark\"}",
    );
    write(&src.join(".config/app-a/Cache/junk.bin"), "cache");
    write(
        &src.join(".local/share/app-a/db.sqlite"),
        "data not selected",
    );
    write(&src.join(".config/app-b/conf.ini"), "[b]\nx=1\n");
    write(&src.join(".config/app-c/never.txt"), "unselected app");
    write(&src.join("Documents/report.txt"), "quarterly report");
    write(&src.join("Documents/sub/notes.md"), "# notes");
    write(&src.join("Documents/Passwords.kdbx"), "secret");
    write(&src.join("Documents/skip-me/big.iso"), "excluded by user");
    // Windows cannot create a file named CON; on Linux/macOS the snapshot must still carry it.
    if cfg!(not(windows)) {
        write(&src.join("Documents/CON"), "windows-reserved name");
    }
    write(&src.join("Pictures/holiday.png"), "unselected folder");
    write(
        &src.join("Applications/Tool.AppImage"),
        "self-contained program",
    );
    #[cfg(unix)]
    std::os::unix::fs::symlink("report.txt", src.join("Documents/latest")).unwrap();

    let here = Platform::current().unwrap();
    let inv = Inventory {
        device: Device {
            hostname: "old-pc".into(),
            os_name: "Test OS".into(),
            platform: here,
        },
        scanned_at: "2026-01-01T00:00:00Z".into(),
        apps: vec![
            app(
                "app-a",
                &[
                    (LocKind::Config, src.join(".config/app-a")),
                    (LocKind::Data, src.join(".local/share/app-a")),
                ],
            ),
            App {
                // Installed from a local .rpm: no installer can be collected again.
                repository: Some("@commandline".into()),
                ..app("app-b", &[(LocKind::Config, src.join(".config/app-b"))])
            },
            app("app-c", &[(LocKind::Config, src.join(".config/app-c"))]),
            App {
                id: "app_image:tool".into(),
                name: "Tool".into(),
                source: Source::AppImage,
                path: Some(src.join("Applications/Tool.AppImage")),
                ..app("tool", &[])
            },
        ],
        hidden_packages: 0,
        notes: vec![],
    };
    let selection = Selection {
        apps: vec![
            AppChoice {
                id: "rpm:app-a".into(),
                installer: false,
                config: true,
                data: false,
                exclude: vec![],
            },
            AppChoice {
                id: "rpm:app-b".into(),
                installer: true,
                config: true,
                data: false,
                exclude: vec![],
            },
            AppChoice {
                id: "app_image:tool".into(),
                installer: true,
                config: false,
                data: false,
                exclude: vec![],
            },
        ],
        folders: vec![FolderChoice {
            path: src.join("Documents"),
            known: Some(KnownFolder::Documents),
            exclude: vec![src.join("Documents/skip-me")],
        }],
        include_secrets: vec![],
        targets: vec![here],
    };
    let engine = t.path().join("engine-bin");
    write(&engine, "#!/bin/sh\necho engine\n");
    let engines: Engines = [(here, engine)].into();
    let dest = t.path().join("usb");
    fs::create_dir_all(&dest).unwrap();

    // Interrupt the first build after one file, then resume it.
    let cancel = AtomicBool::new(false);
    let req = BuildRequest {
        inventory: &inv,
        selection: &selection,
        name: "My Move",
        destination: &dest,
        engines: &engines,
        home: &src,
        password: None,
        base: None,
    };
    let mut copying = false;
    let mut stop_after_first = |p: Progress| match p {
        Progress::Stage { label, .. } => copying = label.starts_with("Copying"),
        Progress::Copy { done_files, .. } if copying && done_files >= 1 => {
            cancel.store(true, Ordering::Relaxed);
        }
        _ => {}
    };
    assert!(matches!(
        build::build(&req, &mut stop_after_first, &cancel),
        Err(simple_setup_core::Error::Cancelled)
    ));
    assert!(build::staging_path(&dest, "My Move").is_dir());
    let cancel = AtomicBool::new(false);
    let report = build::build(&req, &mut |_| {}, &cancel).unwrap();
    assert!(report.resumed_files >= 1, "resume reused finished files");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    let snap = dest.join("My Move");
    assert!(!build::staging_path(&dest, "My Move").exists());

    // Only what was selected is in the bundle.
    let m = Manifest::read(&snap).unwrap();
    let mut rels: Vec<&str> = m.files.iter().map(|f| f.rel.as_str()).collect();
    rels.sort();
    let mut want = vec!["conf.ini", "report.txt", "settings.json", "sub/notes.md"];
    if cfg!(not(windows)) {
        want.insert(0, "CON");
    }
    assert_eq!(rels, want);
    let stored = all_files(&snap).join("\n");
    for never in [
        "/app-c/",
        "never.txt",
        "Pictures",
        "holiday",
        "db.sqlite",
        "junk.bin",
        "kdbx",
        "big.iso",
    ] {
        assert!(
            !stored.contains(never),
            "{never} leaked into the snapshot:\n{stored}"
        );
    }
    assert!(m.files.iter().all(|f| {
        f.stored
            .split('/')
            .all(simple_setup_core::paths::portable_component)
    }));
    assert_eq!(m.excluded.secrets, 1);
    assert!(m.excluded.transient >= 1);
    assert_eq!(m.excluded.by_user, 1);
    assert!(m.roots.iter().all(|r| r.original.starts_with("~/")));
    assert_eq!(
        m.selection, selection,
        "the manifest records exactly what was chosen"
    );
    #[cfg(unix)]
    assert_eq!(m.links.len(), 1);
    // The AppImage's program file is bundled as its installer; app-b's cannot be.
    assert_eq!(m.installers.len(), 1);
    let Recipe::Program { original, file } = &m.installers[0].recipe else {
        panic!("expected a program recipe: {:?}", m.installers);
    };
    assert_eq!(original, "~/Applications/Tool.AppImage");
    assert!(file.stored.starts_with("installers/") && snap.join(&file.stored).is_file());
    let ready = &m.readiness[0];
    assert!(ready.engine_included);
    assert!(
        !ready.offline_ready,
        "app-b's installer is not bundled, so the snapshot must not claim offline readiness"
    );
    assert!(
        snap.join("launchers/restore-linux.sh").is_file()
            || here.os != simple_setup_core::platform::OsFamily::Linux
    );
    assert!(
        snap.join(format!(
            "restore-engine/{here}/{}",
            build::engine_file(here)
        ))
        .is_file()
    );
    assert!(snapshot::verify(&snap, &mut |_| {}, &cancel).unwrap().ok);

    // Restore into another user's home with one conflicting and one identical file.
    let dst = t.path().join("dst-home");
    write(&dst.join("Docs/report.txt"), "a different report");
    write(&dst.join("Docs/sub/notes.md"), "# notes");
    let target = Target {
        device: Device {
            hostname: "new-pc".into(),
            os_name: "Test OS".into(),
            platform: here,
        },
        home: dst.clone(),
        known: HashMap::from([(KnownFolder::Documents, dst.join("Docs"))]),
        reports: Some(t.path().join("reports")),
        distro: None,
        elevation: Elevation::None,
        key: None,
        info: offline(here),
        online: false,
        allow_source: false,
        downloads: None,
    };
    let plan = restore::plan(&snap, &target).unwrap();
    let by_label = |s: &str| plan.items.iter().find(|i| i.label.contains(s)).unwrap();
    assert_eq!(by_label("app-b — installer").status, Status::Manual);
    let tool = by_label("Tool — installer");
    assert_eq!((tool.status, tool.admin), (Status::Ready, false));
    assert_eq!(by_label("app-a — settings").status, Status::Ready);
    let docs = by_label("~/Documents");
    assert_eq!((docs.conflicts, docs.identical), (1, 1));
    assert_eq!(docs.destination.as_deref(), Some("~/Docs"));

    let all: Vec<String> = plan.items.iter().map(|i| i.id.clone()).collect();
    let r = restore::execute(
        &snap,
        &target,
        &RestoreChoice {
            items: all.clone(),
            conflict: Conflict::Skip,
        },
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dst.join(".config/app-a/settings.json")).unwrap(),
        "{\"theme\":\"dark\"}"
    );
    assert_eq!(
        fs::read_to_string(dst.join("Docs/report.txt")).unwrap(),
        "a different report",
        "never overwrite"
    );
    assert!(!dst.join(".config/app-c").exists() && !dst.join("Pictures").exists());
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(dst.join("Docs/latest")).unwrap(),
        Path::new("report.txt")
    );
    let docs_r = r.items.iter().find(|i| i.label == "~/Documents").unwrap();
    assert_eq!(
        (docs_r.outcome, docs_r.skipped_conflicts, docs_r.identical),
        (Outcome::Restored, 1, 1)
    );
    assert!(r.items.iter().any(|i| i.outcome == Outcome::Manual));
    let tool_r = r
        .items
        .iter()
        .find(|i| i.label == "Tool — installer")
        .unwrap();
    assert_eq!(tool_r.outcome, Outcome::Installed, "{}", tool_r.message);
    let restored_tool = dst.join("Applications/Tool.AppImage");
    assert_eq!(
        fs::read_to_string(&restored_tool).unwrap(),
        "self-contained program"
    );
    #[cfg(unix)]
    assert!(
        std::os::unix::fs::PermissionsExt::mode(
            &fs::metadata(&restored_tool).unwrap().permissions()
        ) & 0o100
            != 0
    );
    assert!(r.report_file.as_ref().is_some_and(|f| f.is_file()));

    // Keep-both restores next to the existing file.
    let r2 = restore::execute(
        &snap,
        &target,
        &RestoreChoice {
            items: all.clone(),
            conflict: Conflict::KeepBoth,
        },
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert!(r2.report_file.as_ref().is_some_and(|f| f.is_file()));
    assert_ne!(r2.report_file, r.report_file, "earlier report kept");
    assert_eq!(
        fs::read_to_string(dst.join("Docs/report (restored).txt")).unwrap(),
        "quarterly report"
    );

    // Tampering is detected by verify and blocks restoring the damaged file.
    let settings = m.files.iter().find(|f| f.rel == "settings.json").unwrap();
    fs::write(snap.join(&settings.stored), "{\"theme\":\"evil\"}").unwrap();
    let v = snapshot::verify(&snap, &mut |_| {}, &cancel).unwrap();
    assert!(!v.ok && v.corrupt.len() == 1);
    // A damaged installer is refused before anything is installed.
    fs::write(snap.join(&file.stored), "tampered").unwrap();
    let dst2 = t.path().join("dst2");
    let target2 = Target {
        home: dst2.clone(),
        known: HashMap::new(),
        reports: None,
        ..target
    };
    let r2 = restore::execute(
        &snap,
        &target2,
        &RestoreChoice {
            items: all,
            conflict: Conflict::Skip,
        },
        &mut |_| {},
        &cancel,
    )
    .unwrap();
    assert!(!dst2.join(".config/app-a/settings.json").exists());
    assert!(!dst2.join("Applications/Tool.AppImage").exists());
    let tool_r2 = r2
        .items
        .iter()
        .find(|i| i.label == "Tool — installer")
        .unwrap();
    assert!(
        tool_r2.outcome == Outcome::Failed && tool_r2.message.contains("damaged"),
        "{}",
        tool_r2.message
    );
    assert!(
        r2.items
            .iter()
            .any(|i| i.outcome == Outcome::Failed && i.failed[0].error.contains("damaged"))
    );
    assert_eq!(
        fs::read_to_string(dst2.join("Documents/report.txt")).unwrap(),
        "quarterly report"
    );

    // Building again under the same name never overwrites.
    assert!(build::build(&req, &mut |_| {}, &cancel).is_err());
}

#[test]
fn nothing_selected_is_refused() {
    let here = Platform::current().unwrap();
    let inv = Inventory {
        device: Device {
            hostname: "x".into(),
            os_name: "x".into(),
            platform: here,
        },
        scanned_at: String::new(),
        apps: vec![],
        hidden_packages: 0,
        notes: vec![],
    };
    let sel = Selection {
        targets: vec![here],
        ..Default::default()
    };
    assert!(build::plan(&inv, &sel, Path::new("/"), None, &Engines::new()).is_err());
}

#[test]
fn hostile_manifest_is_rejected() {
    let t = tempfile::tempdir().unwrap();
    let here = Platform::current().unwrap();
    let mut m: serde_json::Value = serde_json::json!({
        "schema_version": 1, "id": "x", "name": "x", "created_at": "", "generator": "",
        "source": {"hostname": "h", "os_name": "o", "platform": here},
        "targets": [here], "selection": {"apps": [], "folders": [], "targets": [here]},
        "apps": [], "roots": [{"id": "r0", "kind": "folder", "app": null, "known": null, "original": "~/x", "dir": "personal-files/x", "is_file": false}],
        "files": [{"root": "r0", "rel": "../../.bashrc", "stored": "personal-files/x/a", "size": 1,
                   "sha256": "0000000000000000000000000000000000000000000000000000000000000000", "mtime": null, "mode": null}],
        "links": [], "excluded": {"secrets": 0, "transient": 0, "by_user": 0, "unreadable": 0, "special": 0, "secret_examples": [], "unreadable_examples": []},
        "coverage": [], "readiness": [], "warnings": [], "total_bytes": 1
    });
    fs::write(t.path().join("simple-setup.json"), m.to_string()).unwrap();
    assert!(Manifest::read(t.path()).is_err(), "parent traversal in rel");
    m["files"][0]["rel"] = "a".into();
    m["files"][0]["stored"] = "../outside".into();
    fs::write(t.path().join("simple-setup.json"), m.to_string()).unwrap();
    assert!(
        Manifest::read(t.path()).is_err(),
        "stored path escaping the bundle"
    );
    m["files"][0]["stored"] = "personal-files/x/a".into();
    m["roots"][0]["original"] = "~/../../etc".into();
    fs::write(t.path().join("simple-setup.json"), m.to_string()).unwrap();
    assert!(Manifest::read(t.path()).is_err(), "original escaping home");
    m["roots"][0]["original"] = "~/x".into();
    m["install_command"] = "rm -rf /".into();
    fs::write(t.path().join("simple-setup.json"), m.to_string()).unwrap();
    assert!(
        Manifest::read(t.path()).is_err(),
        "unknown fields are refused"
    );
}

/// Settings selected on Linux and restored on Windows go through the app's adapter: portable
/// files land in the Windows folder, Git's helper lines are disabled, files tied to the old
/// computer stay in the snapshot, and apps without an adapter are skipped.
#[test]
fn settings_move_to_another_os() {
    let t = if cfg!(windows) {
        tempfile::tempdir_in(".")
    } else {
        tempfile::tempdir()
    }
    .unwrap();
    let src = t.path().join("src-home");
    let code = src.join(".config/Code/User");
    write(&code.join("settings.json"), "{\"editor.fontSize\": 14}");
    write(&code.join("snippets/rust.json"), "{}");
    write(&code.join("keybindings.json"), "[{\"key\": \"ctrl+k\"}]");
    write(&code.join("globalStorage/state.vscdb"), "machine state");
    write(
        &src.join(".gitconfig"),
        "[user]\n\tname = Ada\n[credential]\n\thelper = libsecret\n",
    );
    let ff = src.join(".mozilla/firefox");
    write(
        &ff.join("profiles.ini"),
        "[Install4F96D1932A9F858E]\nDefault=abc.default\n\n[Profile0]\nName=default\nIsRelative=1\nPath=abc.default\nDefault=1\n",
    );
    write(&ff.join("abc.default/prefs.js"), "user_pref(\"a\", 1);");
    write(
        &ff.join("abc.default/compatibility.ini"),
        "[Compatibility]\nLastOSABI=Linux_x86_64-gcc3\n",
    );
    write(&src.join(".config/mytool/conf"), "no adapter");

    let linux = Platform {
        os: simple_setup_core::platform::OsFamily::Linux,
        arch: simple_setup_core::platform::Arch::X86_64,
    };
    let windows = Platform {
        os: simple_setup_core::platform::OsFamily::Windows,
        ..linux
    };
    let with = |id: &str, canonical: &str, kind, path: PathBuf| App {
        canonical: (!canonical.is_empty()).then(|| canonical.to_string()),
        ..app(id, &[(kind, path)])
    };
    let inv = Inventory {
        device: Device {
            hostname: "old-pc".into(),
            os_name: "Linux".into(),
            platform: linux,
        },
        scanned_at: "2026-01-01T00:00:00Z".into(),
        apps: vec![
            with("code", "vscode", LocKind::Config, code.clone()),
            with("git", "git", LocKind::Config, src.join(".gitconfig")),
            with("firefox", "firefox", LocKind::Data, ff.clone()),
            with("mytool", "", LocKind::Config, src.join(".config/mytool")),
        ],
        hidden_packages: 0,
        notes: vec![],
    };
    let choice = |id: &str| AppChoice {
        id: format!("rpm:{id}"),
        installer: false,
        config: true,
        data: true,
        exclude: vec![],
    };
    let selection = Selection {
        apps: ["code", "git", "firefox", "mytool"].map(choice).to_vec(),
        folders: vec![],
        include_secrets: vec![],
        targets: vec![windows],
    };
    let dest = t.path().join("usb");
    fs::create_dir_all(&dest).unwrap();
    let req = BuildRequest {
        inventory: &inv,
        selection: &selection,
        name: "To Windows",
        destination: &dest,
        engines: &Engines::new(),
        home: &src,
        password: None,
        base: None,
    };
    build::build(&req, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    let snap = dest.join("To Windows");
    let m = Manifest::read(&snap).unwrap();
    use simple_setup_core::manifest::Part;
    let cov = |app: &str, part| {
        m.coverage
            .iter()
            .find(|c| c.app == format!("rpm:{app}") && c.part == part)
            .unwrap()
            .status
    };
    assert_eq!(
        [
            cov("code", Part::Config),
            cov("git", Part::Config),
            cov("firefox", Part::Data)
        ],
        [Status::Ready; 3]
    );
    assert_eq!(cov("mytool", Part::Config), Status::Unsupported);

    let dst = t.path().join("win-home");
    let target = Target {
        device: Device {
            hostname: "new-pc".into(),
            os_name: "Windows 11".into(),
            platform: windows,
        },
        home: dst.clone(),
        known: HashMap::new(),
        reports: None,
        distro: None,
        elevation: Elevation::None,
        key: None,
        info: offline(windows),
        online: false,
        allow_source: false,
        downloads: None,
    };
    let plan = restore::plan(&snap, &target).unwrap();
    let code_item = plan
        .items
        .iter()
        .find(|i| i.label.starts_with("code"))
        .unwrap();
    assert_eq!(
        code_item.destination.as_deref(),
        Some("~/AppData/Roaming/Code/User")
    );
    assert_eq!(code_item.kept, 2, "{}", code_item.reason);
    let mytool = plan
        .items
        .iter()
        .find(|i| i.label.starts_with("mytool"))
        .unwrap();
    assert_eq!(mytool.status, Status::Unsupported);

    let all: Vec<String> = plan.items.iter().map(|i| i.id.clone()).collect();
    let run = || {
        restore::execute(
            &snap,
            &target,
            &RestoreChoice {
                items: all.clone(),
                conflict: Conflict::Skip,
            },
            &mut |_| {},
            &AtomicBool::new(false),
        )
        .unwrap()
    };
    let r = run();
    let win_code = dst.join("AppData/Roaming/Code/User");
    assert_eq!(
        fs::read_to_string(win_code.join("settings.json")).unwrap(),
        "{\"editor.fontSize\": 14}"
    );
    assert!(win_code.join("snippets/rust.json").is_file());
    assert!(
        !win_code.join("keybindings.json").exists() && !win_code.join("globalStorage").exists()
    );
    let gitconfig = fs::read_to_string(dst.join(".gitconfig")).unwrap();
    assert!(
        gitconfig.contains("\tname = Ada")
            && gitconfig.contains("# [Simple Setup] not valid on windows: helper = libsecret")
    );
    let win_ff = dst.join("AppData/Roaming/Mozilla/Firefox");
    assert!(
        !fs::read_to_string(win_ff.join("profiles.ini"))
            .unwrap()
            .contains("[Install")
    );
    assert!(win_ff.join("abc.default/prefs.js").is_file());
    assert!(!win_ff.join("abc.default/compatibility.ini").exists());
    assert!(
        !dst.join(".config").exists(),
        "nothing lands in Linux folders"
    );
    assert!(r.items.iter().all(|i| i.failed.is_empty()));

    // Again: converted files are recognised as already restored.
    let again = run();
    let ok: Vec<(u64, u64)> = again
        .items
        .iter()
        .map(|i| (i.restored, i.skipped_conflicts))
        .collect();
    assert!(
        ok.iter()
            .all(|&(restored, skipped)| restored == 0 && skipped == 0),
        "{ok:?}"
    );
}

/// A snapshot sent over the network: a wrong pairing code gets nothing, an interrupted
/// transfer resumes from the partial file, and the received copy verifies.
#[test]
fn network_transfer_resumes() {
    use simple_setup_core::{snapshot, transfer};
    let t = if cfg!(windows) {
        tempfile::tempdir_in(".")
    } else {
        tempfile::tempdir()
    }
    .unwrap();
    let src = t.path().join("src-home");
    let big: String = (0..150_000).map(|i| format!("{i:09}\n")).collect();
    write(&src.join("Documents/big.txt"), &big);
    write(&src.join("Documents/small.txt"), "hello");
    let here = Platform::current().unwrap();
    let inv = Inventory {
        device: Device {
            hostname: "old-pc".into(),
            os_name: "Test OS".into(),
            platform: here,
        },
        scanned_at: "2026-01-01T00:00:00Z".into(),
        apps: vec![],
        hidden_packages: 0,
        notes: vec![],
    };
    let selection = Selection {
        apps: vec![],
        folders: vec![FolderChoice {
            path: src.join("Documents"),
            known: Some(KnownFolder::Documents),
            exclude: vec![],
        }],
        include_secrets: vec![],
        targets: vec![here],
    };
    let out = t.path().join("out");
    fs::create_dir_all(&out).unwrap();
    let req = BuildRequest {
        inventory: &inv,
        selection: &selection,
        name: "Over the air",
        destination: &out,
        engines: &Engines::new(),
        home: &src,
        password: None,
        base: None,
    };
    build::build(&req, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    let snap = out.join("Over the air");

    let inbox = t.path().join("inbox");
    fs::create_dir_all(&inbox).unwrap();
    let receiver = transfer::Receiver::start(false).unwrap();
    let (code, port) = (receiver.code.clone(), receiver.port);
    let notes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = notes.clone();
    let inbox2 = inbox.clone();
    let rx = std::thread::spawn(move || {
        let mut offers = 0;
        let r = receiver.run(
            &inbox2,
            &mut |_, offer| {
                offers += 1;
                offer.name == "Over the air"
            },
            &mut |p| {
                if let Progress::Note { message } = p {
                    seen.lock().unwrap().push(message);
                }
            },
            &AtomicBool::new(false),
        );
        (r, offers)
    });
    let addr: std::net::SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let wrong = if code == "1234-5678" {
        "8765-4321"
    } else {
        "1234-5678"
    };
    let err = transfer::send(
        &snap,
        addr,
        wrong,
        &mut |_| true,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap_err();
    assert!(err.to_string().contains("pairing code"), "{err}");

    // Stop after the first chunk of the big file.
    let stop = AtomicBool::new(false);
    let r = transfer::send(
        &snap,
        addr,
        &code,
        &mut |_| true,
        &mut |p| {
            if let Progress::Copy { current, .. } = p
                && current.ends_with("big.txt")
            {
                stop.store(true, Ordering::Relaxed);
            }
        },
        &stop,
    );
    assert!(matches!(r, Err(simple_setup_core::Error::Cancelled)));

    let report = transfer::send(
        &snap,
        addr,
        &code,
        &mut |_| true,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(report.resumed_bytes > 0, "{report:?}");
    let (received, offers) = rx.join().unwrap();
    let received = received.unwrap();
    assert_eq!(offers, 1, "asked once, not again on resume");
    assert_eq!(received, inbox.join("Over the air"));
    assert_eq!(report.saved_as, received.display().to_string());
    let v = snapshot::verify(&received, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    assert!(v.ok, "{v:?}");
    assert!(received.join("reports/build-report.txt").is_file());
    let m = Manifest::read(&received).unwrap();
    let f = m.files.iter().find(|f| f.rel == "big.txt").unwrap();
    assert_eq!(fs::read_to_string(received.join(&f.stored)).unwrap(), big);
    assert!(!inbox.join(".Over the air.receiving").exists());
    let notes = notes.lock().unwrap();
    assert!(
        notes.iter().any(|n| n.contains("could not pair")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|n| n.contains("continue where it stopped")),
        "{notes:?}"
    );
}

/// An encrypted snapshot keeps names and contents unreadable without the password, still
/// verifies without it, and restores with the password or the recovery key. An update of it
/// takes unchanged files over from it instead of copying them again.
#[test]
fn encrypted_snapshot_and_incremental_update() {
    use simple_setup_core::snapshot;
    let t = if cfg!(windows) {
        tempfile::tempdir_in(".")
    } else {
        tempfile::tempdir()
    }
    .unwrap();
    let src = t.path().join("src-home");
    write(
        &src.join("Documents/diary-secret-name.txt"),
        "dear diary: plaintext-marker",
    );
    write(&src.join("Documents/unchanged.txt"), "same as before");
    let here = Platform::current().unwrap();
    let inv = Inventory {
        device: Device {
            hostname: "old-pc".into(),
            os_name: "Test OS".into(),
            platform: here,
        },
        scanned_at: "2026-01-01T00:00:00Z".into(),
        apps: vec![],
        hidden_packages: 0,
        notes: vec![],
    };
    let selection = Selection {
        apps: vec![],
        folders: vec![FolderChoice {
            path: src.join("Documents"),
            known: Some(KnownFolder::Documents),
            exclude: vec![],
        }],
        include_secrets: vec![],
        targets: vec![here],
    };
    let usb = t.path().join("usb");
    fs::create_dir_all(&usb).unwrap();
    let build_one = |name: &str, password, base| {
        let req = BuildRequest {
            inventory: &inv,
            selection: &selection,
            name,
            destination: &usb,
            engines: &Engines::new(),
            home: &src,
            password,
            base,
        };
        build::build(&req, &mut |_| {}, &AtomicBool::new(false)).unwrap()
    };
    let report = build_one("Locked", Some("correct horse battery"), None);
    let recovery = report.recovery_key.clone().unwrap();
    let snap = usb.join("Locked");

    // Nothing readable on disk: no file names, no contents, no recovery key.
    for p in walkdir(&snap) {
        let bytes = fs::read(&p).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains("plaintext-marker") && !text.contains("diary-secret-name"),
            "{}",
            p.display()
        );
        assert!(!text.contains(&recovery), "{}", p.display());
    }
    assert!(
        snapshot::verify(&snap, &mut |_| {}, &AtomicBool::new(false))
            .unwrap()
            .ok
    );
    let summary = snapshot::summary(&snap).unwrap();
    assert!(summary.encrypted && summary.folders.is_empty());

    let dst = t.path().join("new-home");
    let mut target = Target {
        device: Device {
            hostname: "new-pc".into(),
            os_name: "Test OS".into(),
            platform: here,
        },
        home: dst.clone(),
        known: HashMap::from([(KnownFolder::Documents, dst.join("Docs"))]),
        reports: None,
        distro: None,
        elevation: Elevation::None,
        key: None,
        info: offline(here),
        online: false,
        allow_source: false,
        downloads: None,
    };
    assert!(matches!(
        restore::plan(&snap, &target),
        Err(simple_setup_core::Error::Locked)
    ));
    let outer = Manifest::read(&snap).unwrap();
    assert!(outer.unlock(&snap, Some("wrong horse battery")).is_err());
    target.key = outer.unlock(&snap, Some(&recovery)).unwrap();
    target.key = outer.unlock(&snap, Some("correct horse battery")).unwrap();
    let plan = restore::plan(&snap, &target).unwrap();
    let all: Vec<String> = plan.items.iter().map(|i| i.id.clone()).collect();
    let choice = RestoreChoice {
        items: all,
        conflict: Conflict::Skip,
    };
    restore::execute(
        &snap,
        &target,
        &choice,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        fs::read_to_string(dst.join("Docs/diary-secret-name.txt")).unwrap(),
        "dear diary: plaintext-marker"
    );

    // An update: one file changed, one new, one unchanged (taken over, not copied).
    std::thread::sleep(std::time::Duration::from_millis(1100));
    write(&src.join("Documents/diary-secret-name.txt"), "a new entry");
    write(&src.join("Documents/added.txt"), "new file");
    let upd = build_one("Locked update", Some("correct horse battery"), Some(&snap));
    assert_eq!(upd.reused_files, 1, "{upd:?}");
    let new = usb.join("Locked update");
    let m = Manifest::open(&new, target.key.as_ref()).unwrap();
    assert_eq!(m.base.as_ref().unwrap().name, "Locked");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let stored = |m: &Manifest, rel: &str| {
            m.files
                .iter()
                .find(|f| f.rel == rel)
                .unwrap()
                .stored
                .clone()
        };
        let old_m = Manifest::open(&snap, target.key.as_ref()).unwrap();
        let ino = |p: PathBuf| fs::metadata(p).unwrap().ino();
        assert_eq!(
            ino(snap.join(stored(&old_m, "unchanged.txt"))),
            ino(new.join(stored(&m, "unchanged.txt"))),
            "hard link"
        );
    }
    assert!(
        snapshot::verify(&new, &mut |_| {}, &AtomicBool::new(false))
            .unwrap()
            .ok
    );
    let dst2 = t.path().join("newer-home");
    let target2 = Target {
        home: dst2.clone(),
        known: HashMap::new(),
        ..target
    };
    let plan = restore::plan(&new, &target2).unwrap();
    let choice = RestoreChoice {
        items: plan.items.iter().map(|i| i.id.clone()).collect(),
        conflict: Conflict::Skip,
    };
    restore::execute(
        &new,
        &target2,
        &choice,
        &mut |_| {},
        &AtomicBool::new(false),
    )
    .unwrap();
    for (f, want) in [
        ("diary-secret-name.txt", "a new entry"),
        ("unchanged.txt", "same as before"),
        ("added.txt", "new file"),
    ] {
        assert_eq!(
            fs::read_to_string(dst2.join("Documents").join(f)).unwrap(),
            want
        );
    }

    // Updating an encrypted snapshot needs its password.
    let req = BuildRequest {
        inventory: &inv,
        selection: &selection,
        name: "No password",
        destination: &usb,
        engines: &Engines::new(),
        home: &src,
        password: None,
        base: Some(&snap),
    };
    assert!(
        build::build(&req, &mut |_| {}, &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("password")
    );
}

/// An app chosen for reinstall on Linux is planned on Windows, macOS and another Linux with
/// that computer's own package managers or verified downloads; offline, the same items wait
/// for internet; a cancelled restore still writes its report and marks what was not started.
#[test]
fn apps_install_online_on_any_system() {
    use simple_setup_core::platform::{Arch, OsFamily};
    use simple_setup_core::resolve::Channel;
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src-home");
    write(&src.join("Documents/a.txt"), "a");
    let linux = Platform {
        os: OsFamily::Linux,
        arch: Arch::X86_64,
    };
    let firefox = App {
        id: "manual:firefox".into(),
        source: Source::Manual,
        canonical: Some("firefox".into()),
        ..app("firefox", &[])
    };
    let inv = Inventory {
        device: Device {
            hostname: "old-pc".into(),
            os_name: "Linux".into(),
            platform: linux,
        },
        scanned_at: "2026-01-01T00:00:00Z".into(),
        apps: vec![firefox],
        hidden_packages: 0,
        notes: vec![],
    };
    let selection = Selection {
        apps: vec![AppChoice {
            id: "manual:firefox".into(),
            installer: true,
            config: false,
            data: false,
            exclude: vec![],
        }],
        folders: vec![FolderChoice {
            path: src.join("Documents"),
            known: None,
            exclude: vec![],
        }],
        include_secrets: vec![],
        targets: vec![linux],
    };
    let dest = t.path().join("usb");
    fs::create_dir_all(&dest).unwrap();
    let req = BuildRequest {
        inventory: &inv,
        selection: &selection,
        name: "Anywhere",
        destination: &dest,
        engines: &Engines::new(),
        home: &src,
        password: None,
        base: None,
    };
    build::build(&req, &mut |_| {}, &AtomicBool::new(false)).unwrap();
    let snap = dest.join("Anywhere");

    let on = |os, tools: &[&str], online: bool| {
        let platform = Platform { os, ..linux };
        let mut info = offline(platform);
        info.tools = tools.iter().map(|s| s.to_string()).collect();
        info.online = online;
        let target = Target {
            device: info.device.clone(),
            home: t.path().join("new-home"),
            known: HashMap::new(),
            reports: Some(t.path().join("reports")),
            distro: None,
            elevation: Elevation::Pkexec,
            key: None,
            info,
            online,
            allow_source: true,
            downloads: Some(t.path().join("downloads")),
        };
        let plan = restore::plan(&snap, &target).unwrap();
        let item = plan
            .items
            .iter()
            .find(|i| i.id == "install:manual:firefox")
            .unwrap()
            .clone();
        (target, plan, item)
    };
    let (_, plan, win) = on(OsFamily::Windows, &["winget", "curl"], true);
    assert_eq!(win.status, Status::Ready, "{}", win.reason);
    assert_eq!(win.channel, Some(Channel::Online));
    assert_eq!(win.method.as_deref(), Some("winget (Mozilla.Firefox)"));
    assert!(
        win.reason.contains("If that fails: a verified download"),
        "{}",
        win.reason
    );
    assert!(!plan.offline_possible);
    // Without curl nothing is downloaded; winget still works.
    let (_, _, no_curl) = on(OsFamily::Windows, &["winget"], true);
    assert!(
        !no_curl.reason.contains("verified download"),
        "{}",
        no_curl.reason
    );

    let (_, _, mac) = on(OsFamily::Macos, &["curl"], true);
    assert_eq!(mac.status, Status::Ready);
    assert!(mac.method.unwrap().contains("Homebrew cask catalog"));

    let (_, _, lin) = on(OsFamily::Linux, &["dnf"], true);
    assert_eq!(lin.method.as_deref(), Some("DNF package firefox"));
    assert!(
        lin.admin,
        "system packages ask for the administrator password"
    );

    let (target, plan, off) = on(OsFamily::Windows, &["winget"], false);
    assert_eq!(off.status, Status::Manual);
    assert!(off.reason.contains("Needs internet"), "{}", off.reason);
    assert!(!plan.online);

    // Cancelled before anything ran: nothing is touched, the report says so and is saved.
    let report = restore::execute(
        &snap,
        &target,
        &RestoreChoice {
            items: plan.items.iter().map(|i| i.id.clone()).collect(),
            conflict: Conflict::Skip,
        },
        &mut |_| {},
        &AtomicBool::new(true),
    )
    .unwrap();
    assert!(report.cancelled);
    assert!(report.report_file.as_ref().is_some_and(|f| f.is_file()));
    assert!(
        report
            .items
            .iter()
            .all(|r| r.outcome == Outcome::Skipped && r.message.contains("cancelled"))
    );
    assert!(!t.path().join("new-home/Documents/a.txt").exists());
}
