mod bridge;
mod category;
mod cli;
mod grabber;
mod history;
mod i18n;
mod manager;
mod post;
mod secret;
mod site;
mod tools;
mod updater;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use dm_engine::checksum::Algo;
use dm_engine::{Options, Progress};
use manager::{AddRequest, DoneAction, EventSink, Item, Manager, OpenWhenDone, Priority, ProbeResult, Settings};
use serde::Serialize;
use manager::Status;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager as _, State, WebviewUrl, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_clipboard_manager::ClipboardExt;
#[cfg(not(windows))]
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_opener::OpenerExt;

type Mgr<'a> = State<'a, Arc<Manager>>;
type Hist<'a> = State<'a, Arc<history::History>>;

/// Forwards manager events to the webview.
struct WindowEvents {
    app: AppHandle,
    /// Downloads whose progress window was opened during this run.
    windows_opened: Mutex<HashSet<u64>>,
}

/// The language for texts the app shows itself.
fn lang(app: &AppHandle) -> &'static str {
    app.try_state::<Arc<Manager>>().map_or("en", |m| i18n::resolve(&m.settings().language))
}

fn progress_label(id: u64) -> String {
    format!("progress-{id}")
}

/// A small window with one download's progress, like IDM's.
fn open_progress(app: &AppHandle, id: u64, name: &str) {
    let label = progress_label(id);
    if let Some(w) = app.get_webview_window(&label) {
        let _ = w.set_focus();
        return;
    }
    let built = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(format!("index.html?progress={id}").into()))
        .title(name)
        .inner_size(460.0, 300.0)
        .min_inner_size(380.0, 260.0)
        .build();
    if let Ok(w) = built {
        let _ = w.set_icon(app_icon());
    }
}

#[derive(Clone, Serialize)]
struct ProgressEvent<'a> {
    id: u64,
    #[serde(flatten)]
    progress: &'a Progress,
}

/// A download handed over by the browser extension, plus the request headers
/// (cookies, referrer, user agent) the UI must pass back with `add_download`.
#[derive(Clone, Serialize)]
struct CapturedEvent {
    #[serde(flatten)]
    capture: bridge::CapturedDownload,
    headers: Vec<(String, String)>,
}

impl EventSink for WindowEvents {
    fn item(&self, item: &Item) {
        let _ = self.app.emit("item", item);
        if item.status == Status::Downloading {
            let wanted = self.app.try_state::<Arc<Manager>>().is_some_and(|m| m.settings().progress_windows);
            if wanted && self.windows_opened.lock().unwrap().insert(item.id) {
                open_progress(&self.app, item.id, &item.file_name);
            }
        } else if item.status != Status::Queued {
            self.windows_opened.lock().unwrap().remove(&item.id);
        }
    }

    fn finished(&self, item: &Item) {
        let lang = lang(&self.app);
        let note = match item.status {
            Status::Completed => Some((i18n::t(lang, "complete", ""), item.file_name.clone())),
            Status::Error => Some((
                i18n::t(lang, "failed", ""),
                format!("{}: {}", item.file_name, item.error.as_deref().unwrap_or("unknown error")),
            )),
            _ => None,
        };
        if let Some((title, body)) = note {
            notify(&self.app, &title, &body);
        }
    }

    fn progress(&self, id: u64, progress: &Progress) {
        let _ = self.app.emit("progress", ProgressEvent { id, progress });
    }

    fn removed(&self, id: u64) {
        let _ = self.app.emit("removed", id);
        if let Some(w) = self.app.get_webview_window(&progress_label(id)) {
            let _ = w.close();
        }
    }

    fn open(&self, item: &Item) {
        let opener = self.app.opener();
        match item.open_when_done {
            OpenWhenDone::File => drop(opener.open_path(item.save_path.to_string_lossy(), None::<&str>)),
            OpenWhenDone::Folder => drop(opener.reveal_item_in_dir(&item.save_path)),
            // Same cancellable countdown as "when all downloads finish".
            OpenWhenDone::Shutdown | OpenWhenDone::Sleep => {
                let action = if item.open_when_done == OpenWhenDone::Shutdown { DoneAction::Shutdown } else { DoneAction::Sleep };
                show_main(&self.app);
                let _ = self.app.emit("all-done", action);
            }
            OpenWhenDone::Command => {
                if let Err(e) = post::run_command(item.done_command.as_deref().unwrap_or(""), &post::Vars::of(&item)) {
                    notify(&self.app, &i18n::t(lang(&self.app), "run_failed", ""), &e);
                }
            }
            OpenWhenDone::Nothing => {}
        }
    }

