//! Simple Setup desktop shell: a thin IPC adapter over `simple-setup-core`. Every operation
//! the UI offers runs the same engine code as the CLI; nothing here simulates results.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use simple_setup_core::build::{self, BuildReport, BuildRequest, Engines, Preflight, Progress};
use simple_setup_core::discovery::{self, App, Inventory, ScanOptions};
use simple_setup_core::inspect::{self, DirItem, Measure};
use simple_setup_core::installers::Elevation;
use simple_setup_core::manifest::Manifest;
use simple_setup_core::platform::{self, Device, Platform, Volume};
use simple_setup_core::probe::{self, SystemInfo};
use simple_setup_core::restore::{self, RestoreChoice, RestorePlan, RestoreReport, Target};
use simple_setup_core::selection::{KnownFolder, Selection};
use simple_setup_core::snapshot::{self, Summary, VerifyReport};
use simple_setup_core::store::{AppEdit, Profile, Settings, SnapshotRef, Store, UserApps};
use simple_setup_core::transfer::{self, Peer, SendReport};
use tauri::State;
use tauri::ipc::Channel;

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
    transfer_cancel: AtomicBool,
    /// Where the person's answer to an incoming offer goes while receiving.
    offer_answer: Mutex<Option<std::sync::mpsc::Sender<bool>>>,
    /// What this computer offers, looked up at most every two minutes (the internet check
    /// can take a few seconds when offline).
    system: Mutex<Option<(std::time::Instant, SystemInfo)>>,
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
    for d in &settings.source_dirs {
        if !d.is_absolute() || !d.is_dir() {
            return Err(invalid(format!("{} is not a folder.", d.display())));
        }
    }
    Ok(state.store.save_settings(&settings)?)
}

#[tauri::command]
fn save_app_edit(state: State<'_, AppState>, id: String, edit: AppEdit) -> Res<Inventory> {
    state.store.save_edit(&id, edit)?;
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

#[tauri::command]
fn list_dir(path: PathBuf) -> Res<Vec<DirItem>> {
    if !path.is_absolute() {
        return Err(invalid("Folder paths must be absolute."));
    }
    Ok(inspect::list_dir(&path)?)
}

#[derive(Serialize)]
struct KnownFolderInfo {
    folder: KnownFolder,
    path: Option<PathBuf>,
    exists: bool,
}

#[tauri::command]
fn known_folders() -> Vec<KnownFolderInfo> {
    KnownFolder::ALL
        .iter()
        .map(|&folder| {
            let path = folder.path();
            KnownFolderInfo {
                folder,
                exists: path.as_ref().is_some_and(|p| p.is_dir()),
                path,
            }
        })
        .collect()
}

#[tauri::command]
fn volumes() -> Vec<Volume> {
    platform::volumes()
}

#[tauri::command]
fn engines_available() -> Vec<Platform> {
    engines().into_keys().collect()
}

#[tauri::command]
async fn preflight(
    state: State<'_, AppState>,
    selection: Selection,
    destination: Option<PathBuf>,
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
        )?
        .summary)
    })
    .await
}

#[tauri::command]
async fn build_snapshot(
    state: State<'_, AppState>,
    selection: Selection,
    name: String,
    destination: PathBuf,
    password: Option<String>,
    base: Option<PathBuf>,
    on_progress: Channel<Progress>,
) -> Res<BuildReport> {
    let s = state.inner().clone();
    blocking(move || {
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
                },
                Err(e) => SnapshotItem {
                    reference: r,
                    summary: None,
                    error: Some(format!(
                        "Not available: {e}. Connect the drive that holds it."
                    )),
                },
            })
            .collect())
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
    t.allow_source = settings.allow_source_builds;
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
        s.transfer_cancel.store(false, Ordering::Relaxed);
        let to = transfer::parse_address(&address)?;
        // The person chose this computer and typed its code; the receiver confirms the offer.
        Ok(transfer::send(
            &path,
            to,
            &code,
            &mut |_| true,
            &mut |p| drop(on_progress.send(p)),
            &s.transfer_cancel,
        )?)
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
        s.transfer_cancel.store(false, Ordering::Relaxed);
        if !destination.is_dir() {
            return Err(invalid("Choose an existing folder to receive into."));
        }
        let r = transfer::Receiver::start(true)?;
        let _ = on_event.send(ReceiveEvent::Ready {
            code: r.code.clone(),
            address: transfer::local_address().map(|ip| format!("{ip}:{}", r.port)),
            announce_error: r.announce_error.clone(),
        });
        let path = r.run(
            &destination,
            &mut |from, offer| {
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
            &s.transfer_cancel,
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
fn cancel_transfer(state: State<'_, AppState>) {
    state.transfer_cancel.store(true, Ordering::Relaxed);
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
        transfer_cancel: AtomicBool::new(false),
        offer_answer: Mutex::new(None),
        system: Mutex::new(None),
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
            restore_plan,
            restore_execute,
            cancel_restore,
            unlock_summary,
            find_receivers,
            send_snapshot,
            receive_snapshot,
            answer_offer,
            cancel_transfer,
        ])
        .run(tauri::generate_context!());
    if let Err(e) = result {
        eprintln!("Simple Setup failed: {e}");
        std::process::exit(1);
    }
}
