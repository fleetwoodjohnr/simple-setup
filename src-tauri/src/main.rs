//! Simple Setup desktop shell: a thin IPC adapter over `simple-setup-core`. Every operation
//! the UI offers runs the same engine code as the CLI; nothing here simulates results.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use simple_setup_core::build::{
    self, BuildReport, BuildRequest, Engines, Planning, Preflight, Progress,
};
use simple_setup_core::discovery::{self, App, Inventory, ScanOptions};
use simple_setup_core::distros::{
    self, Listed, Profile as DistroProfile, ProfileFile, Resolved, TestReport, UserStore,
};
use simple_setup_core::inspect::{self, DirItem, Measure};
use simple_setup_core::installers::Elevation;
use simple_setup_core::manifest::Manifest;
use simple_setup_core::platform::{self, Device, Platform, Volume};
use simple_setup_core::probe::{self, SystemInfo};
use simple_setup_core::resolve::Prefs;
use simple_setup_core::restore::{self, RestoreChoice, RestorePlan, RestoreReport, Target};
use simple_setup_core::selection::{KnownFolder, Selection};
use simple_setup_core::snapshot::{self, Summary, VerifyReport};
use simple_setup_core::store::{
    AppEdit, Direction, Profile, Settings, SnapshotRef, Store, TransferRecord, UserApps,
};
use simple_setup_core::transfer::{self, Peer, Probed, SendReport};
use simple_setup_core::update::{self, AfterInstall, UpdateInfo};
use tauri::ipc::Channel;
use tauri::{AppHandle, State};

#[derive(Debug, Serialize)]
struct CmdError {
    code: &'static str,
    message: String,
}

impl From<simple_setup_core::Error> for CmdError {
    fn from(e: simple_setup_core::Error) -> Self {
        let code = match e {
            simple_setup_core::Error::Invalid(_) => "invalid",
            simple_setup_core::Error::Io { .. } => "io",
            simple_setup_core::Error::Cancelled => "cancelled",
            simple_setup_core::Error::Locked => "locked",
            simple_setup_core::Error::Probed(_) => "probed",
            simple_setup_core::Error::Disconnected(_) => "disconnected",
        };
        Self {
            code,
            message: e.to_string(),
        }
    }
}

impl From<std::io::Error> for CmdError {
    fn from(e: std::io::Error) -> Self {
        Self {
            code: "io",
            message: e.to_string(),
        }
    }
}

fn invalid(message: impl Into<String>) -> CmdError {
    CmdError {
        code: "invalid",
        message: message.into(),
    }
}

type Res<T> = Result<T, CmdError>;

struct Inner {
    store: Store,
    /// Last scan without user corrections applied; corrections are re-applied on read.
    raw: Mutex<Option<Inventory>>,
    build_cancel: AtomicBool,
    restore_cancel: AtomicBool,
    /// Separate flags: starting one direction must not undo cancelling the other.
    send_cancel: AtomicBool,
    /// Pause and speed limit of the send in progress.
    send_control: transfer::Control,
    receive_cancel: AtomicBool,
    /// Where the person's answer to an incoming offer goes while receiving.
    offer_answer: Mutex<Option<std::sync::mpsc::Sender<bool>>>,
    /// What this computer offers, looked up at most every two minutes (the internet check
    /// can take a few seconds when offline).
    system: Mutex<Option<(std::time::Instant, SystemInfo)>>,
    /// The last update check, and the verified download once it is complete.
    update: Mutex<(Option<UpdateInfo>, Option<PathBuf>)>,
    update_cancel: AtomicBool,
}

type AppState = Arc<Inner>;

impl Inner {
    fn system(&self) -> Res<SystemInfo> {
        let mut cached = self
            .system
            .lock()
            .map_err(|_| invalid("Internal state error."))?;
        if let Some((at, info)) = cached.as_ref()
            && at.elapsed() < Duration::from_secs(120)
        {
            return Ok(info.clone());
        }
        let info = probe::detect(&home()?, true)?;
        *cached = Some((std::time::Instant::now(), info.clone()));
        Ok(info)
    }

    /// The distribution profiles or the override changed: detect again next time.
    fn forget_system(&self) {
        if let Ok(mut c) = self.system.lock() {
            *c = None;
        }
    }

    fn inventory(&self) -> Res<Inventory> {
        let raw = self
            .raw
            .lock()
            .map_err(|_| invalid("Internal state error."))?;
        let mut inv = raw
            .clone()
            .ok_or_else(|| invalid("Scan this computer first."))?;
        self.store.user_apps()?.apply(&mut inv.apps);
        inv.apps.sort_by_cached_key(|a| a.name.to_lowercase());
        Ok(inv)
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| invalid(e.to_string()))?
}

fn home() -> Res<PathBuf> {
    simple_setup_core::paths::home_dir().ok_or_else(|| invalid("No home folder for this user."))
}

/// The CLI next to this executable is the host restore engine; installed packages also carry a
/// `restore-kit/<os>-<arch>/` folder with engines for every platform (see `build::kit_dirs`).
fn engines() -> Engines {
    let exe = std::env::current_exe().ok();
    let dir = exe
        .as_ref()
        .and_then(|e| e.parent())
        .map(|d| d.to_path_buf())
        .unwrap_or_default();
    let host = Platform::current()
        .ok()
        .map(|p| dir.join(build::engine_file(p)));
    build::find_engines(host, &build::kit_dirs(&dir))
}