    fn idle(&self) {
        let Some(mgr) = self.app.try_state::<Arc<Manager>>() else { return };
        let settings = mgr.settings();
        match settings.when_done {
            DoneAction::Nothing => {}
            DoneAction::Run => {
                if let Err(e) = run_program(&settings.when_done_program) {
                    notify(&self.app, &i18n::t(lang(&self.app), "run_failed", ""), &e);
                }
            }
            // Power actions get a countdown in the window that the user can cancel.
            action => {
                show_main(&self.app);
                let _ = self.app.emit("all-done", action);
            }
        }
    }
}

/// Start the user's "when done" program, detached.
fn run_program(command: &str) -> Result<(), String> {
    let parts = post::split_command(command);
    let (program, args) = parts.split_first().ok_or("no program set")?;
    std::process::Command::new(program).args(args).spawn().map(drop).map_err(|e| format!("{program}: {e}"))
}

#[cfg(windows)]
fn suspend(hibernate: bool) -> Result<(), String> {
    // SAFETY: plain Win32 call without pointer arguments.
    let ok = unsafe { windows_sys::Win32::System::Power::SetSuspendState(hibernate, false, false) };
    if ok {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(windows)]
fn shutdown_pc() -> Result<(), String> {
    let exe = std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("shutdown.exe"))
        .unwrap_or_else(|| "shutdown".into());
    std::process::Command::new(exe).args(["/s", "/t", "0"]).spawn().map(drop).map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn suspend(_hibernate: bool) -> Result<(), String> {
    Err("not supported on this system".into())
}

#[cfg(not(windows))]
fn shutdown_pc() -> Result<(), String> {
    Err("not supported on this system".into())
}

/// Called by the window when the "all downloads finished" countdown ends.
#[tauri::command]
fn run_done_action(app: AppHandle, mgr: Mgr, action: Option<DoneAction>) -> Result<(), String> {
    // A download's own "when done" passes its action; otherwise the setting.
    match action.unwrap_or(mgr.settings().when_done) {
        DoneAction::Nothing => Ok(()),
        DoneAction::Exit => {
            quit(&app);
            Ok(())
        }
        DoneAction::Run => run_program(&mgr.settings().when_done_program),
        DoneAction::Sleep => suspend(false),
        DoneAction::Hibernate => suspend(true),
        DoneAction::Shutdown => shutdown_pc(),
    }
}

/// Shows a desktop notification.
///
/// On Windows the notification plugin falls back to PowerShell's identity when
/// the exe runs from `target/`, so the toast says "Windows PowerShell". Use our
/// own app id instead, registered by `register_app_id`.
fn notify(app: &AppHandle, title: &str, body: &str) {
    #[cfg(windows)]
    {
        let id = app.config().identifier.clone();
        let _ = tauri_winrt_notification::Toast::new(&id).title(title).text1(body).show();
    }
    #[cfg(not(windows))]
    {
        let _ = app.notification().builder().title(title).body(body).show();
    }
}

/// Registers the app id with a display name so Windows labels toasts "Bright Downloader".
#[cfg(windows)]
fn register_app_id(app: &AppHandle) {
    let key = format!(r"Software\Classes\AppUserModelId\{}", app.config().identifier);
    if let Ok((key, _)) = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey(key) {
        let name = app.config().product_name.clone().unwrap_or_else(|| "Bright Downloader".into());
        let _ = key.set_value("DisplayName", &name);
    }
}

#[tauri::command]
fn list_downloads(mgr: Mgr) -> Vec<Item> {
    mgr.list()
}

#[tauri::command]
async fn probe_url(
    mgr: Mgr<'_>,
    history: Hist<'_>,
    url: String,
    headers: Option<Vec<(String, String)>>,
) -> Result<ProbeResult, String> {
    let result = mgr.probe(url.clone(), headers.unwrap_or_default()).await;
    // Failed checks are remembered too: the link may work later.
    let (file_name, size) = match &result {
        Ok(p) => (Some(p.file_name.clone()), p.size),
        Err(_) => (None, None),
    };
    let entry = history::Entry { url: url.trim().to_string(), file_name, size, checked_at: manager::now() };
    history.record(entry, mgr.settings().history_retention);
    result
}

#[tauri::command]
fn list_history(mgr: Mgr, history: Hist) -> Vec<history::Entry> {
    history.list(mgr.settings().history_retention, manager::now())
}

#[tauri::command]
fn remove_history(history: Hist, url: String) {
    history.remove(&url);
}

#[tauri::command]
fn clear_history(history: Hist) {
    history.clear();
}

#[tauri::command]
async fn add_download(mgr: Mgr<'_>, request: AddRequest) -> Result<Item, String> {
    mgr.inner().clone().add(request).await
}

#[tauri::command]
fn pause_download(mgr: Mgr, id: u64) {
    mgr.pause(id);
}

#[tauri::command]
fn resume_download(mgr: Mgr, id: u64) {
    mgr.inner().resume(id);
}

#[tauri::command]
fn remove_download(mgr: Mgr, id: u64, delete_file: bool) {
    mgr.inner().remove(id, delete_file);
}

#[tauri::command]
fn set_queue(mgr: Mgr, ids: Vec<u64>, queue: String) -> Result<(), String> {
    mgr.inner().set_queue(&ids, &queue)
}

#[tauri::command]
fn set_priority(mgr: Mgr, ids: Vec<u64>, priority: Priority) {
    mgr.inner().set_priority(&ids, priority);
}

#[tauri::command]
fn set_speed_limit(mgr: Mgr, id: u64, kib: u64) {
    mgr.inner().set_speed_limit(id, kib);
}

#[tauri::command]
fn start_queue(mgr: Mgr, queue: String) {
    mgr.inner().start_queue(&queue);
}

#[tauri::command]
fn stop_queue(mgr: Mgr, queue: String) {
    mgr.stop_queue(&queue);
}

#[tauri::command]
async fn compute_checksum(mgr: Mgr<'_>, id: u64, algo: Algo) -> Result<String, String> {
    mgr.checksum(id, algo).await
}

#[tauri::command]
fn free_space(dir: PathBuf) -> Option<u64> {
    dm_engine::disk::available_space(&dir)
}

#[tauri::command]
async fn grab_links(mgr: Mgr<'_>, url: String, depth: u32, exts: Vec<String>) -> Result<grabber::Grabbed, String> {
    grabber::grab(&mgr.client(), &url, depth.min(3), &exts).await
}

/// Links in a text file (one per line; `#` lines are comments).
#[tauri::command]
fn read_link_file(path: PathBuf) -> Result<Vec<String>, String> {
    const MAX: u64 = 16 << 20;
    let len = std::fs::metadata(&path).map_err(|e| e.to_string())?.len();
    if len > MAX {
        return Err("the file is too large for a link list".into());
    }
    let bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    Ok(links_in_text(&String::from_utf8_lossy(&bytes)))
}

/// Save the addresses of the given downloads (all if empty), one per line.
#[tauri::command]
fn export_links(mgr: Mgr, path: PathBuf, ids: Vec<u64>) -> Result<usize, String> {
    let urls: Vec<String> =
        mgr.list().into_iter().filter(|i| ids.is_empty() || ids.contains(&i.id)).map(|i| i.url).collect();
    let mut text = urls.join("\r\n");
    text.push_str("\r\n");
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(urls.len())
}

#[tauri::command]
fn tools_status(mgr: Mgr) -> tools::ToolsStatus {
    tools::status(mgr.data_dir())
}

#[derive(Clone, Serialize)]
struct ToolProgress<'a> {
    tool: tools::Tool,
    #[serde(flatten)]
    progress: &'a Progress,
}

