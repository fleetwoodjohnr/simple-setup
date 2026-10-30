//! `simple-setup` command-line interface. Also shipped inside every snapshot as the restore
//! engine, so it must work on a freshly installed computer with no other tools.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use clap::{Parser, Subcommand};
use simple_setup_core::build::{self, BuildRequest, Progress};
use simple_setup_core::discovery::{self, Inventory, ScanOptions};
use simple_setup_core::installers::Elevation;
use simple_setup_core::manifest::{self, Status};
use simple_setup_core::platform;
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
    Restore {
        snapshot: PathBuf,
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
    },
    /// List computers on this network that are receiving right now.
    Peers {
        #[arg(long)]
        json: bool,
    },
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
            let plan = build::plan(&inv, &selection, &home()?, dest.as_deref(), &engines(&[]))?;
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
            let plan = build::plan(&inv, &selection, &home, Some(&dest), &engines)?;
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
            dry_run,
            only,
            keep_both,
            yes,
            json,
            password_file,
        } => {
            return restore_cmd(
                &store,
                &path,
                dry_run,
                only,
                keep_both,
                yes,
                json,
                password_file.as_deref(),
                &cancel,
            );
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
                    "{}  {}  {}",
                    p.name,
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
            println!(
                "Received and checked: {}\nTo restore it here: simple-setup restore \"{}\"",
                path.display(),
                path.display()
            );
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

#[allow(clippy::too_many_arguments)]
fn restore_cmd(
    store: &Store,
    path: &Path,
    dry_run: bool,
    only: Vec<String>,
    keep_both: bool,
    yes: bool,
    json: bool,
    password_file: Option<&Path>,
    cancel: &AtomicBool,
) -> R<ExitCode> {
    let mut target =
        Target::this_user(Some(store.root().join("reports")), Elevation::detect(true))?;
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
        println!("{mark} {:<10} {}  ({status})", i.id, i.label);
        println!("      {}", i.reason);
        if i.status == Status::Ready && i.kind == ItemKind::Installer {
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
            "Installing some apps needs administrator rights; sudo will ask for your password when they are installed."
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
    let choice = RestoreChoice {
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
        for f in &r.failed {
            println!("{:>22}{}: {}", "", f.path, f.error);
        }
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