#[tauri::command]
fn device_info() -> Res<Device> {
    Ok(Device::detect()?)
}

#[tauri::command]
async fn scan(state: State<'_, AppState>) -> Res<Inventory> {
    let s = state.inner().clone();
    blocking(move || {
        let opts = ScanOptions {
            source_dirs: s.store.settings()?.source_dirs,
        };
        let raw = discovery::scan(&opts, &UserApps::default())?;
        *s.raw.lock().map_err(|_| invalid("Internal state error."))? = Some(raw);
        s.inventory()
    })
    .await
}

#[tauri::command]
fn get_settings(state: State<'_, AppState>) -> Res<Settings> {
    Ok(state.store.settings()?)
}

#[tauri::command]
fn save_settings(state: State<'_, AppState>, settings: Settings) -> Res<()> {
    let folders = settings
        .source_dirs
        .iter()
        .chain(&settings.default_destination)
        .chain(&settings.receive_folder);
    // Only the form is checked: a folder on a drive that is not plugged in right now must not
    // stop other settings from being saved (the scan skips folders that are missing).
    for d in folders {
        if !d.is_absolute() {
            return Err(invalid(format!(
                "{} is not a full folder path.",
                d.display()
            )));
        }
    }
    if let Some(id) = &settings.distro_override
        && UserStore::open_default()?.distros().resolve(id).is_err()
    {
        return Err(invalid(format!("No distribution profile {id:?}.")));
    }
    let changed = state.store.settings()?.distro_override != settings.distro_override;
    state.store.save_settings(&settings)?;
    if changed {
        state.forget_system();
    }
    Ok(())
}

#[derive(Serialize)]
struct AppInfo {
    version: &'static str,
    repository: &'static str,
    /// Development builds never check for updates by themselves.
    development: bool,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo {
        version: update::current_version(),
        repository: update::REPO,
        development: cfg!(debug_assertions),
    }
}

#[tauri::command]
async fn system_info(state: State<'_, AppState>) -> Res<SystemInfo> {
    let s = state.inner().clone();
    blocking(move || s.system()).await
}

#[tauri::command]
fn recent_restores(state: State<'_, AppState>) -> Vec<restore::ReportSummary> {
    restore::recent_reports(&state.store.root().join("reports"), 5)
}

#[tauri::command]
async fn check_update(state: State<'_, AppState>) -> Res<UpdateInfo> {
    let s = state.inner().clone();
    blocking(move || {
        s.update_cancel.store(false, Ordering::Relaxed);
        let info = update::check(&s.update_cancel)?;
        let mut settings = s.store.settings()?;
        settings.last_update_check = Some(simple_setup_core::now());
        s.store.save_settings(&settings)?;
        let mut u = s
            .update
            .lock()
            .map_err(|_| invalid("Internal state error."))?;
        if u.0.as_ref().and_then(|i| i.latest.as_ref()) != info.latest.as_ref() {
            u.1 = None;
        }
        u.0 = Some(info.clone());
        Ok(info)
    })
    .await
}

#[derive(Clone, Serialize)]
struct UpdateProgress {
    done: u64,
    total: u64,
}

#[tauri::command]
async fn download_update(
    state: State<'_, AppState>,
    on_progress: Channel<UpdateProgress>,
) -> Res<()> {
    let s = state.inner().clone();
    blocking(move || {
        s.update_cancel.store(false, Ordering::Relaxed);
        let info = s
            .update
            .lock()
            .map_err(|_| invalid("Internal state error."))?
            .0
            .clone()
            .ok_or_else(|| invalid("Check for updates first."))?;
        let file = update::download(&info, &s.update_cancel, &mut |done, total| {
            drop(on_progress.send(UpdateProgress { done, total }))
        })?;
        s.update
            .lock()
            .map_err(|_| invalid("Internal state error."))?
            .1 = Some(file);
        Ok(())
    })
    .await
}

#[tauri::command]
fn cancel_update(state: State<'_, AppState>) {
    state.update_cancel.store(true, Ordering::Relaxed);
}

/// Installs the downloaded update, then quits (the new version starts by itself, or the system
/// installer takes over).
#[tauri::command]
async fn install_update(state: State<'_, AppState>, app: AppHandle) -> Res<()> {
    let s = state.inner().clone();
    let after = blocking(move || {
        let u = s
            .update
            .lock()
            .map_err(|_| invalid("Internal state error."))?;
        let (Some(info), Some(file)) = (&u.0, &u.1) else {
            return Err(invalid("Download the update first."));
        };
        let kind = info
            .kind
            .ok_or_else(|| invalid("This copy cannot update itself."))?;
        Ok(update::install(file, kind, Elevation::detect(false))?)
    })
    .await?;
    match after {
        AfterInstall::Relaunched | AfterInstall::QuitForInstaller => app.exit(0),
    }
    Ok(())
}

#[tauri::command]
fn save_app_edit(state: State<'_, AppState>, id: String, edit: AppEdit) -> Res<Inventory> {
    state.store.save_edit(&id, edit)?;
    state.inventory()
}