/// Download and install ffmpeg or yt-dlp; progress goes out as `tool-progress`.
#[tauri::command]
async fn install_tool(app: AppHandle, mgr: Mgr<'_>, tool: tools::Tool) -> Result<String, String> {
    let engine = mgr.engine();
    let path = tools::install(&engine, mgr.data_dir(), tool, |p| {
        let _ = app.emit("tool-progress", ToolProgress { tool, progress: p });
    })
    .await?;
    Ok(path.display().to_string())
}

#[tauri::command]
fn set_sync(mgr: Mgr, ids: Vec<u64>, hours: u32) {
    mgr.inner().set_sync(&ids, hours);
}

/// Open the download's page in the browser; the next matching download the
/// extension captures becomes this download's new address.
#[tauri::command]
fn refresh_address(app: AppHandle, mgr: Mgr, id: u64) -> Result<(), String> {
    let page = mgr.request_refresh(id)?;
    app.opener().open_url(page, None::<&str>).map_err(|e| e.to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: &'static str,
    repo: &'static str,
    site: &'static str,
    portable: bool,
}

#[tauri::command]
fn app_info() -> AppInfo {
    AppInfo { version: updater::current_version(), repo: updater::REPO_URL, site: updater::SITE_URL, portable: portable_data_dir().is_some() }
}

/// The portable copy ships a `portable.txt` next to the program; it then keeps
/// its settings, downloads list and tools in a `data` folder beside it.
fn portable_data_dir() -> Option<PathBuf> {
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    dir.join("portable.txt").is_file().then(|| dir.join("data"))
}

/// Open one of the project's pages; only these fixed addresses are allowed.
#[tauri::command]
fn open_help(app: AppHandle, page: String) -> Result<(), String> {
    let url = match page.as_str() {
        "repo" => updater::REPO_URL.to_string(),
        "site" => updater::SITE_URL.to_string(),
        "issues" => format!("{}/issues", updater::REPO_URL),
        "releases" => format!("{}/releases", updater::REPO_URL),
        _ => return Err("unknown page".into()),
    };
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
async fn check_update(mgr: Mgr<'_>) -> Result<Option<updater::UpdateInfo>, String> {
    updater::check(&mgr.engine()).await
}

#[tauri::command]
fn skip_update(mgr: Mgr, version: String) {
    let mut settings = mgr.settings();
    settings.skipped_version = version;
    mgr.inner().set_settings(settings);
}

/// Download and verify the new version (progress goes out as
/// `update-progress`), then quit and let the installer start it again.
/// The update is looked up again here so only what GitHub lists is installed.
#[tauri::command]
async fn install_update(app: AppHandle, mgr: Mgr<'_>, version: String) -> Result<(), String> {
    if portable_data_dir().is_some() {
        return Err("the portable version is updated by downloading the new portable zip".into());
    }
    let engine = mgr.engine();
    let info = updater::check(&engine).await?.filter(|i| i.version == version).ok_or("this update is no longer available")?;
    let installer = updater::download(&engine, mgr.data_dir(), &info, |p| {
        let _ = app.emit("update-progress", p);
    })
    .await?;
    let running: Vec<u64> =
        mgr.list().into_iter().filter(|i| matches!(i.status, Status::Downloading | Status::Queued)).map(|i| i.id).collect();
    mgr.shutdown(std::time::Duration::from_secs(3)).await;
    updater::run_installer(mgr.data_dir(), &installer, &info, running)?;
    app.exit(0);
    Ok(())
}

/// Release notes of the update that installed this version, shown once.
/// Downloads that were running before the update start again.
#[tauri::command]
fn take_update_notes(mgr: Mgr) -> Option<updater::AfterUpdate> {
    let after = updater::take_after_update(mgr.data_dir())?;
    for &id in &after.resume {
        mgr.inner().resume(id);
    }
    Some(after)
}

/// Shortly after startup and then twice a day: bring the window forward
/// and offer a new version (each one once per run, never a skipped one).
fn watch_for_updates(app: AppHandle, mgr: Arc<Manager>) {
    tauri::async_runtime::spawn(async move {
        let mut offered = String::new();
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        loop {
            let settings = mgr.settings();
            if settings.check_updates {
                if let Ok(Some(info)) = updater::check(&mgr.engine()).await {
                    if info.version != settings.skipped_version && info.version != offered {
                        offered = info.version.clone();
                        show_main(&app);
                        let _ = app.emit("update-available", info);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(12 * 3600)).await;
        }
    });
}

/// Async: on Windows a window created inside a synchronous command never
/// finishes loading (the command runs on the thread the window needs).
#[tauri::command]
async fn open_progress_window(app: AppHandle, mgr: Mgr<'_>, id: u64) -> Result<(), String> {
    let item = mgr.find(id).ok_or("download not found")?;
    open_progress(&app, id, &item.file_name);
    Ok(())
}

#[tauri::command]
fn show_main_window(app: AppHandle) {
    show_main(&app);
}

/// Links handed over by the browser or the command line, waiting for the
/// window to show the Add dialog.
#[derive(Default)]
struct Pending(Mutex<Vec<CapturedEvent>>);

#[tauri::command]
fn take_pending(pending: State<Pending>) -> Vec<CapturedEvent> {
    std::mem::take(&mut *pending.0.lock().unwrap())
}

/// Queue a capture for the Add dialog and wake the window.
fn hand_to_window(app: &AppHandle, capture: CapturedEvent) {
    if let Some(p) = app.try_state::<Pending>() {
        p.0.lock().unwrap().push(capture);
    }
    show_main(app);
    let _ = app.emit("captured", ());
}

/// A capture from the browser: first a fresh address for a download waiting
/// for one (see `refresh_address`), else the Add dialog.
fn on_capture(app: AppHandle, capture: bridge::CapturedDownload) {
    tauri::async_runtime::spawn(async move {
        let headers = capture.headers();
        if let Some(mgr) = app.try_state::<Arc<Manager>>() {
            let mgr = mgr.inner().clone();
            if let Some(item) = mgr.try_refresh(&capture.url, &headers, capture.file_name.as_deref()).await {
                notify(&app, &i18n::t(lang(&app), "refreshed", ""), &item.file_name);
                return;
            }
        }
        hand_to_window(&app, CapturedEvent { headers, capture });
    });
}

/// Act on a command line (this launch's or a second launch's).
fn run_cli(app: &AppHandle, args: impl IntoIterator<Item = String>) {
    let Some(cmd) = cli::parse(args) else { return };
    if !cmd.silent {
        let capture = bridge::CapturedDownload {
            url: cmd.url,
            file_name: cmd.name,
            referrer: None,
            cookies: None,
            user_agent: None,
            variant: None,
        };
        hand_to_window(app, CapturedEvent { headers: Vec::new(), capture });
        return;
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let Some(mgr) = app.try_state::<Arc<Manager>>().map(|m| m.inner().clone()) else { return };
        let request = serde_json::json!({
            "url": cmd.url,
            "fileName": cmd.name,
            "saveDir": cmd.dir,
            "start": !cmd.later,
            "queue": cmd.queue,
        });
        let lang = lang(&app);
        match serde_json::from_value::<AddRequest>(request) {
            Ok(req) => match mgr.add(req).await {
                Ok(item) => notify(&app, &i18n::t(lang, "added", ""), &item.file_name),
                Err(e) => notify(&app, &i18n::t(lang, "failed", ""), &e),
            },
            Err(e) => tracing::error!(error = %e, "bad command line"),
        }
    });
}

#[tauri::command]
fn default_scan_command() -> &'static str {
    post::DEFENDER_COMMAND
}

#[tauri::command]
fn set_download_url(mgr: Mgr, id: u64, url: String) -> Result<Item, String> {
    mgr.set_url(id, url)
}

/// The clipboard text if it is a single http(s) link, for pre-filling the
/// Add dialog. Anything else (prose, file paths, other schemes) is ignored.
#[tauri::command]
fn clipboard_url(app: AppHandle) -> Option<String> {
    app.clipboard().read_text().ok().and_then(|text| link_from_text(&text))
}

/// Every distinct http(s) link in `text`, in order. Lines starting with `#`
/// are comments.
fn links_in_text(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with('#'))
        .flat_map(str::split_whitespace)
        .filter_map(link_from_text)
        .filter(|l| seen.insert(l.clone()))
        .collect()
}

/// A copied link worth offering: it names a file of a known type.
fn looks_like_file(url: &str) -> bool {
    dm_engine::probe::filename_from_url(url).is_some_and(|n| category::category_for(&n) != category::GENERAL)
}

/// Offer links copied anywhere in Windows. Polls the clipboard's change
/// counter, so the clipboard itself is only opened after a copy.
#[cfg(windows)]
fn watch_clipboard(app: AppHandle) {
    use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
    tauri::async_runtime::spawn(async move {
        // SAFETY: no arguments, reads a counter.
        let mut last = unsafe { GetClipboardSequenceNumber() };
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(700)).await;
            let seq = unsafe { GetClipboardSequenceNumber() };
            if seq == last {
                continue;
            }
            last = seq;
            let Some(mgr) = app.try_state::<Arc<Manager>>() else { continue };
            if !mgr.settings().clipboard_monitor {
                continue;
            }
            let Some(url) = app.clipboard().read_text().ok().and_then(|t| link_from_text(&t)) else { continue };
            // Links already in the list include ones copied with "Copy address".
            if !looks_like_file(&url) || mgr.list().iter().any(|i| i.url == url) {
                continue;
            }
            let visible = app.get_webview_window("main").and_then(|w| w.is_visible().ok()).unwrap_or(false);
            if !visible {
                let name = dm_engine::probe::filename_from_url(&url).unwrap_or_default();
                let lang = lang(&app);
                notify(&app, &i18n::t(lang, "copied", ""), &i18n::t(lang, "copied_body", &name));
            }
            let _ = app.emit("clipboard-link", url);
        }
    });
}

