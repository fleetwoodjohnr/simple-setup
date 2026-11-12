//! `simple-setup` command-line interface. Also shipped inside every snapshot as the restore
//! engine, so it must work on a freshly installed computer with no other tools.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clap::{Parser, Subcommand};
use simple_setup_core::build::{self, BuildRequest, ItemState, Planning, Progress};
use simple_setup_core::compat::Verdict;
use simple_setup_core::discovery::{self, Inventory, ScanOptions};
use simple_setup_core::distros::{self, UserStore};
use simple_setup_core::installers::Elevation;
use simple_setup_core::manifest::{self, Status};
use simple_setup_core::platform;
use simple_setup_core::probe::{self, SystemInfo};
use simple_setup_core::resolve::Prefs;
use simple_setup_core::restore::{self, Conflict, ItemKind, Outcome, RestoreChoice, Target};
use simple_setup_core::selection::Selection;
use simple_setup_core::snapshot;
use simple_setup_core::store::{SnapshotRef, Store};
use simple_setup_core::transfer;

#[derive(Parser)]
#[command(
    name = "simple-setup",
    version,
    about = "Move chosen apps, settings and files to another computer."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List installed applications (read-only).
    Scan {
        #[arg(long)]
        json: bool,
        /// Also look for source checkouts and portable apps in this folder (repeatable).
        #[arg(long = "source-dir")]
        source_dirs: Vec<PathBuf>,
    },
    /// Print the JSON Schema of simple-setup.json.
    Schema,
    /// List external drives and mounted volumes.
    Volumes {
        #[arg(long)]
        json: bool,
    },
    #[command(subcommand)]
    Snapshot(SnapshotCmd),
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Restore a snapshot onto this computer. Shows the plan and asks before changing anything.
    /// Apps the snapshot has no installer for are installed with this computer's package
    /// managers or verified downloads (unless --offline).
    Restore {
        snapshot: PathBuf,
        #[command(flatten)]
        opts: RestoreArgs,
    },
    /// Send a snapshot to another computer on this network that is receiving.
    Send {
        snapshot: PathBuf,
        /// The address the receiving computer shows, e.g. 192.168.1.20:41234. Default: find
        /// receiving computers nearby.
        #[arg(long)]
        to: Option<String>,
        /// The pairing code the receiving computer shows (otherwise you are asked).
        #[arg(long)]
        code: Option<String>,
        /// Do not ask before sending.
        #[arg(long)]
        yes: bool,
    },
    /// Receive a snapshot from another computer on this network. Listens only until one
    /// snapshot has arrived.
    Receive {
        /// Folder to save it in. Default: "Simple Setup Snapshots" in your home folder.
        #[arg(long)]
        dest: Option<PathBuf>,
        /// Accept the offered snapshot without asking.
        #[arg(long)]
        yes: bool,
        /// Do not announce this computer on the network; the sender then needs the address.
        #[arg(long)]
        no_announce: bool,
        /// Restore the snapshot as soon as it has arrived and been checked (every item; with
        /// --yes, without asking).
        #[arg(long)]
        restore: bool,
    },
    /// List computers on this network that are receiving right now.
    Peers {
        #[arg(long)]
        json: bool,
    },
    /// Ask a receiving computer what it is (system, distribution, package managers), to
    /// prepare a snapshot for it. Nothing is sent.
    Probe {
        /// Its address, e.g. 192.168.1.20:41234. Default: find receiving computers nearby.
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        code: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show what this computer is: system, distribution profile, package managers, portable
    /// formats (read-only, no network).
    System {
        #[arg(long)]
        json: bool,
    },
    /// Linux distribution profiles: list, inspect, add your own, check for official updates.
    #[command(subcommand)]
    Distros(DistrosCmd),
    /// Check how a snapshot's apps would be installed: on this computer, or on another kind of
    /// computer (--as). Changes nothing.
    Analyze {
        snapshot: PathBuf,
        /// A distribution profile and optional release, e.g. linuxmint:22 or fedora:44; with
        /// --os windows or macos, not needed.
        #[arg(long = "as")]
        as_profile: Option<String>,
        /// System of that computer (default linux when --as is given).
        #[arg(long)]
        os: Option<String>,
        /// Processor of that computer: x86_64 or aarch64 (default: this computer's).
        #[arg(long)]
        arch: Option<String>,
        #[arg(long)]
        json: bool,
        /// For an encrypted snapshot: file whose first line is the password or recovery key.
        #[arg(long)]
        password_file: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum DistrosCmd {
    /// Every profile, with where it comes from and whether it is used.
    List {
        #[arg(long)]
        json: bool,
    },
    /// One profile with everything it inherits applied.
    Show { id: String },
    /// Check a profile file (JSON or YAML) without saving it, and test it against this computer.
    Validate { file: PathBuf },
    /// Add or replace profiles from a file (JSON, or YAML by extension).
    Import {
        file: PathBuf,
        /// Also trust the custom commands in it (they are shown first).
        #[arg(long)]
        trust: bool,
    },
    /// Write profiles to a JSON file to share or edit (default: print).
    Export {
        ids: Vec<String>,
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Use a profile again.
    Enable { id: String },
    /// Stop using a profile (built-in ones too).
    Disable { id: String },
    /// Remove one of your profiles.
    Remove { id: String },
    /// Trust (or with --revoke, stop trusting) the custom commands of one of your profiles.
    Trust {
        id: String,
        #[arg(long)]
        revoke: bool,
    },
    /// Download newer official profiles from Simple Setup's GitHub releases (checked against
    /// the release's SHA-256 list).
    Update,
}

#[derive(clap::Args, Default)]
struct RestoreArgs {
    /// Show what would happen and stop.
    #[arg(long)]
    dry_run: bool,
    /// Restore only these plan item IDs (repeatable). Default: every item.
    #[arg(long = "only")]
    only: Vec<String>,
    /// When a different file exists, restore next to it as "name (restored)" instead of skipping.
    #[arg(long)]
    keep_both: bool,
    /// Do not ask for confirmation.
    #[arg(long)]
    yes: bool,
    #[arg(long)]
    json: bool,
    /// For an encrypted snapshot: file whose first line is the password or recovery key
    /// (otherwise you are asked).
    #[arg(long)]
    password_file: Option<PathBuf>,
    /// Use only what is in the snapshot: no downloads, no package-manager installs.
    #[arg(long)]
    offline: bool,
    /// Never build apps from their source code.
    #[arg(long)]
    no_source_builds: bool,
}

#[derive(Subcommand)]
enum SnapshotCmd {
    /// Show what a selection would copy, its size, and offline readiness per target.
    Preflight {
        #[command(flatten)]
        sel: SelectionArg,
        #[arg(long)]
        dest: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Build a snapshot from an explicit selection.
    Create {
        #[command(flatten)]
        sel: SelectionArg,
        /// Folder (e.g. on an external drive) to create the snapshot in.
        #[arg(long)]
        dest: PathBuf,
        #[arg(long)]
        name: Option<String>,
        /// Folder with restore programs for other platforms (`<os>-<arch>/simple-setup[.exe]`).
        #[arg(long)]
        engine_kit: Vec<PathBuf>,
        #[arg(long)]
        yes: bool,
        /// Encrypt the snapshot with a password (asked for twice).
        #[arg(long)]
        encrypt: bool,
        /// Encrypt with the password on the first line of this file.
        #[arg(long)]
        password_file: Option<PathBuf>,
        /// Update this earlier snapshot of the same selection: unchanged files are taken from it.
        #[arg(long)]
        base: Option<PathBuf>,
    },
    /// Re-check every file in a snapshot against its checksum.
    Verify { snapshot: PathBuf },
    /// List snapshots this computer has made or opened.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show a snapshot's contents.
    Show { snapshot: PathBuf },
    /// Delete a snapshot folder (asks you to type its name).
    Delete { snapshot: PathBuf },
    /// Remove an interrupted build.
    DiscardPartial { dest: PathBuf, name: String },
}

#[derive(Subcommand)]
enum ProfileCmd {
    List,
    Save {
        name: String,
        #[arg(long)]
        selection: PathBuf,
    },
    Show {
        name: String,
    },
}

#[derive(clap::Args)]
struct SelectionArg {
    /// Prepare for this Linux distribution (a profile ID such as linuxmint, or linux for any).
    #[arg(long = "for")]
    for_distro: Option<String>,
    /// Selection JSON file (see `simple-setup profile show`).
    #[arg(long, conflicts_with = "profile")]
    selection: Option<PathBuf>,
    /// Use a saved profile.
    #[arg(long)]
    profile: Option<String>,
}

type R<T> = Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn human(bytes: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < units.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", units[i])
    }
}

fn confirm(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).is_ok() && matches!(line.trim(), "y" | "Y" | "yes")
}

/// The first line of `file`, or typed in without echo (twice for a new password).
fn password(file: Option<&Path>, prompt: &str, new: bool) -> R<String> {
    if let Some(f) = file {
        return Ok(std::fs::read_to_string(f)?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string());
    }
    let ask = |q: &str| {
        rpassword::prompt_password(q).map_err(
            |_| "A password is needed: run this in a terminal to type it, or use --password-file.",
        )
    };
    let p = ask(prompt)?;
    if new && ask("Type it again: ")? != p {
        return Err("The passwords do not match.".into());
    }
    Ok(p)
}

fn printer() -> impl FnMut(Progress) {
    let mut last = 0;
    move |p| match p {
        Progress::Stage { label, step, steps } => eprintln!("[{step}/{steps}] {label}"),
        Progress::Copy {
            done_bytes,
            total_bytes,
            done_files,
            total_files,
            ..
        } => {
            let pct = (done_bytes * 100).checked_div(total_bytes).unwrap_or(100);
            if pct != last || done_files == total_files {
                last = pct;
                eprint!(
                    "\r  {pct:>3}%  {done_files}/{total_files} files  {}   ",
                    human(done_bytes)
                );
                if done_files == total_files {
                    eprintln!();
                }
            }
        }
        Progress::Warning { message } => eprintln!("\n  warning: {message}"),
        Progress::Note { message } => eprintln!("  {message}"),
        Progress::Item {
            state: ItemState::Started,
            message,
            ..
        } => eprintln!("→ {message}"),
        Progress::Item { .. } => {}
    }
}

fn inventory(store: &Store, extra: &[PathBuf]) -> R<Inventory> {
    let mut opts = ScanOptions {
        source_dirs: store.settings()?.source_dirs,
    };
    opts.source_dirs.extend(extra.iter().cloned());
    Ok(discovery::scan(&opts, &store.user_apps()?)?)
}

fn load_selection(store: &Store, arg: &SelectionArg) -> R<Selection> {
    let mut sel = load_selection_file(store, arg)?;
    if let Some(d) = &arg.for_distro {
        sel.linux_distro = Some(d.clone());
    }
    Ok(sel)
}

fn planning(store: &Store) -> R<Planning> {
    Ok(Planning {
        remote: None,
        prefs: Prefs::from_settings(&store.settings()?),
    })
}

fn load_selection_file(store: &Store, arg: &SelectionArg) -> R<Selection> {
    match (&arg.selection, &arg.profile) {
        (Some(p), _) => Ok(serde_json::from_slice(&std::fs::read(p)?)?),
        (None, Some(name)) => Ok(store
            .profiles()?
            .into_iter()
            .find(|p| &p.name == name)
            .ok_or(format!("No profile named {name}"))?
            .selection),
        (None, None) => Err(
            "Pass --selection FILE or --profile NAME. Nothing is ever selected automatically."
                .into(),
        ),
    }
}

fn home() -> R<PathBuf> {
    Ok(simple_setup_core::paths::home_dir().ok_or("No home folder for this user.")?)
}

fn engines(kits: &[PathBuf]) -> build::Engines {
    let exe = std::env::current_exe().ok();
    let mut kits = kits.to_vec();
    if let Some(dir) = exe.as_ref().and_then(|e| e.parent()) {
        kits.extend(build::kit_dirs(dir));
    }
    build::find_engines(exe, &kits)
}

fn print_preflight(p: &build::Preflight) {
    println!(
        "Will copy {} files, {}:",
        p.total_files,
        human(p.total_bytes)
    );
    if p.installer_bytes > 0 {
        println!(
            "Will also collect offline installers (roughly {}; downloads need internet, Flatpak runtimes can take several minutes).",
            human(p.installer_bytes)
        );
    }
    for r in &p.roots {
        println!(
            "  {:<60} {:>7} files {:>10}",
            r.label,
            r.files,
            human(r.bytes)
        );
    }
    let e = &p.excluded;
    println!(
        "Left out: {} secret item(s), {} cache/lock item(s), {} excluded by you, {} unreadable, {} special file(s)",
        e.secrets, e.transient, e.by_user, e.unreadable, e.special
    );
    for s in &e.secret_examples {
        println!("  protected: {s}");
    }
    for t in &p.readiness {
        println!(
            "{}: {}",
            t.target,
            if t.offline_ready {
                "fully offline-ready"
            } else {
                "NOT fully offline-ready"
            }
        );
        for r in &t.reasons {
            println!("  - {r}");
        }
    }
    for w in &p.warnings {
        println!("warning: {w}");
    }
    if let Some(free) = p.destination_free {
        println!(
            "Needs {} at the destination; {} free{}",
            human(p.required_bytes),
            human(free),
            if p.fits { "" } else { " — NOT ENOUGH SPACE" }
        );
    }
}

fn run(cli: Cli) -> R<ExitCode> {
    let store = Store::open_default()?;
    let cancel = AtomicBool::new(false);
    match cli.cmd {
        Cmd::Schema => println!("{}", serde_json::to_string_pretty(&manifest::schema())?),
        Cmd::Scan { json, source_dirs } => {
            let inv = inventory(&store, &source_dirs)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&inv)?);
            } else {
                println!(
                    "{} — {} ({})",
                    inv.device.hostname, inv.device.os_name, inv.device.platform
                );
                for a in &inv.apps {
                    println!(
                        "  {:<44} {:<16} {:<10} {:<9} {}",
                        a.id,
                        a.version
                            .as_deref()
                            .unwrap_or("-")
                            .chars()
                            .take(16)
                            .collect::<String>(),
                        discovery::source_key(a.source),
                        if a.system { "os" } else { "yours" },
                        a.name
                    );
                }
                println!(
                    "{} applications; {} libraries and system packages not shown.",
                    inv.apps.len(),
                    inv.hidden_packages
                );
                for n in &inv.notes {
                    println!("note: {n}");
                }
            }
        }
        Cmd::Volumes { json } => {
            let v = platform::volumes();
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                for d in v {
                    println!(
                        "{:<40} {:>10} free of {:>10} {}",
                        d.mount.display(),
                        human(d.free),
                        human(d.total),
                        if d.removable { "removable" } else { "" }
                    );
                }
            }
        }
        Cmd::Profile(ProfileCmd::List) => {
            for p in store.profiles()? {
                println!(
                    "{}  (saved {}, {} apps, {} folders)",
                    p.name,
                    p.saved_at,
                    p.selection.apps.len(),
                    p.selection.folders.len()
                );
            }
        }
        Cmd::Profile(ProfileCmd::Save { name, selection }) => {
            let sel: Selection = serde_json::from_slice(&std::fs::read(selection)?)?;
            store.save_profile(&name, &sel)?;
            println!("Saved profile {name}");
        }
        Cmd::Profile(ProfileCmd::Show { name }) => {
            let p = store
                .profiles()?
                .into_iter()
                .find(|p| p.name == name)
                .ok_or("No such profile")?;
            println!("{}", serde_json::to_string_pretty(&p.selection)?);
        }
        Cmd::Snapshot(SnapshotCmd::Preflight { sel, dest, json }) => {
            let selection = load_selection(&store, &sel)?;
            let inv = inventory(&store, &[])?;
            let plan = build::plan(
                &inv,
                &selection,
                &home()?,
                dest.as_deref(),
                &engines(&[]),
                &planning(&store)?,
            )?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan.summary)?);
            } else {
                print_preflight(&plan.summary);
            }
        }
        Cmd::Snapshot(SnapshotCmd::Create {
            sel,
            dest,
            name,
            engine_kit,
            yes,
            encrypt,
            password_file,
            base,
        }) => {
            let base_locked = match &base {
                Some(b) => manifest::Manifest::read(b)?.encryption.is_some(),
                None => false,
            };
            let pw = if encrypt || password_file.is_some() || base_locked {
                let prompt = if base_locked {
                    "Password of the earlier snapshot: "
                } else {
                    "Password for the snapshot: "
                };
                Some(password(password_file.as_deref(), prompt, !base_locked)?)
            } else {
                None
            };
            let selection = load_selection(&store, &sel)?;
            let inv = inventory(&store, &[])?;
            let engines = engines(&engine_kit);
            let home = home()?;
            let planning = planning(&store)?;
            let plan = build::plan(&inv, &selection, &home, Some(&dest), &engines, &planning)?;
            print_preflight(&plan.summary);
            let name = name.unwrap_or_else(|| {
                format!(
                    "{} {}",
                    inv.device.hostname,
                    &simple_setup_core::now()[..10]
                )
            });
            if !yes && !confirm(&format!("Build snapshot \"{name}\" in {}?", dest.display())) {
                println!("Nothing was written.");
                return Ok(ExitCode::SUCCESS);
            }
            let req = BuildRequest {
                inventory: &inv,
                selection: &selection,
                name: &name,
                destination: &dest,
                engines: &engines,
                home: &home,
                password: pw.as_deref(),
                base: base.as_deref(),
                planning: &planning,
            };
            let report = build::build(&req, &mut printer(), &cancel)?;
            store.remember_snapshot(SnapshotRef {
                path: report.path.clone(),
                id: report.id.clone(),
                name: report.name.clone(),
                created_at: simple_setup_core::now(),
                last_verified: Some(simple_setup_core::now()),
                last_verify_ok: Some(true),
            })?;
            println!(
                "Snapshot written and verified: {} ({} files, {})",
                report.path.display(),
                report.files,
                human(report.bytes)
            );
            println!(
                "  Restore instructions (the command to run on the new computer): {}",
                report.path.join(build::RESTORE_MD).display()
            );
            if report.reused_files > 0 {
                println!(
                    "  {} files ({}) were taken from the earlier snapshot instead of copied again.",
                    report.reused_files,
                    human(report.reused_bytes)
                );
            }
            if let Some(k) = &report.recovery_key {
                println!(
                    "\nThis snapshot is encrypted. Without its password or this recovery key nobody can\nopen it, including you. Write the key down and keep it away from the drive:\n\n    {k}\n"
                );
            }
            for f in &report.failed {
                println!("  could not copy {}: {}", f.path, f.error);
            }
            for t in &report.readiness {
                println!(
                    "  {}: {}",
                    t.target,
                    if t.offline_ready {
                        "fully offline-ready"
                    } else {
                        "not fully offline-ready (see reports/build-report.txt)"
                    }
                );
            }
            if !report.failed.is_empty() {
                return Ok(ExitCode::from(2));
            }
        }
        Cmd::Snapshot(SnapshotCmd::Verify { snapshot: path }) => {
            let r = snapshot::verify(&path, &mut printer(), &cancel)?;
            store.update_snapshot(&path, |s| {
                s.last_verified = Some(simple_setup_core::now());
                s.last_verify_ok = Some(r.ok);
            })?;
            for m in &r.missing {
                println!("missing: {m}");
            }
            for c in &r.corrupt {
                println!("damaged: {c}");
            }
            println!(
                "{} ({} files, {} checked)",
                if r.ok {
                    "Snapshot is intact"
                } else {
                    "Snapshot is DAMAGED"
                },
                r.checked,
                human(r.bytes)
            );
            if !r.ok {
                return Ok(ExitCode::from(3));
            }
        }
        Cmd::Snapshot(SnapshotCmd::List { json }) => {
            let refs = store.snapshots()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&refs)?);
            }
            for r in refs.iter().filter(|_| !json) {
                let state = match snapshot::summary(&r.path) {
                    Ok(s) => format!("{} files, {}", s.files, human(s.bytes)),
                    Err(_) => "not reachable (drive unplugged?)".into(),
                };
                println!(
                    "{}  {}  {}  [{}]",
                    r.created_at,
                    r.name,
                    r.path.display(),
                    state
                );
            }
        }
        Cmd::Snapshot(SnapshotCmd::Show { snapshot: path }) => {
            let s = snapshot::summary(&path)?;
            println!(
                "{}\n  created {} on {} ({}, {})",
                s.name, s.created_at, s.source.hostname, s.source.os_name, s.source.platform
            );
            if let Some(b) = &s.base {
                println!("  an update of \"{b}\"");
            }
            if s.encrypted {
                println!("  encrypted: apps, folders and files are shown only with the password");
            } else {
                println!("  apps: {}", s.apps.join(", "));
                println!("  folders: {}", s.folders.join(", "));
                println!(
                    "  {} files, {}; offline installers for {} app(s)",
                    s.files,
                    human(s.bytes),
                    s.installers
                );
            }
            for t in &s.readiness {
                println!(
                    "  {}: {}",
                    t.target,
                    if t.offline_ready {
                        "fully offline-ready"
                    } else {
                        "not fully offline-ready"
                    }
                );
            }
        }
        Cmd::Snapshot(SnapshotCmd::Delete { snapshot: path }) => {
            let s = snapshot::summary(&path)?;
            print!(
                "This permanently deletes {} ({} files). Type the snapshot name to confirm: ",
                path.display(),
                s.files
            );
            std::io::stdout().flush()?;
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            if line.trim() != s.name {
                println!("Name did not match; nothing was deleted.");
                return Ok(ExitCode::FAILURE);
            }
            snapshot::delete(&path)?;
            store.forget_snapshot(&path)?;
            println!("Deleted.");
        }
        Cmd::Snapshot(SnapshotCmd::DiscardPartial { dest, name }) => {
            build::discard_partial(&dest, &name)?;
            println!("Removed the interrupted snapshot.");
        }
        Cmd::Restore {
            snapshot: path,
            opts,
        } => {
            return restore_cmd(&store, &path, opts, &cancel);
        }
        Cmd::Probe { to, code, json } => {
            let addr = match to {
                Some(a) => transfer::parse_address(&a)?,
                None => pick_peer()?,
            };
            let code = match code {
                Some(c) => c,
                None => {
                    print!("Pairing code shown on the other computer: ");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line)?;
                    line
                }
            };
            let p = transfer::probe(addr, &code)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&p.info)?);
            } else {
                print_system(&p.info);
            }
        }
        Cmd::System { json } => {
            let info = probe::detect(&home()?, false)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                print_system(&info);
            }
        }
        Cmd::Distros(c) => return distros_cmd(c, &cancel),
        Cmd::Analyze {
            snapshot: path,
            as_profile,
            os,
            arch,
            json,
            password_file,
        } => {
            let mut target = match (&as_profile, &os) {
                (None, None) => {
                    let mut t = Target::this_user(None, Elevation::detect(false), false)?;
                    t.online = true;
                    t
                }
                _ => Target::described(described(
                    as_profile.as_deref(),
                    os.as_deref(),
                    arch.as_deref(),
                )?),
            };
            target.prefs = Prefs::from_settings(&store.settings()?);
            let outer = manifest::Manifest::read(&path)?;
            if outer.encryption.is_some() {
                let secret = password(
                    password_file.as_deref(),
                    "Password or recovery key: ",
                    false,
                )?;
                target.key = outer.unlock(&path, Some(&secret))?;
            }
            let plan = restore::plan(&path, &target)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
                return Ok(ExitCode::SUCCESS);
            }
            print_analysis(&plan);
        }
        Cmd::Peers { json } => {
            let peers = transfer::discover(Duration::from_secs(3))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&peers)?);
            } else if peers.is_empty() {
                println!("No computer nearby is receiving. Start receiving there first.");
            }
            for p in peers.iter().filter(|_| !json) {
                println!(
                    "{}  {}  {}  {}",
                    p.name,
                    p.system.as_deref().unwrap_or("?"),
                    p.os.as_deref().unwrap_or("?"),
                    p.addresses[0]
                );
            }
        }
        Cmd::Send {
            snapshot: path,
            to,
            code,
            yes,
        } => {
            let addr = match to {
                Some(a) => transfer::parse_address(&a)?,
                None => pick_peer()?,
            };
            let code = match code {
                Some(c) => c,
                None => {
                    print!("Pairing code shown on the other computer: ");
                    let _ = std::io::stdout().flush();
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line)?;
                    line
                }
            };
            let targets = manifest::Manifest::read(&path)?.targets;
            let report = transfer::send(
                &path,
                addr,
                &code,
                &mut |d| {
                    println!(
                        "Paired with {} ({}, {}).",
                        d.hostname, d.os_name, d.platform
                    );
                    if !targets.contains(&d.platform) {
                        println!(
                            "  Note: this snapshot was not prepared for {}; only personal files and some settings may restore there.",
                            d.platform
                        );
                    }
                    yes || confirm("Send the snapshot?")
                },
                &mut printer(),
                &cancel,
            )?;
            println!(
                "Sent {} files ({}{}) to {}; it was checked there and saved as {}.",
                report.files,
                human(report.bytes),
                if report.resumed_bytes > 0 {
                    format!(
                        ", {} of it already there from an earlier attempt",
                        human(report.resumed_bytes)
                    )
                } else {
                    String::new()
                },
                report.receiver.hostname,
                report.saved_as
            );
        }
        Cmd::Receive {
            dest,
            yes,
            no_announce,
            restore,
        } => {
            let dest = match dest {
                Some(d) => d,
                None => home()?.join("Simple Setup Snapshots"),
            };
            std::fs::create_dir_all(&dest)?;
            let r = transfer::Receiver::start(!no_announce)?;
            let addr = transfer::local_address().map_or_else(
                || format!("port {}", r.port),
                |ip| format!("{ip}:{}", r.port),
            );
            println!(
                "Receiving into {}. On the other computer, send to this one and enter:\n\n    {}\n\nAddress (if it is not found automatically): {addr}",
                dest.display(),
                r.code
            );
            if let Some(e) = &r.announce_error {
                println!("  Not announced on the network ({e}); use the address.");
            }
            println!("Waiting… (Ctrl+C to stop)");
            let path = r.run(
                &dest,
                &mut |from, offer| {
                    println!(
                        "{} ({}, {}) wants to send \"{}\": {} files, {}{}, made for {}.",
                        from.hostname,
                        from.os_name,
                        from.platform,
                        offer.name,
                        offer.files.len(),
                        human(offer.bytes()),
                        if offer.encrypted { ", encrypted" } else { "" },
                        offer
                            .targets
                            .iter()
                            .map(|t| t.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    yes || confirm("Accept it?")
                },
                &mut printer(),
                &cancel,
            )?;
            let m = manifest::Manifest::read(&path)?;
            store.remember_snapshot(SnapshotRef {
                path: path.clone(),
                id: m.id,
                name: m.name,
                created_at: m.created_at,
                last_verified: Some(simple_setup_core::now()),
                last_verify_ok: Some(true),
            })?;
            println!("Received and checked: {}", path.display());
            if restore {
                return restore_cmd(
                    &store,
                    &path,
                    RestoreArgs {
                        yes,
                        ..RestoreArgs::default()
                    },
                    &cancel,
                );
            }
            println!(
                "To restore it here: simple-setup restore \"{}\"",
                path.display()
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_system(info: &SystemInfo) {
    println!(
        "{} — {} ({})",
        info.device.hostname, info.device.os_name, info.device.platform
    );
    if let Some(l) = &info.linux {
        let p = &l.profile;
        println!("  {}", l.explanation);
        println!(
            "  Distribution: {} (profile {}, family {}; {})",
            l.os_release.display(),
            p.id,
            p.family,
            p.chain.join(" → ")
        );
        println!(
            "  Packages: {} with {}{}{}",
            if p.formats.is_empty() {
                "none".to_string()
            } else {
                p.formats
                    .iter()
                    .map(|f| f.label())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            p.package_manager.label(),
            if l.immutable {
                "; read-only system image"
            } else {
                ""
            },
            if p.rolling { "; rolling release" } else { "" }
        );
        println!(
            "  Portable formats: Flatpak {}, Snap {}, AppImage {}",
            if info.has("flatpak") {
                "installed"
            } else {
                "not installed"
            },
            if info.has("snap") {
                "installed"
            } else {
                "not installed"
            },
            if l.fuse {
                "mounts (FUSE)"
            } else {
                "runs unpacked (no FUSE)"
            }
        );
        println!(
            "  Processor: {}; kernel {}",
            l.machine,
            l.kernel.as_deref().unwrap_or("unknown")
        );
    }
    println!(
        "  Tools: {}",
        if info.tools.is_empty() {
            "none".into()
        } else {
            info.tools.join(", ")
        }
    );
}

/// A computer described on the command line: `profile[:release]`, system and processor.
fn described(profile: Option<&str>, os: Option<&str>, arch: Option<&str>) -> R<SystemInfo> {
    let os = match os.unwrap_or("linux") {
        "linux" => platform::OsFamily::Linux,
        "windows" => platform::OsFamily::Windows,
        "macos" => platform::OsFamily::Macos,
        o => return Err(format!("Unknown system {o:?} (linux, windows or macos).").into()),
    };
    let arch = match arch {
        None => platform::Platform::current()?.arch,
        Some("x86_64") => platform::Arch::X86_64,
        Some("aarch64") => platform::Arch::Aarch64,
        Some(a) => return Err(format!("Unknown processor {a:?} (x86_64 or aarch64).").into()),
    };
    let p = platform::Platform { os, arch };
    let (id, release) = match profile {
        Some(s) => match s.split_once(':') {
            Some((i, r)) => (Some(i), Some(r)),
            None => (Some(s), None),
        },
        None => (None, None),
    };
    let profile = match (os, id) {
        (platform::OsFamily::Linux, Some(id)) => Some(
            distros::Distros::load_default()
                .resolve(distros::canonical(id))
                .map_err(|e| e.to_string())?,
        ),
        _ => None,
    };
    let mut info = SystemInfo::assumed(p, profile);
    if let (Some(l), Some(r)) = (info.linux.as_mut(), release) {
        l.os_release.version_id = Some(r.into());
        info.distro = Some(l.os_release.release());
    }
    Ok(info)
}

fn print_analysis(plan: &restore::RestorePlan) {
    println!(
        "Snapshot \"{}\" from {} ({}) → {} ({})",
        plan.snapshot_name,
        plan.source.hostname,
        plan.source.os_name,
        plan.this_device.os_name,
        plan.this_device.platform
    );
    for w in &plan.warnings {
        println!("warning: {w}");
    }
    let installs: Vec<_> = plan
        .items
        .iter()
        .filter(|i| i.kind == ItemKind::Installer)
        .collect();
    for v in [
        Verdict::Ready,
        Verdict::Alternative,
        Verdict::Review,
        Verdict::Unavailable,
    ] {
        let group: Vec<_> = installs.iter().filter(|i| i.verdict == v).collect();
        println!("\n{} ({}):", v.label(), group.len());
        for i in group {
            println!(
                "  {:<28} {}",
                i.app.as_deref().unwrap_or(&i.label),
                i.method.as_deref().unwrap_or("—")
            );
            println!("      {}", i.reason);
            for c in &i.considered {
                println!("      · {}: {}", c.option, c.outcome);
            }
        }
    }
    let files = plan.items.iter().filter(|i| i.kind != ItemKind::Installer);
    let (ok, other): (Vec<_>, Vec<_>) = files.partition(|i| i.status == Status::Ready);
    let not: Vec<&str> = other.iter().map(|i| i.label.as_str()).collect();
    println!(
        "\nSettings and files: {} restorable, {} not{}.",
        ok.len(),
        not.len(),
        if not.is_empty() {
            String::new()
        } else {
            format!(" ({})", not.join("; "))
        }
    );
}

fn distros_cmd(c: DistrosCmd, cancel: &AtomicBool) -> R<ExitCode> {
    let us = UserStore::open_default()?;
    let d = us.distros();
    for p in &d.load_problems {
        eprintln!("warning: {p}");
    }
    match c {
        DistrosCmd::List { json } => {
            let list = d.list();
            if json {
                println!("{}", serde_json::to_string_pretty(&list)?);
                return Ok(ExitCode::SUCCESS);
            }
            println!(
                "Profiles {} (built in {}{}):",
                d.current_version(),
                distros::builtin_version(),
                d.official_version()
                    .map(|v| format!(", official update {v}"))
                    .unwrap_or_default()
            );
            for l in list {
                println!(
                    "  {:<22} {:<30} {:<10} {:<20} {}{}{}",
                    l.id,
                    l.name,
                    format!("{:?}", l.layer).to_lowercase(),
                    l.package_manager.label(),
                    if l.enabled { "" } else { "[off] " },
                    if l.has_custom && !l.trusted {
                        "[commands not trusted] "
                    } else {
                        ""
                    },
                    l.problems.join("; ")
                );
            }
        }
        DistrosCmd::Show { id } => {
            println!("{}", serde_json::to_string_pretty(&d.resolve(&id)?)?);
        }
        DistrosCmd::Validate { file } => {
            let f = distros::read_file(&file)?;
            let here = platform::OsRelease::read();
            let r = distros::test(&f, &d, here.as_ref());
            if r.problems.is_empty() {
                println!("Valid: {} profile(s).", f.distros.len());
                if let Some(p) = &r.resolved {
                    println!(
                        "{} resolves to {} with {} ({}).",
                        p.id,
                        p.name,
                        p.package_manager.label(),
                        p.chain.join(" → ")
                    );
                }
                println!(
                    "It {} this computer.",
                    if r.matches_here {
                        "matches"
                    } else {
                        "does not match"
                    }
                );
                for (t, have) in &r.tools {
                    println!(
                        "  needs {t}: {}",
                        if *have {
                            "found here"
                        } else {
                            "not on this computer"
                        }
                    );
                }
            } else {
                for p in &r.problems {
                    println!("problem: {p}");
                }
                return Ok(ExitCode::FAILURE);
            }
        }
        DistrosCmd::Import { file, trust } => {
            let f = distros::read_file(&file)?;
            let ids = us.import(&f)?;
            println!("Saved {}.", ids.join(", "));
            for p in f.distros.iter().filter(|p| p.custom.is_some()) {
                let c = p.custom.as_ref().ok_or("unreachable")?;
                println!(
                    "{} has its own package-manager commands:
  install: {}
  query:   {}{}",
                    p.id,
                    c.install.join(" "),
                    c.query.join(" "),
                    if c.admin {
                        "
  (install runs with administrator rights)"
                    } else {
                        ""
                    }
                );
                if trust && confirm("Trust these commands?") {
                    us.trust(&p.id, true)?;
                    println!("Trusted.");
                } else {
                    println!(
                        "Not trusted: they are not run until you trust them (simple-setup distros trust {}).",
                        p.id
                    );
                }
            }
        }
        DistrosCmd::Export { ids, out } => {
            let f = us.export(&ids)?;
            let json = serde_json::to_string_pretty(&f)?;
            match out {
                Some(p) => {
                    std::fs::write(&p, json)?;
                    println!("Wrote {}.", p.display());
                }
                None => println!("{json}"),
            }
        }
        DistrosCmd::Enable { id } => us.set_enabled(&id, true)?,
        DistrosCmd::Disable { id } => us.set_enabled(&id, false)?,
        DistrosCmd::Remove { id } => us.remove(&id)?,
        DistrosCmd::Trust { id, revoke } => {
            if !revoke {
                let p = d.resolve(&id)?;
                let c = p.custom.ok_or("That profile has no custom commands.")?;
                println!(
                    "install: {}
query:   {}",
                    c.install.join(" "),
                    c.query.join(" ")
                );
                if !confirm("Trust these commands?") {
                    return Ok(ExitCode::FAILURE);
                }
            }
            us.trust(&id, !revoke)?;
        }
        DistrosCmd::Update => {
            let r = distros::update_official(&us, cancel)?;
            println!("{}", r.message);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// A receiving computer found nearby (asks when there are several).
fn pick_peer() -> R<std::net::SocketAddr> {
    println!("Looking for receiving computers nearby…");
    let peers = transfer::discover(Duration::from_secs(3))?;
    let p = match peers.len() {
        0 => return Err("No receiving computer found. Start receiving on the other computer, or pass --to with the address it shows.".into()),
        1 => &peers[0],
        _ => {
            for (i, p) in peers.iter().enumerate() {
                println!("  {}. {} ({})", i + 1, p.name, p.os.as_deref().unwrap_or("?"));
            }
            print!("Send to which one? ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            std::io::stdin().lock().read_line(&mut line)?;
            let n: usize = line.trim().parse().map_err(|_| "Type the number shown.")?;
            peers.get(n.wrapping_sub(1)).ok_or("Type the number shown.")?
        }
    };
    println!("Sending to {} at {}.", p.name, p.addresses[0]);
    Ok(p.addresses[0])
}

fn restore_cmd(store: &Store, path: &Path, o: RestoreArgs, cancel: &AtomicBool) -> R<ExitCode> {
    let RestoreArgs {
        dry_run,
        only,
        keep_both,
        yes,
        json,
        password_file,
        offline,
        no_source_builds,
    } = o;
    let password_file = password_file.as_deref();
    let elevation = Elevation::detect(true);
    let mut target = Target::this_user(Some(store.root().join("reports")), elevation, !offline)?;
    target.prefs = Prefs::from_settings(&store.settings()?);
    target.prefs.source &= !no_source_builds;
    let outer = manifest::Manifest::read(path)?;
    if outer.encryption.is_some() {
        let secret = password(password_file, "Password or recovery key: ", false)?;
        target.key = outer.unlock(path, Some(&secret))?;
    }
    println!("Checking the snapshot's integrity…");
    let v = snapshot::verify(path, &mut printer(), cancel)?;
    if !v.ok {
        for m in v.missing.iter().chain(&v.corrupt) {
            println!("  problem: {m}");
        }
        println!("The snapshot is damaged. Damaged files will not be restored.");
    }
    let plan = restore::plan(path, &target)?;
    if json && dry_run {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "Snapshot \"{}\" from {} ({}) → this computer ({}, {})",
        plan.snapshot_name,
        plan.source.hostname,
        plan.source.platform,
        plan.this_device.os_name,
        plan.this_device.platform
    );
    let sys = &plan.this_system;
    println!(
        "This computer: {}, {} cores, {} memory; tools: {}; internet: {}{}",
        sys.cpu,
        sys.cores,
        human(sys.memory_bytes),
        if sys.tools.is_empty() {
            "none".into()
        } else {
            sys.tools.join(", ")
        },
        if plan.online {
            "yes"
        } else if offline {
            "not used (--offline)"
        } else {
            "no"
        },
        if plan.offline_possible {
            "; everything chosen can be restored offline"
        } else {
            ""
        }
    );
    for w in &plan.warnings {
        println!("warning: {w}");
    }
    let chosen: Vec<String> = if only.is_empty() {
        plan.items.iter().map(|i| i.id.clone()).collect()
    } else {
        only
    };
    for i in &plan.items {
        let status = match i.status {
            Status::Ready => "Supported",
            Status::Manual => "Requires manual action",
            Status::Unsupported => "Unsupported — will skip",
        };
        let mark = if chosen.contains(&i.id) { "[x]" } else { "[ ]" };
        println!(
            "{mark} {:<10} {}  ({status}; {})",
            i.id,
            i.label,
            i.verdict.label()
        );
        println!("      {}", i.reason);
        if i.status == Status::Ready && i.kind == ItemKind::Installer && i.files > 0 {
            println!(
                "      {} installer file(s), {}{}",
                i.files,
                human(i.bytes),
                if i.admin {
                    "; needs an administrator password"
                } else {
                    ""
                }
            );
        } else if i.status == Status::Ready && i.kind == ItemKind::Installer {
            if let Some(m) = &i.method {
                println!("      via {m}");
            }
        } else if i.status == Status::Ready {
            println!(
                "      {} files, {}; {} already identical; {} conflicting",
                i.files,
                human(i.bytes),
                i.identical,
                i.conflicts
            );
            for c in &i.conflict_examples {
                println!("        exists: {c}");
            }
        }
    }
    println!(
        "Needs {} free; {} available.",
        human(plan.required_bytes),
        plan.free_bytes.map_or("unknown".into(), human)
    );
    if plan.items.iter().any(|i| i.conflicts > 0) {
        println!(
            "Existing different files will be {}.",
            if keep_both {
                "kept, with the snapshot's copy restored next to them as \"name (restored)\""
            } else {
                "left alone and the snapshot's copy skipped"
            }
        );
    }
    if plan.needs_admin {
        println!(
            "Installing some apps needs administrator rights; sudo asks for your password once, before anything starts."
        );
    }
    if dry_run {
        println!("Dry run: nothing was changed.");
        return Ok(ExitCode::SUCCESS);
    }
    if !yes && !confirm("Restore the checked items now?") {
        println!("Nothing was changed.");
        return Ok(ExitCode::SUCCESS);
    }
    // Ask for the administrator password now, not halfway through an unattended restore
    // (sudo then remembers it for a while).
    if plan.needs_admin
        && elevation == Elevation::Sudo
        && !std::process::Command::new("sudo")
            .arg("-v")
            .status()
            .is_ok_and(|s| s.success())
    {
        println!("No administrator rights: the apps that need them will fail and be reported.");
    }
    let choice = RestoreChoice {
        approvals: plan
            .items
            .iter()
            .filter(|i| chosen.contains(&i.id))
            .map(|i| restore::Approval {
                item: i.id.clone(),
                considered: i.considered.clone(),
            })
            .collect(),
        items: chosen,
        conflict: if keep_both {
            Conflict::KeepBoth
        } else {
            Conflict::Skip
        },
    };
    let report = restore::execute(path, &target, &choice, &mut printer(), cancel)?;
    let mut bad = false;
    for r in &report.items {
        let word = match r.outcome {
            Outcome::Installed => "installed",
            Outcome::Restored => "restored",
            Outcome::Partial => "partly restored",
            Outcome::Failed => "FAILED",
            Outcome::Manual => "needs manual action",
            Outcome::Skipped => "skipped",
            Outcome::NotSelected => continue,
        };
        bad |= matches!(r.outcome, Outcome::Failed | Outcome::Partial);
        println!("{word:>20}: {}  {}", r.label, r.message);
        if r.outcome != Outcome::Failed {
            for d in &r.details {
                println!("{:>22}{d}", "");
            }
        }
        for f in r.failed.iter().filter(|f| f.error != r.message) {
            println!("{:>22}{}: {}", "", f.path, f.error);
        }
    }
    if report.cancelled {
        println!(
            "\nThe restore was cancelled; run it again to continue (finished items are kept)."
        );
        bad = true;
    }
    if report.restart_required {
        println!(
            "\nRestart the computer to finish installing. Running the restore again afterwards completes anything left."
        );
    }
    if !report.manual_steps.is_empty() {
        println!("\nManual follow-up:");
        for s in &report.manual_steps {
            println!("  - {s}");
        }
    }
    if let Some(f) = &report.report_file {
        println!("\nReport saved to {}", f.display());
    }
    Ok(if bad {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}