/// Saves one app's rules (preferred ways to install it, release file rules, hidden…).
#[tauri::command]
fn save_app_prefs(
    state: State<'_, AppState>,
    id: String,
    prefs: simple_setup_core::discovery::AppPrefs,
) -> Res<Inventory> {
    state.store.save_app_prefs(&id, prefs)?;
    state.inventory()
}

#[tauri::command]
fn save_custom_app(state: State<'_, AppState>, app: App) -> Res<Inventory> {
    state.store.save_custom_app(app)?;
    state.inventory()
}

#[tauri::command]
fn delete_custom_app(state: State<'_, AppState>, id: String) -> Res<Inventory> {
    state.store.delete_custom_app(&id)?;
    state.inventory()
}

#[tauri::command]
async fn measure(path: PathBuf, exclude: Vec<PathBuf>) -> Res<Measure> {
    blocking(move || Ok(inspect::measure(&path, &exclude, Duration::from_secs(4)))).await
}

// Commands below that touch the disk run off the window's thread (`blocking`): a slow or
// stale drive must not freeze the app.
#[tauri::command]
async fn list_dir(path: PathBuf) -> Res<Vec<DirItem>> {
    blocking(move || {
        if !path.is_absolute() {
            return Err(invalid("Folder paths must be absolute."));
        }
        Ok(inspect::list_dir(&path)?)
    })
    .await
}

#[derive(Serialize)]
struct KnownFolderInfo {
    folder: KnownFolder,
    path: Option<PathBuf>,
    exists: bool,
}

#[tauri::command]
async fn known_folders() -> Res<Vec<KnownFolderInfo>> {
    blocking(|| {
        Ok(KnownFolder::ALL
            .iter()
            .map(|&folder| {
                let path = folder.path();
                KnownFolderInfo {
                    folder,
                    exists: path.as_ref().is_some_and(|p| p.is_dir()),
                    path,
                }
            })
            .collect())
    })
    .await
}

#[tauri::command]
async fn volumes() -> Res<Vec<Volume>> {
    blocking(|| Ok(platform::volumes())).await
}

#[tauri::command]
async fn engines_available() -> Res<Vec<Platform>> {
    blocking(|| Ok(engines().into_keys().collect())).await
}

#[tauri::command]
async fn preflight(
    state: State<'_, AppState>,
    selection: Selection,
    destination: Option<PathBuf>,
    remote: Option<SystemInfo>,
) -> Res<Preflight> {
    let s = state.inner().clone();
    blocking(move || {
        let inv = s.inventory()?;
        Ok(build::plan(
            &inv,
            &selection,
            &home()?,
            destination.as_deref(),
            &engines(),
            &planning(&s, remote)?,
        )?
        .summary)
    })
    .await
}

/// The computers a selection is prepared for, as the build sees them: the paired computer as it
/// described itself, otherwise each target as a standard installation of its system (this
/// computer as it really is).
#[tauri::command]
async fn describe_destinations(
    state: State<'_, AppState>,
    selection: Selection,
    remote: Option<SystemInfo>,
) -> Res<Vec<SystemInfo>> {
    let s = state.inner().clone();
    blocking(move || {
        let distros = simple_setup_core::distros::Distros::load_default();
        let here = simple_setup_core::platform::Platform::current()?;
        let to = build::Destinations::new(&selection, remote.as_ref(), &distros).to;
        to.into_iter()
            .map(|d| {
                let this = remote.is_none()
                    && d.device.platform == here
                    && selection.linux_distro.is_none();
                if this { s.system() } else { Ok(d) }
            })
            .collect()
    })
    .await
}

/// What a plan or build is for: the probed destination (Transfer flow) and the user's install
/// preferences from Settings.
fn planning(s: &Inner, remote: Option<SystemInfo>) -> Res<Planning> {
    Ok(Planning {
        remote,
        prefs: Prefs::from_settings(&s.store.settings()?),
    })
}

// IPC arguments arrive as named fields from the UI.
#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn build_snapshot(
    state: State<'_, AppState>,
    selection: Selection,
    name: String,
    destination: PathBuf,
    password: Option<String>,
    base: Option<PathBuf>,
    remote: Option<SystemInfo>,
    on_progress: Channel<Progress>,
) -> Res<BuildReport> {
    let s = state.inner().clone();
    blocking(move || {
        let planning = planning(&s, remote)?;
        if !destination.is_dir() {
            return Err(invalid(
                "The destination folder is not available. Is the drive connected?",
            ));
        }
        let inv = s.inventory()?;
        let (home, engines) = (home()?, engines());
        s.build_cancel.store(false, Ordering::Relaxed);
        let req = BuildRequest {
            inventory: &inv,
            selection: &selection,
            name: &name,
            destination: &destination,
            engines: &engines,
            home: &home,
            password: password.as_deref().filter(|p| !p.is_empty()),
            base: base.as_deref(),
            planning: &planning,
        };
        let report = build::build(&req, &mut |p| drop(on_progress.send(p)), &s.build_cancel)?;
        s.store.remember_snapshot(SnapshotRef {
            path: report.path.clone(),
            id: report.id.clone(),
            name: report.name.clone(),
            created_at: simple_setup_core::now(),
            last_verified: Some(simple_setup_core::now()),
            last_verify_ok: Some(true),
        })?;
        Ok(report)
    })
    .await
}

#[tauri::command]
fn cancel_build(state: State<'_, AppState>) {
    state.build_cancel.store(true, Ordering::Relaxed);
}