#[cfg(windows)]
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

fn autostart_name(app: &AppHandle) -> String {
    app.config().product_name.clone().unwrap_or_else(|| "Bright Downloader".into())
}

#[cfg(windows)]
fn autostart_enabled(app: &AppHandle) -> bool {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    hkcu.open_subkey(RUN_KEY).and_then(|k| k.get_value::<String, _>(autostart_name(app))).is_ok()
}

/// Register (or remove) a Run entry that starts Bright Downloader hidden in the tray.
#[cfg(windows)]
fn set_autostart(app: &AppHandle, on: bool) -> Result<(), String> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(RUN_KEY).map_err(|e| e.to_string())?;
    if on {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        key.set_value(autostart_name(app), &format!("\"{}\" {MINIMIZED_ARG}", exe.display())).map_err(|e| e.to_string())
    } else {
        match key.delete_value(autostart_name(app)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
            _ => Ok(()),
        }
    }
}

/// The app used to be called OpenBolt: move its data folder over once and
/// swap its Windows startup entry for ours.
fn migrate_from_openbolt(app: &AppHandle, data_dir: &Path) {
    if let Some(old) = data_dir.parent().map(|p| p.join("org.openbolt.app")) {
        if old.is_dir() && !data_dir.exists() {
            let _ = std::fs::rename(&old, data_dir);
        }
    }
    #[cfg(windows)]
    {
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        let had_old = hkcu
            .open_subkey_with_flags(RUN_KEY, winreg::enums::KEY_ALL_ACCESS)
            .and_then(|k| k.delete_value("OpenBolt"))
            .is_ok();
        if had_old {
            let _ = set_autostart(app, true);
        }
    }
    #[cfg(not(windows))]
    let _ = app;
}

#[cfg(not(windows))]
fn autostart_enabled(_app: &AppHandle) -> bool {
    false
}

#[cfg(not(windows))]
fn set_autostart(_app: &AppHandle, _on: bool) -> Result<(), String> {
    Err("not supported on this system".into())
}

/// Passed by the Windows startup entry: start in the tray without a window.
const MINIMIZED_ARG: &str = "--minimized";

fn link_from_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let url = url::Url::parse(text).ok()?;
    (matches!(url.scheme(), "http" | "https" | "ftp") && url.host().is_some()).then(|| text.to_string())
}

#[tauri::command]
fn get_settings(app: AppHandle, mgr: Mgr) -> Settings {
    Settings { start_with_windows: autostart_enabled(&app), ..mgr.settings() }
}

#[tauri::command]
fn set_settings(app: AppHandle, mgr: Mgr, history: Hist, settings: Settings) -> Result<(), String> {
    // Reject an unusable proxy before anything is saved.
    dm_engine::build_client(&settings.engine_options(&Options::default())).map_err(|e| e.to_string())?;
    let retention = settings.history_retention;
    let autostart = settings.start_with_windows;
    mgr.inner().set_settings(settings);
    translate_tray(&app);
    // Apply a shorter retention (or "Off") right away.
    history.list(retention, manager::now());
    if autostart != autostart_enabled(&app) {
        set_autostart(&app, autostart).map_err(|e| format!("could not change the Windows startup setting: {e}"))?;
    }
    Ok(())
}