#[tauri::command]
fn discard_partial(destination: PathBuf, name: String) -> Res<()> {
    Ok(build::discard_partial(&destination, &name)?)
}

#[tauri::command]
fn list_profiles(state: State<'_, AppState>) -> Res<Vec<Profile>> {
    Ok(state.store.profiles()?)
}

#[tauri::command]
fn save_profile(state: State<'_, AppState>, name: String, selection: Selection) -> Res<Profile> {
    Ok(state.store.save_profile(&name, &selection)?)
}

#[tauri::command]
fn delete_profile(state: State<'_, AppState>, name: String) -> Res<()> {
    Ok(state.store.delete_profile(&name)?)
}

#[derive(Serialize)]
struct SnapshotItem {
    reference: SnapshotRef,
    summary: Option<Summary>,
    error: Option<String>,
    /// The history it belongs to (the ID of its oldest known predecessor).
    history: Option<String>,
}

#[tauri::command]
async fn list_snapshots(state: State<'_, AppState>) -> Res<Vec<SnapshotItem>> {
    let s = state.inner().clone();
    blocking(move || {
        Ok(s.store
            .snapshots()?
            .into_iter()
            .map(|r| match snapshot::summary(&r.path) {
                Ok(sum) => SnapshotItem {
                    reference: r,
                    summary: Some(sum),
                    error: None,
                    history: None,
                },
                Err(e) => SnapshotItem {
                    reference: r,
                    summary: None,
                    error: Some(format!(
                        "Not available: {e}. Connect the drive that holds it."
                    )),
                    history: None,
                },
            })
            .collect::<Vec<_>>())
        .map(|mut items: Vec<SnapshotItem>| {
            let sums: Vec<Summary> = items.iter().filter_map(|i| i.summary.clone()).collect();
            let h = snapshot::history(&sums);
            for i in &mut items {
                i.history = i.summary.as_ref().and_then(|s| h.get(&s.id).cloned());
            }
            items
        })
    })
    .await
}

#[tauri::command]
async fn open_snapshot(state: State<'_, AppState>, path: PathBuf) -> Res<Summary> {
    let s = state.inner().clone();
    blocking(move || {
        let sum = snapshot::summary(&path)?;
        let known = s.store.snapshots()?.into_iter().find(|r| r.path == path);
        s.store.remember_snapshot(SnapshotRef {
            path: path.clone(),
            id: sum.id.clone(),
            name: sum.name.clone(),
            created_at: sum.created_at.clone(),
            last_verified: known.as_ref().and_then(|k| k.last_verified.clone()),
            last_verify_ok: known.and_then(|k| k.last_verify_ok),
        })?;
        Ok(sum)
    })
    .await
}

/// The full summary of an encrypted snapshot (apps, folders, choices), with its password.
#[tauri::command]
async fn unlock_summary(path: PathBuf, secret: String) -> Res<Summary> {
    blocking(move || {
        let key = Manifest::read(&path)?.unlock(&path, Some(&secret))?;
        Ok(snapshot::summary_with(&path, key.as_ref())?)
    })
    .await
}

#[tauri::command]
async fn verify_snapshot(
    state: State<'_, AppState>,
    path: PathBuf,
    on_progress: Channel<Progress>,
) -> Res<VerifyReport> {
    let s = state.inner().clone();
    blocking(move || {
        s.restore_cancel.store(false, Ordering::Relaxed);
        let r = snapshot::verify(&path, &mut |p| drop(on_progress.send(p)), &s.restore_cancel)?;
        s.store.update_snapshot(&path, |x| {
            x.last_verified = Some(simple_setup_core::now());
            x.last_verify_ok = Some(r.ok);
        })?;
        Ok(r)
    })
    .await
}

/// Deletes the older versions of a snapshot's history beyond the newest `keep`, after the
/// person typed "delete".
#[tauri::command]
async fn prune_history(
    state: State<'_, AppState>,
    path: PathBuf,
    keep: usize,
    confirm: String,
) -> Res<Vec<PathBuf>> {
    let s = state.inner().clone();
    blocking(move || {
        if confirm.trim() != "delete" {
            return Err(invalid("Type delete to confirm; nothing was deleted."));
        }
        let known: Vec<PathBuf> = s.store.snapshots()?.into_iter().map(|r| r.path).collect();
        let gone = snapshot::prune(&path, &known, keep)?;
        for p in &gone {
            s.store.forget_snapshot(p)?;
        }
        Ok(gone)
    })
    .await
}

/// Packs a snapshot into one .tar.gz file.
#[tauri::command]
async fn pack_snapshot(path: PathBuf, to: PathBuf, on_progress: Channel<Progress>) -> Res<()> {
    blocking(move || {
        Ok(snapshot::pack(
            &path,
            &to,
            &mut |p| drop(on_progress.send(p)),
            &AtomicBool::new(false),
        )?)
    })
    .await
}

/// Unpacks a packed snapshot into `dest`, checks it and remembers it.
#[tauri::command]
async fn unpack_snapshot(
    state: State<'_, AppState>,
    archive: PathBuf,
    dest: PathBuf,
    on_progress: Channel<Progress>,
) -> Res<PathBuf> {
    let s = state.inner().clone();
    blocking(move || {
        let path = snapshot::unpack(
            &archive,
            &dest,
            &mut |p| drop(on_progress.send(p)),
            &AtomicBool::new(false),
        )?;
        let m = Manifest::read(&path)?;
        s.store.remember_snapshot(SnapshotRef {
            path: path.clone(),
            id: m.id,
            name: m.name,
            created_at: m.created_at,
            last_verified: Some(simple_setup_core::now()),
            last_verify_ok: Some(true),
        })?;
        Ok(path)
    })
    .await
}