#[tauri::command]
fn open_file(app: tauri::AppHandle, mgr: Mgr, id: u64) -> Result<(), String> {
    let item = mgr.find(id).ok_or("download not found")?;
    app.opener().open_path(item.save_path.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

#[tauri::command]
fn show_in_folder(app: tauri::AppHandle, mgr: Mgr, id: u64) -> Result<(), String> {
    let item = mgr.find(id).ok_or("download not found")?;
    let part = dm_engine::part_path(&item.save_path);
    let target = if item.save_path.exists() { &item.save_path } else { &part };
    if target.exists() {
        app.opener().reveal_item_in_dir(target).map_err(|e| e.to_string())
    } else {
        let dir = item.save_path.parent().ok_or("no folder")?;
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        app.opener().open_path(dir.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
    }
}

fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Save progress of running downloads, then exit.
fn quit(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Some(mgr) = app.try_state::<Arc<Manager>>() {
            mgr.shutdown(std::time::Duration::from_secs(3)).await;
            if mgr.settings().history_retention == history::Retention::OnExit {
                if let Some(history) = app.try_state::<Arc<history::History>>() {
                    history.clear();
                }
            }
        }
        app.exit(0);
    });
}

/// Bundled icon, so the taskbar and tray never fall back to a generic one.
fn app_icon() -> tauri::image::Image<'static> {
    tauri::include_image!("icons/128x128.png")
}