#[tauri::command]
async fn delete_snapshot(
    state: State<'_, AppState>,
    path: PathBuf,
    confirm_name: String,
) -> Res<()> {
    let s = state.inner().clone();
    blocking(move || {
        let sum = snapshot::summary(&path)?;
        if sum.name != confirm_name {
            return Err(invalid(
                "The name you typed does not match; nothing was deleted.",
            ));
        }
        snapshot::delete(&path)?;
        Ok(s.store.forget_snapshot(&path)?)
    })
    .await
}

/// This user as the restore target; `secret` (password or recovery key) opens an encrypted
/// snapshot. Without it an encrypted snapshot fails with the `locked` error code.
fn target(s: &Inner, path: &std::path::Path, secret: Option<&str>) -> Res<Target> {
    let settings = s.store.settings()?;
    let mut t = Target::with_info(
        Some(s.store.root().join("reports")),
        Elevation::detect(false),
        s.system()?,
    )?;
    t.online &= settings.restore_online;
    t.prefs = Prefs::from_settings(&settings);
    t.key = Manifest::read(path)?.unlock(path, secret.filter(|p| !p.is_empty()))?;
    Ok(t)
}

#[tauri::command]
async fn restore_plan(
    state: State<'_, AppState>,
    path: PathBuf,
    secret: Option<String>,
) -> Res<RestorePlan> {
    let s = state.inner().clone();
    blocking(move || {
        Ok(restore::plan(
            &path,
            &target(&s, &path, secret.as_deref())?,
        )?)
    })
    .await
}

/// Writes a standalone install script for restoring `path` on this computer (POSIX sh, or
/// PowerShell on Windows) to `to`.
#[tauri::command]
async fn export_script(
    state: State<'_, AppState>,
    path: PathBuf,
    to: PathBuf,
    secret: Option<String>,
) -> Res<()> {
    let s = state.inner().clone();
    blocking(move || {
        let mut t = target(&s, &path, secret.as_deref())?;
        t.online = true;
        let shell = simple_setup_core::script::Shell::for_os(t.device.platform.os);
        let text = simple_setup_core::script::export(&path, &t, shell)?;
        std::fs::write(&to, text)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755));
        }
        Ok(())
    })
    .await
}

/// The files of one folder item of a snapshot, to choose which to restore.
#[tauri::command]
async fn restore_files(
    state: State<'_, AppState>,
    path: PathBuf,
    secret: Option<String>,
    item: String,
) -> Res<Vec<(String, u64)>> {
    let s = state.inner().clone();
    blocking(move || {
        Ok(restore::item_files(
            &path,
            &target(&s, &path, secret.as_deref())?,
            &item,
        )?)
    })
    .await
}

#[tauri::command]
async fn restore_execute(
    state: State<'_, AppState>,
    path: PathBuf,
    choice: RestoreChoice,
    secret: Option<String>,
    on_progress: Channel<Progress>,
) -> Res<RestoreReport> {
    let s = state.inner().clone();
    blocking(move || {
        s.restore_cancel.store(false, Ordering::Relaxed);
        Ok(restore::execute(
            &path,
            &target(&s, &path, secret.as_deref())?,
            &choice,
            &mut |p| drop(on_progress.send(p)),
            &s.restore_cancel,
        )?)
    })
    .await
}

#[tauri::command]
fn cancel_restore(state: State<'_, AppState>) {
    state.restore_cancel.store(true, Ordering::Relaxed);
}

#[tauri::command]
async fn find_receivers() -> Res<Vec<Peer>> {
    blocking(|| Ok(transfer::discover(Duration::from_secs(3))?)).await
}

#[tauri::command]
async fn send_snapshot(
    state: State<'_, AppState>,
    path: PathBuf,
    address: String,
    code: String,
    on_progress: Channel<Progress>,
) -> Res<SendReport> {
    let s = state.inner().clone();
    blocking(move || {
        s.send_cancel.store(false, Ordering::Relaxed);
        s.send_control.pause.store(false, Ordering::Relaxed);
        s.send_control
            .limit
            .store(s.store.settings()?.send_limit, Ordering::Relaxed);
        let to = transfer::parse_address(&address)?;
        let name = Manifest::read(&path).map_or_else(|_| path.display().to_string(), |m| m.name);
        // The person chose this computer and typed its code; the receiver confirms the offer.
        let r = transfer::send(
            &path,
            to,
            &code,
            &mut |_| true,
            &mut |p| drop(on_progress.send(p)),
            &s.send_cancel,
            &s.send_control,
        );
        let record = match &r {
            Ok(r) => TransferRecord {
                at: simple_setup_core::now(),
                direction: Direction::Sent,
                snapshot: name,
                peer: r.receiver.hostname.clone(),
                files: r.files,
                bytes: r.bytes,
                path: None,
                outcome: None,
            },
            Err(e) => TransferRecord {
                at: simple_setup_core::now(),
                direction: Direction::Sent,
                snapshot: name,
                peer: address.clone(),
                files: 0,
                bytes: 0,
                path: None,
                outcome: Some(e.to_string()),
            },
        };
        s.store.record_transfer(record)?;
        Ok(r?)
    })
    .await
}