/// The app keeps running in the tray when the window is closed, so browser
/// capture and the scheduler keep working.
struct TrayItems {
    open: MenuItem<tauri::Wry>,
    quit: MenuItem<tauri::Wry>,
}

/// Re-label the tray menu after a language change.
fn translate_tray(app: &AppHandle) {
    if let Some(items) = app.try_state::<TrayItems>() {
        let lang = lang(app);
        let _ = items.open.set_text(i18n::t(lang, "open", ""));
        let _ = items.quit.set_text(i18n::t(lang, "quit", ""));
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let language = lang(app.handle());
    let open = MenuItem::with_id(app, "open", i18n::t(language, "open", ""), true, None::<&str>)?;
    let exit = MenuItem::with_id(app, "quit", i18n::t(language, "quit", ""), true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&open, &exit])?;
    app.manage(TrayItems { open: open.clone(), quit: exit.clone() });
    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("Bright Downloader")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main(app),
            "quit" => {
                use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};
                let app = app.clone();
                let lang = lang(&app);
                app.dialog()
                    .message(i18n::t(lang, "quit_ask", ""))
                    .title(i18n::t(lang, "quit_title", ""))
                    .kind(MessageDialogKind::Warning)
                    .buttons(MessageDialogButtons::YesNo)
                    .show({
                        let app = app.clone();
                        move |yes| if yes { quit(&app) }
                    });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                show_main(tray.app_handle());
            }
        });
    tray = tray.icon(app_icon());
    tray.build(app)?;
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Must be first: a second launch just brings the running window forward.
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            show_main(app);
            run_cli(app, args.into_iter().skip(1));
        }))
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            #[cfg(windows)]
            register_app_id(app.handle());
            let data_dir = match portable_data_dir() {
                Some(dir) => dir,
                None => {
                    let dir = app.path().app_data_dir()?;
                    migrate_from_openbolt(app.handle(), &dir);
                    dir
                }
            };
            dm_engine::hls::set_tools_dir(tools::dir(&data_dir));
            app.manage(Pending::default());
            let events = Arc::new(WindowEvents { app: app.handle().clone(), windows_opened: Mutex::new(HashSet::new()) });
            let manager = Manager::load(events, data_dir.clone(), Options::default())?;
            app.manage(manager.clone());
            watch_for_updates(app.handle().clone(), manager.clone());
            let retention = manager.settings().history_retention;
            app.manage(Arc::new(history::History::load(data_dir.join("history.json"), retention, manager::now())));

            tauri::async_runtime::spawn(async move {
                use chrono::{Datelike, Timelike};
                let mut ticks = 0u64;
                loop {
                    let now = chrono::Local::now();
                    let weekday = now.weekday().num_days_from_monday() as u8;
                    manager.scheduler_tick(now.timestamp() / 60, weekday, now.hour() * 60 + now.minute());
                    manager.quota_tick(manager::now());
                    if ticks % 4 == 0 {
                        manager.sync_tick(manager::now()).await;
                    }
                    ticks += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
                }
            });

            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_icon(app_icon());
                // The window starts hidden (see tauri.conf.json) so a launch at
                // Windows startup does not flash it.
                if !std::env::args().any(|a| a == MINIMIZED_ARG) {
                    let _ = window.show();
                }
            }
            build_tray(app)?;
            #[cfg(windows)]
            watch_clipboard(app.handle().clone());

            run_cli(app.handle(), std::env::args().skip(1));
            let handle = app.handle().clone();
            let capture: bridge::OnCapture = Arc::new(move |c| on_capture(handle.clone(), c));
            tauri::async_runtime::spawn(async move {
                if let Err(e) = bridge::serve(bridge::PORT, capture).await {
                    tracing::error!(error = %e, "browser bridge unavailable");
                }
            });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // X hides the main window to the tray; minimize (-) still goes
                // to the taskbar. Progress windows really close.
                if window.label() == "main" {
                    let _ = window.hide();
                    api.prevent_close();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            app_info,
            open_help,
            check_update,
            skip_update,
            install_update,
            take_update_notes,
            list_downloads,
            probe_url,
            add_download,
            pause_download,
            resume_download,
            remove_download,
            set_download_url,
            set_queue,
            set_priority,
            set_speed_limit,
            start_queue,
            stop_queue,
            compute_checksum,
            free_space,
            grab_links,
            read_link_file,
            export_links,
            default_scan_command,
            run_done_action,
            tools_status,
            install_tool,
            set_sync,
            refresh_address,
            open_progress_window,
            show_main_window,
            take_pending,
            clipboard_url,
            list_history,
            remove_history,
            clear_history,
            get_settings,
            set_settings,
            open_file,
            show_in_folder,
        ])
        .run(tauri::generate_context!())
        .expect("error while running Bright Downloader");
}

#[cfg(test)]
mod tests {
    use super::{link_from_text, links_in_text, looks_like_file};

    #[test]
    fn only_single_web_links_are_pasted() {
        assert_eq!(link_from_text("  https://example.com/a.zip
").as_deref(), Some("https://example.com/a.zip"));
        assert_eq!(link_from_text("http://x.org/file?id=1&t=2").as_deref(), Some("http://x.org/file?id=1&t=2"));
        assert_eq!(link_from_text("see https://example.com/a.zip for details"), None);
        assert_eq!(link_from_text("ftp://example.com/a.zip").as_deref(), Some("ftp://example.com/a.zip"));
        assert_eq!(link_from_text("gopher://example.com/a.zip"), None);
        assert_eq!(link_from_text(r"C:\Users\me\file.zip"), None);
        assert_eq!(link_from_text("javascript:alert(1)"), None);
        assert_eq!(link_from_text(""), None);
    }

    #[test]
    fn link_lists_skip_comments_and_duplicates() {
        let text = "# Bright Downloader test links\r\nhttps://a.example/1.zip\n\n  https://b.example/2.iso  see also https://a.example/1.zip\n# https://c.example/no.zip\ngopher://x/y";
        assert_eq!(links_in_text(text), ["https://a.example/1.zip", "https://b.example/2.iso"]);
    }

    #[test]
    fn clipboard_offers_only_file_links() {
        assert!(looks_like_file("https://x.example/setup.exe?token=1"));
        assert!(looks_like_file("https://x.example/a/Movie.MKV"));
        assert!(!looks_like_file("https://x.example/article"));
        assert!(!looks_like_file("https://x.example/page.html"));
    }
}