/// Asks a receiving computer what it is (after pairing with its code), to prepare for it.
#[tauri::command]
async fn probe_receiver(address: String, code: String) -> Res<Probed> {
    blocking(move || Ok(transfer::probe(transfer::parse_address(&address)?, &code)?)).await
}

/// Where snapshots made only to be sent are built (removed after they arrive).
#[tauri::command]
fn outgoing_folder() -> Res<PathBuf> {
    Ok(build::outgoing_dir()?)
}

/// Pauses (closes the connection, keeping what arrived) or resumes the send in progress.
#[tauri::command]
fn pause_send(state: State<'_, AppState>, paused: bool) {
    state.send_control.pause.store(paused, Ordering::Relaxed);
}

/// Changes the speed limit of the send in progress (bytes per second, 0: none).
#[tauri::command]
fn set_send_limit(state: State<'_, AppState>, limit: u64) {
    state.send_control.limit.store(limit, Ordering::Relaxed);
}

#[tauri::command]
fn list_transfers(state: State<'_, AppState>) -> Res<Vec<TransferRecord>> {
    Ok(state.store.transfers()?)
}

/// The GitHub token lives in the system keyring, never in a file Simple Setup writes, and is
/// handed to the engine in memory.
fn token_entry() -> Res<keyring::Entry> {
    keyring::Entry::new("simple-setup", "github-token")
        .map_err(|e| invalid(format!("The system keyring is not available: {e}")))
}

#[derive(serde::Serialize)]
struct TokenStatus {
    /// A token is saved in the system keyring.
    stored: bool,
    /// Requests to GitHub carry a token (the saved one, GITHUB_TOKEN or the GitHub CLI's).
    active: bool,
    /// Why the keyring could not be read, if it could not.
    problem: Option<String>,
}

#[tauri::command]
async fn github_token_status() -> Res<TokenStatus> {
    blocking(|| {
        let (stored, problem) = match token_entry().and_then(|e| {
            e.get_password().map(|_| true).or_else(|e| match e {
                keyring::Error::NoEntry => Ok(false),
                e => Err(invalid(e.to_string())),
            })
        }) {
            Ok(s) => (s, None),
            Err(e) => (false, Some(e.message)),
        };
        Ok(TokenStatus {
            stored,
            active: simple_setup_core::fetch::has_github_token(),
            problem,
        })
    })
    .await
}

/// Saves a GitHub token (for private repositories and a higher request limit).
#[tauri::command]
async fn save_github_token(token: String) -> Res<()> {
    blocking(move || {
        let token = token.trim().to_string();
        if token.is_empty()
            || token.len() > 255
            || !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return Err(invalid(
                "That is not a GitHub token (letters, digits and _ only).",
            ));
        }
        token_entry()?
            .set_password(&token)
            .map_err(|e| invalid(format!("Saving to the system keyring failed: {e}")))?;
        simple_setup_core::fetch::set_github_token(Some(token));
        Ok(())
    })
    .await
}

#[tauri::command]
async fn delete_github_token() -> Res<()> {
    blocking(|| {
        match token_entry()?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(e) => return Err(invalid(format!("Removing it from the keyring failed: {e}"))),
        }
        simple_setup_core::fetch::set_github_token(None);
        Ok(())
    })
    .await
}

/// An app's icon as a data URL, for apps in the latest scan only (never an arbitrary file).
#[tauri::command]
async fn app_icon(state: State<'_, AppState>, id: String) -> Res<Option<String>> {
    let s = state.inner().clone();
    blocking(move || {
        use base64::Engine;
        let icon = s
            .inventory()?
            .apps
            .into_iter()
            .find(|a| a.id == id)
            .and_then(|a| a.icon);
        Ok(icon
            .and_then(|p| inspect::icon_image(&p))
            .map(|(mime, bytes)| {
                format!(
                    "data:{mime};base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(bytes)
                )
            }))
    })
    .await
}

/// Writes the diagnostic log and a description of this computer to `to` (a file the user chose).
#[tauri::command]
async fn export_diagnostics(state: State<'_, AppState>, to: PathBuf) -> Res<PathBuf> {
    let s = state.inner().clone();
    blocking(move || {
        let system = serde_json::to_string_pretty(&s.system()?).unwrap_or_default();
        Ok(simple_setup_core::diag::export(&to, &system)?)
    })
    .await
}

/// Saves a restore report (from this computer's reports folder) as JSON or, for a `.txt`
/// name, as text.
#[tauri::command]
fn export_report(state: State<'_, AppState>, file: PathBuf, to: PathBuf) -> Res<()> {
    let dir = state.store.root().join("reports").canonicalize()?;
    let file = file.canonicalize()?;
    if !file.starts_with(&dir) {
        return Err(invalid("Only restore reports can be exported."));
    }
    let report: restore::RestoreReport = serde_json::from_slice(&std::fs::read(&file)?)
        .map_err(|e| invalid(format!("The report is damaged: {e}")))?;
    let text = if to
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("txt"))
    {
        restore::report_text(&report)
    } else {
        serde_json::to_string_pretty(&report).map_err(|e| invalid(e.to_string()))?
    };
    std::fs::write(&to, text)?;
    Ok(())
}

// ---------------------------------------------------------------- distribution profiles

#[derive(Serialize)]
struct DistroList {
    profiles: Vec<Listed>,
    version: String,
    builtin_version: String,
    official_version: Option<String>,
    problems: Vec<String>,
}

/// The processor table, read-only.
#[tauri::command]
fn list_arches() -> Vec<(
    simple_setup_core::platform::Arch,
    simple_setup_core::platform::ArchInfo,
)> {
    simple_setup_core::platform::Arch::ALL
        .into_iter()
        .map(|a| (a, a.info().clone()))
        .collect()
}

#[tauri::command]
async fn list_distros() -> Res<DistroList> {
    blocking(|| {
        let d = UserStore::open_default()?.distros();
        Ok(DistroList {
            profiles: d.list(),
            version: d.current_version().to_string(),
            builtin_version: distros::builtin_version().to_string(),
            official_version: d.official_version().map(String::from),
            problems: d.load_problems.clone(),
        })
    })
    .await
}

#[tauri::command]
async fn distro_details(id: String) -> Res<(Option<DistroProfile>, Resolved)> {
    blocking(move || {
        let d = UserStore::open_default()?.distros();
        let raw = d.get(&id).map(|(p, _)| p.clone());
        Ok((raw, d.resolve(&id)?))
    })
    .await
}

/// Reads a profile file the user picked (JSON, or YAML by extension), without saving it.
#[tauri::command]
async fn read_distro_file(path: PathBuf) -> Res<ProfileFile> {
    blocking(move || Ok(distros::read_file(&path)?)).await
}

/// Validates profiles and reports how the first behaves on this computer.
#[tauri::command]
async fn test_distros(file: ProfileFile) -> Res<TestReport> {
    blocking(move || {
        let d = UserStore::open_default()?.distros();
        Ok(distros::test(
            &file,
            &d,
            platform::OsRelease::read().as_ref(),
        ))
    })
    .await
}

/// Adds or replaces the user's profiles (import, the guided wizard, edits).
#[tauri::command]
fn save_distros(state: State<'_, AppState>, file: ProfileFile) -> Res<Vec<String>> {
    let ids = UserStore::open_default()?.import(&file)?;
    state.forget_system();
    Ok(ids)
}

#[tauri::command]
async fn export_distros(ids: Vec<String>, to: PathBuf) -> Res<()> {
    blocking(move || {
        let f = UserStore::open_default()?.export(&ids)?;
        std::fs::write(
            to,
            serde_json::to_string_pretty(&f).map_err(|e| invalid(e.to_string()))? + "\n",
        )?;
        Ok(())
    })
    .await
}

#[tauri::command]
fn set_distro_enabled(state: State<'_, AppState>, id: String, enabled: bool) -> Res<()> {
    UserStore::open_default()?.set_enabled(&id, enabled)?;
    state.forget_system();
    Ok(())
}

#[tauri::command]
fn delete_distro(state: State<'_, AppState>, id: String) -> Res<()> {
    UserStore::open_default()?.remove(&id)?;
    state.forget_system();
    Ok(())
}

#[tauri::command]
fn trust_distro(state: State<'_, AppState>, id: String, trust: bool) -> Res<()> {
    UserStore::open_default()?.trust(&id, trust)?;
    state.forget_system();
    Ok(())
}

#[tauri::command]
async fn update_distros(state: State<'_, AppState>) -> Res<distros::UpdateResult> {
    let s = state.inner().clone();
    blocking(move || {
        let r = distros::update_official(&UserStore::open_default()?, &AtomicBool::new(false))?;
        s.forget_system();
        Ok(r)
    })
    .await
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ReceiveEvent {
    Ready {
        code: String,
        address: Option<String>,
        announce_error: Option<String>,
    },
    Offer {
        from: Device,
        name: String,
        files: usize,
        bytes: u64,
        encrypted: bool,
        targets: Vec<Platform>,
    },
    Progress {
        progress: Progress,
    },
}

/// Receives one snapshot into `destination`; returns its folder. Listens (and announces this
/// computer) only while this runs.
#[tauri::command]
async fn receive_snapshot(
    state: State<'_, AppState>,
    destination: PathBuf,
    on_event: Channel<ReceiveEvent>,
) -> Res<PathBuf> {
    let s = state.inner().clone();
    blocking(move || {
        s.receive_cancel.store(false, Ordering::Relaxed);
        if !destination.is_dir() {
            return Err(invalid("Choose an existing folder to receive into."));
        }
        let r = transfer::Receiver::start(true, s.store.settings()?.receive_port)?;
        let _ = on_event.send(ReceiveEvent::Ready {
            code: r.code.clone(),
            address: transfer::local_address().map(|ip| format!("{ip}:{}", r.port)),
            announce_error: r.announce_error.clone(),
        });
        // What was offered and accepted, for the history if it then fails.
        let mut taken: Option<(String, String, u64, u64)> = None;
        let path = r.run(
            &destination,
            &mut |from, offer| {
                taken = Some((
                    offer.name.clone(),
                    from.hostname.clone(),
                    offer.files.len() as u64,
                    offer.bytes(),
                ));
                let (tx, rx) = std::sync::mpsc::channel();
                if let Ok(mut a) = s.offer_answer.lock() {
                    *a = Some(tx);
                }
                let _ = on_event.send(ReceiveEvent::Offer {
                    from: from.clone(),
                    name: offer.name.clone(),
                    files: offer.files.len(),
                    bytes: offer.bytes(),
                    encrypted: offer.encrypted,
                    targets: offer.targets.clone(),
                });
                rx.recv().unwrap_or(false)
            },
            &mut |p| drop(on_event.send(ReceiveEvent::Progress { progress: p })),
            &s.receive_cancel,
        );
        let path = match path {
            Ok(p) => p,
            Err(e) => {
                if let Some((snapshot, peer, files, bytes)) = taken {
                    let _ = s.store.record_transfer(TransferRecord {
                        at: simple_setup_core::now(),
                        direction: Direction::Received,
                        snapshot,
                        peer,
                        files,
                        bytes,
                        path: None,
                        outcome: Some(e.to_string()),
                    });
                }
                return Err(e.into());
            }
        };
        let m = Manifest::read(&path)?;
        s.store.record_transfer(TransferRecord {
            at: simple_setup_core::now(),
            direction: Direction::Received,
            snapshot: m.name.clone(),
            peer: m.source.hostname.clone(),
            files: m.files.len() as u64,
            bytes: m.total_bytes,
            path: Some(path.clone()),
            outcome: None,
        })?;
        s.store.remember_snapshot(SnapshotRef {
            path: path.clone(),
            id: m.id,
            name: m.name,
            created_at: m.created_at,
            last_verified: Some(simple_setup_core::now()),
            last_verify_ok: Some(true),
        })?;
        Ok(path)
    })
    .await
}

#[tauri::command]
fn answer_offer(state: State<'_, AppState>, accept: bool) -> Res<()> {
    let tx = state
        .offer_answer
        .lock()
        .map_err(|_| invalid("Internal state error."))?
        .take();
    tx.ok_or_else(|| invalid("Nothing is waiting for an answer."))?
        .send(accept)
        .map_err(|_| invalid("The transfer already ended."))
}

#[tauri::command]
fn cancel_transfer(state: State<'_, AppState>, direction: Option<String>) {
    match direction.as_deref() {
        Some("send") => state.send_cancel.store(true, Ordering::Relaxed),
        Some("receive") => state.receive_cancel.store(true, Ordering::Relaxed),
        _ => {
            state.send_cancel.store(true, Ordering::Relaxed);
            state.receive_cancel.store(true, Ordering::Relaxed);
        }
    }
    if let Ok(mut a) = state.offer_answer.lock()
        && let Some(tx) = a.take()
    {
        let _ = tx.send(false);
    }
}

fn main() {
    let store = match Store::open_default() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Simple Setup cannot start: {e}");
            std::process::exit(1);
        }
    };
    let state: AppState = Arc::new(Inner {
        store,
        raw: Mutex::new(None),
        build_cancel: AtomicBool::new(false),
        restore_cancel: AtomicBool::new(false),
        send_cancel: AtomicBool::new(false),
        send_control: transfer::Control::default(),
        receive_cancel: AtomicBool::new(false),
        offer_answer: Mutex::new(None),
        system: Mutex::new(None),
        update: Mutex::new((None, None)),
        update_cancel: AtomicBool::new(false),
    });
    // The previous version kept beside an updated AppImage or app bundle is no longer needed
    // once this one has started.
    update::cleanup();
    if let Some(d) = simple_setup_core::diag::default_dir() {
        simple_setup_core::diag::enable(d);
    }
    // A locked keyring may ask to be unlocked: read it off the main thread.
    std::thread::spawn(|| {
        if let Ok(t) =
            token_entry().and_then(|e| e.get_password().map_err(|e| invalid(e.to_string())))
        {
            simple_setup_core::fetch::set_github_token(Some(t));
        }
    });
    let result = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            device_info,
            scan,
            get_settings,
            save_settings,
            save_app_edit,
            save_app_prefs,
            save_custom_app,
            delete_custom_app,
            measure,
            list_dir,
            known_folders,
            volumes,
            engines_available,
            preflight,
            build_snapshot,
            cancel_build,
            discard_partial,
            list_profiles,
            save_profile,
            delete_profile,
            list_snapshots,
            open_snapshot,
            verify_snapshot,
            delete_snapshot,
            prune_history,
            pack_snapshot,
            unpack_snapshot,
            restore_plan,
            restore_execute,
            cancel_restore,
            unlock_summary,
            find_receivers,
            send_snapshot,
            receive_snapshot,
            answer_offer,
            cancel_transfer,
            app_info,
            system_info,
            recent_restores,
            check_update,
            download_update,
            cancel_update,
            export_diagnostics,
            describe_destinations,
            app_icon,
            github_token_status,
            save_github_token,
            delete_github_token,
            install_update,
            probe_receiver,
            outgoing_folder,
            list_transfers,
            export_report,
            list_distros,
            list_arches,
            pause_send,
            set_send_limit,
            export_script,
            restore_files,
            distro_details,
            read_distro_file,
            test_distros,
            save_distros,
            export_distros,
            set_distro_enabled,
            delete_distro,
            trust_distro,
            update_distros,
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("Simple Setup failed: {e}");
        std::process::exit(1);
    }
}
