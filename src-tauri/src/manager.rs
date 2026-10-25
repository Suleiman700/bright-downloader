//! Download list, queues and persistence. Owns every running engine task and
//! reports changes to the UI through `item` / `progress` events.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dm_engine::checksum::{self, Algo};
use dm_engine::hls::{self, StreamInfo};
use dm_engine::{probe, temp_paths, Engine, Job, Options, Outcome, Progress, RateLimiter, RequestSpec};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::category::category_for;
use crate::history::Retention;
use crate::post;
use crate::site::{self, SiteInfo};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Completed,
    Error,
}

/// Order in which queued downloads start; ties go to the oldest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    #[default]
    Normal,
    High,
}

/// What happens once this download (and its virus scan) has finished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OpenWhenDone {
    #[default]
    Nothing,
    File,
    Folder,
    /// Shut the computer down (after a countdown that can be cancelled).
    Shutdown,
    Sleep,
    /// Run `Item::done_command` in Command Prompt.
    Command,
}

pub const MAIN_QUEUE: &str = "Main";

fn main_queue() -> String {
    MAIN_QUEUE.into()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub id: u64,
    pub url: String,
    pub file_name: String,
    pub save_path: PathBuf,
    pub category: String,
    pub size: Option<u64>,
    pub downloaded: u64,
    pub status: Status,
    pub error: Option<String>,
    pub resumable: bool,
    pub added_at: u64,
    /// When it first started downloading (for the `%t` command placeholder).
    #[serde(default)]
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Added with "Download Later": started and stopped by the scheduler.
    #[serde(default)]
    pub scheduled: bool,
    #[serde(default = "main_queue")]
    pub queue: String,
    #[serde(default)]
    pub priority: Priority,
    /// This download's own limit in KiB/s, 0 = only the global limit applies.
    #[serde(default)]
    pub speed_limit_kib: u64,
    /// Expected MD5 or SHA-256 hex digest, checked when the download finishes.
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub open_when_done: OpenWhenDone,
    /// Command line for [`OpenWhenDone::Command`]; `%f` is the downloaded file.
    #[serde(default)]
    pub done_command: Option<String>,
    /// Progress or outcome of the after-download steps (checksum, scan, extraction).
    #[serde(default)]
    pub note: Option<String>,
    /// Automatic restarts after network failures since the last success or manual resume.
    #[serde(default)]
    pub auto_retries: u32,
    /// An HLS (`.m3u8`) stream rather than a plain file.
    #[serde(default)]
    pub stream: bool,
    /// A live stream: recorded until stopped, then saved.
    #[serde(default)]
    pub live: bool,
    /// Chosen quality (variant playlist URL) of a stream; `None` = best.
    #[serde(default)]
    pub variant: Option<String>,
    /// A video-site page downloaded with yt-dlp; the yt-dlp format selector.
    #[serde(default)]
    pub site_format: Option<String>,
    /// Re-check the server every this many hours and download again if the
    /// file changed; 0 = off.
    #[serde(default)]
    pub sync_hours: u32,
    #[serde(default)]
    pub last_checked: Option<u64>,
    /// ETag / Last-Modified / size seen at the last check.
    #[serde(default)]
    pub remote_version: Option<String>,
    /// Waiting for the browser to hand over a fresh address for this file.
    #[serde(default)]
    pub awaiting_refresh: bool,
}

/// Daily window in which scheduled downloads run (local time).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct Schedule {
    pub enabled: bool,
    /// "HH:MM"
    pub start: String,
    /// "HH:MM", or empty to let downloads run until they finish.
    pub stop: String,
    /// 0 = Monday … 6 = Sunday.
    pub days: Vec<u8>,
}

impl Default for Schedule {
    fn default() -> Self {
        Self { enabled: false, start: "02:00".into(), stop: String::new(), days: (0..7).collect() }
    }
}

/// "HH:MM" → minutes after midnight.
pub fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Queue {
    pub name: String,
    /// Downloads from this queue at the same time; 0 = only the overall limit.
    pub max_concurrent: usize,
}

/// Connection count for one server (and its subdomains).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HostLimit {
    pub host: String,
    pub connections: usize,
}

/// Saved user name and password for a server (HTTP Basic auth and FTP).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiteLogin {
    pub host: String,
    pub username: String,
    /// Clear text in memory; encrypted in `settings.json` and in what the
    /// window receives (see [`crate::secret`]).
    #[serde(default, with = "crate::secret")]
    pub password: String,
    /// A password typed in the Options window; replaces `password`.
    #[serde(default, skip_serializing)]
    pub new_password: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// Windows' proxy settings (and `HTTP(S)_PROXY`).
    #[default]
    System,
    None,
    Custom,
}

/// Does `rule` (a host, `*.host`, or empty) cover `host`, including subdomains?
fn host_matches(rule: &str, host: &str) -> bool {
    let rule = rule.trim().trim_start_matches("*.").to_ascii_lowercase();
    !rule.is_empty() && (host == rule || host.ends_with(&format!(".{rule}")))
}

fn host_of(url: &str) -> Option<String> {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase))
}

/// What to do once every download has finished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DoneAction {
    #[default]
    Nothing,
    Exit,
    Sleep,
    Hibernate,
    Shutdown,
    Run,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub connections: usize,
    pub max_concurrent: usize,
    /// Global limit in KiB/s, 0 = unlimited.
    pub speed_limit_kib: u64,
    /// Save into `<download_dir>/<Category>/` like classic download managers.
    pub sort_by_category: bool,
    pub schedule: Schedule,
    /// How long checked links stay in the Add dialog's history.
    pub history_retention: Retention,
    /// Always contains [`MAIN_QUEUE`].
    pub queues: Vec<Queue>,
    /// Per-server connection counts that override `connections`.
    pub host_connections: Vec<HostLimit>,
    /// How often a download that failed with a network error restarts on its own.
    pub auto_retry: u32,
    /// Virus scan run on each finished file (`%f` = the file); empty = off.
    pub scan_command: String,
    pub extract_archives: bool,
    pub when_done: DoneAction,
    /// Command line for [`DoneAction::Run`].
    pub when_done_program: String,
    /// Offer to download file links copied to the clipboard.
    pub clipboard_monitor: bool,
    /// Launch to the tray when Windows starts. The registry is the source of
    /// truth; this mirrors it for the UI.
    pub start_with_windows: bool,
    pub proxy_mode: ProxyMode,
    /// `http://host:port` or `socks5://host:port`, optionally with `user:pass@`.
    pub proxy_url: String,
    pub logins: Vec<SiteLogin>,
    /// Download at most this many MB per `quota_hours`; 0 = no limit.
    pub quota_mb: u64,
    pub quota_hours: u32,
    /// Open a small progress window for each download that starts.
    pub progress_windows: bool,
    /// "auto" or one of [`crate::i18n::LANGUAGES`].
    pub language: String,
    /// Command run whenever a download starts or finishes; empty = none.
    /// `%f` = file, `%n` = file name, `%u` = address.
    pub start_command: String,
    pub finish_command: String,
    /// What the Add dialog's "When done" starts with.
    pub default_when_done: OpenWhenDone,
    pub default_done_command: String,
    /// Look for a new version on GitHub at startup and twice a day.
    pub check_updates: bool,
    /// A version the user chose to skip; it is not offered again.
    pub skipped_version: String,
}

impl Default for Settings {
    fn default() -> Self {
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(PathBuf::from);
        Self {
            download_dir: home.map(|h| h.join("Downloads").join("Bright Downloader")).unwrap_or_else(|| "Downloads".into()),
            connections: 8,
            max_concurrent: 3,
            speed_limit_kib: 0,
            sort_by_category: true,
            schedule: Schedule::default(),
            history_retention: Retention::default(),
            queues: vec![Queue { name: main_queue(), max_concurrent: 0 }],
            host_connections: Vec::new(),
            auto_retry: 3,
            scan_command: String::new(),
            extract_archives: false,
            when_done: DoneAction::Nothing,
            when_done_program: String::new(),
            clipboard_monitor: true,
            start_with_windows: false,
            proxy_mode: ProxyMode::System,
            proxy_url: String::new(),
            logins: Vec::new(),
            quota_mb: 0,
            quota_hours: 1,
            progress_windows: false,
            language: "auto".into(),
            start_command: String::new(),
            finish_command: String::new(),
            default_when_done: OpenWhenDone::Nothing,
            default_done_command: String::new(),
            check_updates: true,
            skipped_version: String::new(),
        }
    }
}

impl Settings {
    /// Connections for `url`: the first matching host rule, else the default.
    pub fn connections_for(&self, url: &str) -> usize {
        let rule = host_of(url).and_then(|host| self.host_connections.iter().find(|r| host_matches(&r.host, &host)));
        rule.map_or(self.connections, |r| r.connections).clamp(1, 32)
    }

    pub fn login_for(&self, url: &str) -> Option<&SiteLogin> {
        let host = host_of(url)?;
        self.logins.iter().find(|l| host_matches(&l.host, &host))
    }

    /// The engine options for these settings (proxy) on top of `base`.
    pub fn engine_options(&self, base: &Options) -> Options {
        Options {
            proxy: (self.proxy_mode == ProxyMode::Custom).then(|| self.proxy_url.trim().to_string()).filter(|p| !p.is_empty()),
            no_proxy: self.proxy_mode == ProxyMode::None,
            ..base.clone()
        }
    }

    /// Add the saved login for `url`'s server: user and password in an FTP
    /// address, an `Authorization` header for HTTP. Existing credentials win.
    pub fn authorize(&self, url: &str, headers: &[(String, String)]) -> (String, Vec<(String, String)>) {
        let mut headers = headers.to_vec();
        let Some(login) = self.login_for(url) else { return (url.to_string(), headers) };
        if dm_engine::is_ftp(url) {
            if let Ok(mut u) = url::Url::parse(url) {
                if u.username().is_empty() && u.set_username(&login.username).is_ok() && u.set_password(Some(&login.password)).is_ok() {
                    return (u.to_string(), headers);
                }
            }
        } else if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("authorization")) {
            use base64::Engine as _;
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", login.username, login.password));
            headers.push(("Authorization".into(), format!("Basic {token}")));
        }
        (url.to_string(), headers)
    }

    fn queue_limit(&self, name: &str) -> usize {
        self.queues.iter().find(|q| q.name == name).map_or(0, |q| q.max_concurrent)
    }

    fn has_queue(&self, name: &str) -> bool {
        self.queues.iter().any(|q| q.name == name)
    }

    /// Main always exists; names are trimmed, non-empty and unique.
    fn normalize(&mut self) {
        if !self.has_queue(MAIN_QUEUE) {
            self.queues.insert(0, Queue { name: main_queue(), max_concurrent: 0 });
        }
        let mut seen = HashSet::new();
        self.queues.retain_mut(|q| {
            q.name = q.name.trim().to_string();
            !q.name.is_empty() && seen.insert(q.name.to_lowercase())
        });
        self.host_connections.retain(|r| !r.host.trim().is_empty());
        for login in &mut self.logins {
            login.host = login.host.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_string();
            if let Some(p) = login.new_password.take().filter(|p| !p.is_empty()) {
                login.password = p;
            }
        }
        self.logins.retain(|l| !l.host.is_empty());
        self.quota_hours = self.quota_hours.max(1);
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeResult {
    pub final_url: String,
    pub file_name: String,
    pub size: Option<u64>,
    pub resumable: bool,
    pub content_type: Option<String>,
    pub category: String,
    pub save_dir: PathBuf,
    /// A download already in the list with the same address.
    pub duplicate: Option<Duplicate>,
    /// Free space where the file would be saved.
    pub free_space: Option<u64>,
    /// Set for HLS streams: qualities, live or not, ffmpeg availability.
    pub stream: Option<StreamInfo>,
    /// Set for video-site pages handled by yt-dlp.
    pub site: Option<SiteInfo>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Duplicate {
    pub id: u64,
    pub file_name: String,
    pub status: Status,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddRequest {
    pub url: String,
    pub file_name: Option<String>,
    pub save_dir: Option<PathBuf>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default = "yes")]
    pub start: bool,
    #[serde(default)]
    pub scheduled: bool,
    /// Size / resume support from an earlier `probe` (the Add dialog), so
    /// the list shows them before the download starts.
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub resumable: Option<bool>,
    /// Defaults to Main; unknown names fall back to Main too.
    #[serde(default)]
    pub queue: Option<String>,
    #[serde(default)]
    pub priority: Priority,
    #[serde(default)]
    pub checksum: Option<String>,
    #[serde(default)]
    pub open_when_done: OpenWhenDone,
    #[serde(default)]
    pub done_command: Option<String>,
    #[serde(default)]
    pub speed_limit_kib: u64,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub site_format: Option<String>,
}

fn yes() -> bool {
    true
}

/// Where the manager reports changes (the Tauri window in the app, a
/// recorder in tests).
pub trait EventSink: Send + Sync + 'static {
    fn item(&self, item: &Item);
    fn progress(&self, id: u64, progress: &Progress);
    fn removed(&self, id: u64);
    /// A download completed or failed for good. Not called for failures
    /// that will be retried automatically.
    fn finished(&self, _item: &Item) {}
    /// The after-download steps passed and the user asked to see the file.
    fn open(&self, _item: &Item) {}
    /// The last download completed and nothing else is queued or running.
    fn idle(&self) {}
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    next_id: u64,
    items: Vec<Item>,
}

struct Running {
    token: CancellationToken,
    /// This download's own limiter, so its limit can change while it runs.
    limiter: Arc<RateLimiter>,
}

struct Inner {
    store: Store,
    settings: Settings,
    running: HashMap<u64, Running>,
    /// Minute the scheduler last acted on, so each trigger fires once.
    last_schedule_minute: Option<i64>,
    /// Completed downloads still verifying, scanning or extracting.
    finishing: usize,
    /// Failed downloads waiting for an automatic restart.
    retry_pending: HashSet<u64>,
    /// A download completed since the list was last idle.
    completed_since_idle: bool,
    /// Start (Unix seconds) and bytes of the current download-limit window.
    quota_start: u64,
    quota_bytes: u64,
    /// Limit reached: nothing runs until this time.
    quota_until: Option<u64>,
    /// Downloads stopped by the limit, restarted when the window ends.
    quota_paused: HashSet<u64>,
}

impl Inner {
    /// The highest-priority queued download whose queue has a free slot.
    fn next_to_start(&self) -> Option<usize> {
        let mut per_queue: HashMap<&str, usize> = HashMap::new();
        for item in self.store.items.iter().filter(|i| self.running.contains_key(&i.id)) {
            *per_queue.entry(item.queue.as_str()).or_default() += 1;
        }
        self.store
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.status == Status::Queued)
            .filter(|(_, i)| {
                let limit = self.settings.queue_limit(&i.queue);
                limit == 0 || per_queue.get(i.queue.as_str()).copied().unwrap_or(0) < limit
            })
            .max_by_key(|(_, i)| (i.priority, std::cmp::Reverse(i.id)))
            .map(|(idx, _)| idx)
    }

    /// Count `bytes` against the download limit. When it is reached, returns
    /// the running downloads to stop (they restart when the window ends).
    fn count_quota(&mut self, bytes: u64) -> Vec<CancellationToken> {
        let limit = self.settings.quota_mb * 1024 * 1024;
        self.quota_bytes += bytes;
        if limit == 0 || self.quota_bytes < limit || self.quota_until.is_some() {
            return Vec::new();
        }
        self.quota_until = Some(self.quota_start + u64::from(self.settings.quota_hours) * 3600);
        self.quota_paused.extend(self.running.keys().copied());
        self.running.values().map(|r| r.token.clone()).collect()
    }

    /// Move downloads whose queue no longer exists into Main.
    fn adopt_orphans(&mut self) -> Vec<Item> {
        let mut moved = Vec::new();
        for item in &mut self.store.items {
            if !self.settings.has_queue(&item.queue) {
                item.queue = main_queue();
                moved.push(item.clone());
            }
        }
        moved
    }
}

/// How a download task ended, after checksum verification.
enum Ended {
    Completed(u64),
    Paused,
    Failed { message: String, retryable: bool },
}

/// First automatic restart waits this long, the next twice as long, and so on.
const RETRY_DELAY: Duration = Duration::from_secs(15);

pub struct Manager {
    events: Arc<dyn EventSink>,
    /// Rebuilt when the proxy settings change; running downloads keep theirs.
    engine: Mutex<Arc<Engine>>,
    base_opts: Options,
    global_limiter: Arc<RateLimiter>,
    data_dir: PathBuf,
    inner: Mutex<Inner>,
}

pub(crate) fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_json<T: Serialize>(path: &Path, value: &T) {
    let tmp = path.with_extension("json.tmp");
    let result = serde_json::to_vec_pretty(value)
        .map_err(std::io::Error::other)
        .and_then(|bytes| std::fs::write(&tmp, bytes))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = result {
        tracing::error!(path = %path.display(), error = %e, "failed to save");
    }
}

fn is_playlist(info: &dm_engine::ProbeInfo) -> bool {
    let ct = info.content_type.as_deref().unwrap_or("").to_ascii_lowercase();
    let name = info.file_name.to_ascii_lowercase();
    ct.contains("mpegurl") || ct.contains("dash+xml") || name.ends_with(".m3u8") || name.ends_with(".mpd")
}

/// A file name for a stream: the playlist's name unless it is a generic one
/// like `index.m3u8`, then the site's name. `.mp4` needs ffmpeg, else `.ts`.
fn stream_file_name(playlist_name: &str, url: &str, container: &str) -> String {
    let stem = playlist_name.rsplit_once('.').map_or(playlist_name, |(s, _)| s);
    const GENERIC: &[&str] = &["index", "master", "playlist", "manifest", "chunklist", "prog_index", "media", "video", "stream", "live", "mono"];
    let lower = stem.to_ascii_lowercase();
    let generic = lower.is_empty() || GENERIC.iter().any(|g| lower.starts_with(g)) || lower.chars().all(|c| !c.is_alphabetic());
    let stem = if generic {
        let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_string()));
        format!("{} video", host.unwrap_or_else(|| "stream".into()))
    } else {
        stem.to_string()
    };
    format!("{}.{container}", dm_engine::probe::sanitize_filename(&stem))
}

/// `name.ext`, `name (1).ext`, ... — first one not on disk and not used by
/// another download in the list.
fn unique_path(dir: &Path, name: &str, taken: &[&Path]) -> PathBuf {
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    (0..)
        .map(|n| if n == 0 { dir.join(name) } else { dir.join(format!("{stem} ({n}){ext}")) })
        .find(|p| {
            !p.exists()
                && !temp_paths(p).iter().chain(&hls::stream_temp_paths(p)).any(|t| t.exists())
                && !taken.contains(&p.as_path())
        })
        .expect("unbounded search")
}

/// Compare the file against a published MD5 / SHA-256 digest.
async fn verify(path: PathBuf, expected: String) -> Result<(), String> {
    let algo = Algo::for_digest(&expected).ok_or("the expected checksum is not a valid MD5 or SHA-256 value")?;
    let actual = tokio::task::spawn_blocking(move || checksum::hash_file(&path, algo))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("could not read the file to verify it: {e}"))?;
    if actual.eq_ignore_ascii_case(expected.trim()) {
        Ok(())
    } else {
        Err(format!("{} checksum mismatch: expected {}, got {actual}", algo.name(), expected.trim()))
    }
}

impl Manager {
    pub fn load(events: Arc<dyn EventSink>, data_dir: PathBuf, engine_opts: Options) -> Result<Arc<Self>, String> {
        std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
        let store: Store = read_json(&data_dir.join("downloads.json"));
        let mut settings: Settings = read_json(&data_dir.join("settings.json"));
        settings.normalize();
        let global_limiter = Arc::new(RateLimiter::new(settings.speed_limit_kib * 1024));
        // A broken proxy setting must not keep the app from starting.
        let engine = Engine::new(settings.engine_options(&engine_opts), global_limiter.clone())
            .or_else(|_| Engine::new(engine_opts.clone(), global_limiter.clone()))
            .map_err(|e| e.to_string())?;
        let mut inner = Inner {
            store,
            settings,
            running: HashMap::new(),
            last_schedule_minute: None,
            finishing: 0,
            retry_pending: HashSet::new(),
            completed_since_idle: false,
            quota_start: now(),
            quota_bytes: 0,
            quota_until: None,
            quota_paused: HashSet::new(),
        };
        // Nothing runs across restarts until the user resumes it.
        for item in &mut inner.store.items {
            if matches!(item.status, Status::Downloading | Status::Queued) {
                item.status = Status::Paused;
            }
            item.note = item.note.take().filter(|_| item.status == Status::Completed);
        }
        inner.adopt_orphans();
        Ok(Arc::new(Self {
            events,
            engine: Mutex::new(Arc::new(engine)),
            base_opts: engine_opts,
            global_limiter,
            data_dir,
            inner: Mutex::new(inner),
        }))
    }

    fn save(&self, inner: &Inner) {
        write_json(&self.data_dir.join("downloads.json"), &inner.store);
    }

    fn emit_item(&self, item: &Item) {
        self.events.item(item);
    }

    pub fn engine(&self) -> Arc<Engine> {
        self.engine.lock().unwrap().clone()
    }

    pub fn client(&self) -> reqwest::Client {
        self.engine().client().clone()
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn list(&self) -> Vec<Item> {
        self.inner.lock().unwrap().store.items.clone()
    }

    pub fn settings(&self) -> Settings {
        self.inner.lock().unwrap().settings.clone()
    }

    pub fn set_settings(self: &Arc<Self>, mut settings: Settings) {
        settings.normalize();
        self.global_limiter.set_rate(settings.speed_limit_kib * 1024);
        let proxy = settings.engine_options(&self.base_opts);
        if proxy.proxy != self.engine().options().proxy || proxy.no_proxy != self.engine().options().no_proxy {
            match Engine::new(proxy, self.global_limiter.clone()) {
                Ok(engine) => *self.engine.lock().unwrap() = Arc::new(engine),
                Err(e) => tracing::error!(error = %e, "proxy settings rejected"),
            }
        }
        write_json(&self.data_dir.join("settings.json"), &settings);
        let moved = {
            let mut inner = self.inner.lock().unwrap();
            inner.settings = settings;
            let moved = inner.adopt_orphans();
            if !moved.is_empty() {
                self.save(&inner);
            }
            moved
        };
        for item in &moved {
            self.emit_item(item);
        }
        self.pump();
    }

    fn target_dir(settings: &Settings, category: &str) -> PathBuf {
        if settings.sort_by_category {
            settings.download_dir.join(category)
        } else {
            settings.download_dir.clone()
        }
    }

    pub async fn probe(&self, url: String, headers: Vec<(String, String)>) -> Result<ProbeResult, String> {
        let original = RequestSpec { url: url.trim().to_string(), headers };
        let (url, headers) = self.settings().authorize(&original.url, &original.headers);
        let spec = RequestSpec { url, headers };
        let client = self.client();
        let info = probe(&client, &spec).await.map_err(|e| e.to_string())?;
        let stream = if is_playlist(&info) {
            Some(hls::inspect(&client, &spec).await.map_err(|e| e.to_string())?)
        } else {
            None
        };
        let is_page = stream.is_none() && info.content_type.as_deref().is_some_and(|c| c.to_ascii_lowercase().contains("text/html"));
        // A web page: maybe a video site yt-dlp can read (without our login header).
        let site = if is_page { Some(site::inspect(&original.url, &original.headers).await?) } else { None };
        let file_name = match (&stream, &site) {
            (Some(s), _) => stream_file_name(&info.file_name, &info.final_url, s.container),
            (_, Some(s)) => format!("{}.mp4", dm_engine::probe::sanitize_filename(&s.title)),
            _ => info.file_name.clone(),
        };
        let category = category_for(&file_name).to_string();
        let (save_dir, duplicate) = {
            let inner = self.inner.lock().unwrap();
            let duplicate = inner
                .store
                .items
                .iter()
                .rev()
                .find(|i| i.url == original.url || i.url == info.final_url)
                .map(|i| Duplicate { id: i.id, file_name: i.file_name.clone(), status: i.status });
            (Self::target_dir(&inner.settings, &category), duplicate)
        };
        Ok(ProbeResult {
            final_url: info.final_url,
            file_name,
            size: match (&stream, &site) {
                (Some(s), _) => s.estimated_size,
                (_, Some(s)) => s.formats.first().and_then(|f| f.size),
                _ => info.total_size,
            },
            resumable: match (&stream, &site) {
                (Some(s), _) => !s.live,
                (_, Some(s)) => !s.live,
                _ => info.accepts_ranges,
            },
            content_type: info.content_type,
            category,
            free_space: dm_engine::disk::available_space(&save_dir),
            save_dir,
            duplicate,
            stream,
            site,
        })
    }

    pub async fn add(self: &Arc<Self>, req: AddRequest) -> Result<Item, String> {
        let url = req.url.trim().to_string();
        let parsed = url::Url::parse(&url).map_err(|e| format!("invalid URL: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https" | "ftp") {
            return Err("only http, https and ftp links are supported".into());
        }
        let checksum = req.checksum.map(|c| c.trim().to_ascii_lowercase()).filter(|c| !c.is_empty());
        if checksum.as_deref().is_some_and(|c| Algo::for_digest(c).is_none()) {
            return Err("the checksum must be an MD5 (32 hex digits) or SHA-256 (64 hex digits) value".into());
        }
        let (file_name, size, resumable, stream, live, site_format) = match req.file_name.filter(|n| !n.trim().is_empty()) {
            Some(name) => (
                dm_engine::probe::sanitize_filename(&name),
                req.size,
                req.resumable.unwrap_or(false),
                req.stream,
                req.live,
                req.site_format.filter(|f| !f.is_empty()),
            ),
            None => {
                let p = self.probe(url.clone(), req.headers.clone()).await?;
                let live = p.stream.as_ref().map(|s| s.live).or(p.site.as_ref().map(|s| s.live)).unwrap_or(false);
                let format = p.site.as_ref().and_then(|s| s.formats.first()).map(|f| f.id.clone());
                (p.file_name, p.size, p.resumable, p.stream.is_some(), live, format)
            }
        };
        let category = category_for(&file_name).to_string();

        let item = {
            let mut inner = self.inner.lock().unwrap();
            let dir = req.save_dir.unwrap_or_else(|| Self::target_dir(&inner.settings, &category));
            let taken: Vec<&Path> = inner.store.items.iter().map(|i| i.save_path.as_path()).collect();
            let save_path = unique_path(&dir, &file_name, &taken);
            let queue = req.queue.filter(|q| inner.settings.has_queue(q)).unwrap_or_else(main_queue);
            inner.store.next_id += 1;
            let item = Item {
                id: inner.store.next_id,
                url,
                file_name: save_path.file_name().unwrap().to_string_lossy().into_owned(),
                save_path,
                category,
                size,
                downloaded: 0,
                status: if req.start { Status::Queued } else { Status::Paused },
                error: None,
                resumable,
                added_at: now(),
                started_at: None,
                finished_at: None,
                headers: req.headers,
                scheduled: req.scheduled,
                queue,
                priority: req.priority,
                speed_limit_kib: req.speed_limit_kib,
                checksum,
                open_when_done: req.open_when_done,
                done_command: req.done_command.filter(|c| !c.trim().is_empty()),
                note: None,
                auto_retries: 0,
                stream,
                live,
                variant: req.variant.filter(|v| stream && !v.is_empty()),
                site_format,
                sync_hours: 0,
                last_checked: None,
                remote_version: None,
                awaiting_refresh: false,
            };
            inner.store.items.push(item.clone());
            self.save(&inner);
            item
        };
        self.emit_item(&item);
        self.pump();
        Ok(item)
    }

    pub fn pause(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(r) = inner.running.get(&id) {
            // The task records the Paused state once the engine has saved.
            r.token.cancel();
            return;
        }
        // Stopping a failed download that waits for an automatic retry cancels the retry.
        let retrying = inner.retry_pending.remove(&id);
        if let Some(item) = inner
            .store
            .items
            .iter_mut()
            .find(|i| i.id == id && (i.status == Status::Queued || (retrying && i.status == Status::Error)))
        {
            item.status = Status::Paused;
            item.error = None;
            let item = item.clone();
            self.save(&inner);
            drop(inner);
            self.emit_item(&item);
        }
    }

    pub fn resume(self: &Arc<Self>, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        inner.retry_pending.remove(&id);
        if let Some(item) = inner
            .store
            .items
            .iter_mut()
            .find(|i| i.id == id && matches!(i.status, Status::Paused | Status::Error))
        {
            item.status = Status::Queued;
            item.error = None;
            item.note = None;
            item.auto_retries = 0;
            let item = item.clone();
            self.save(&inner);
            drop(inner);
            self.emit_item(&item);
            self.pump();
        }
    }

    pub fn remove(self: &Arc<Self>, id: u64, delete_files: bool) {
        let (item, token) = {
            let mut inner = self.inner.lock().unwrap();
            let Some(pos) = inner.store.items.iter().position(|i| i.id == id) else { return };
            let item = inner.store.items.remove(pos);
            let token = inner.running.get(&id).map(|r| r.token.clone());
            inner.retry_pending.remove(&id);
            self.save(&inner);
            (item, token)
        };
        self.events.removed(id);
        if let Some(t) = &token {
            t.cancel();
        }
        let this = self.clone();
        tauri::async_runtime::spawn(async move {
            // Wait for the engine to release its file handles first.
            for _ in 0..100 {
                if !this.inner.lock().unwrap().running.contains_key(&id) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            for temp in temp_paths(&item.save_path) {
                let _ = tokio::fs::remove_file(temp).await;
            }
            for leftover in site::temp_files(&item.save_path) {
                let _ = tokio::fs::remove_file(leftover).await;
            }
            let [segments, audio] = hls::stream_temp_paths(&item.save_path);
            let _ = tokio::fs::remove_dir_all(segments).await;
            let _ = tokio::fs::remove_file(audio).await;
            if delete_files && item.status == Status::Completed {
                let _ = tokio::fs::remove_file(&item.save_path).await;
            }
        });
    }

    /// Start queued downloads while there are free slots.
    pub fn pump(self: &Arc<Self>) {
        loop {
            let mut inner = self.inner.lock().unwrap();
            if inner.running.len() >= inner.settings.max_concurrent.max(1) || inner.quota_until.is_some() {
                return;
            }
            let Some(idx) = inner.next_to_start() else { return };
            let item = &mut inner.store.items[idx];
            item.status = Status::Downloading;
            let item = item.clone();
            let token = CancellationToken::new();
            let limiter = Arc::new(RateLimiter::new(item.speed_limit_kib * 1024));
            inner.running.insert(item.id, Running { token: token.clone(), limiter: limiter.clone() });
            let connections = inner.settings.connections_for(&item.url);
            drop(inner);
            self.emit_item(&item);
            self.clone().spawn_download(item, connections, token, limiter);
        }
    }

    fn spawn_download(
        self: Arc<Self>,
        item: Item,
        connections: usize,
        token: CancellationToken,
        limiter: Arc<RateLimiter>,
    ) {
        if item.started_at.is_none() {
            self.update_items(&[item.id], |i| i.started_at = Some(now()));
        }
        let start_command = self.settings().start_command;
        if !start_command.trim().is_empty() {
            if let Err(e) = post::run_command(&start_command, &post::Vars::of(&item)) {
                tracing::error!(error = %e, "start command failed");
            }
        }
        tauri::async_runtime::spawn(async move {
            let (url, headers) = self.settings().authorize(&item.url, &item.headers);
            let job = Job { spec: RequestSpec { url, headers }, save_path: item.save_path.clone(), connections };
            let id = item.id;
            let this = self.clone();
            let on_progress = move |p: &Progress| {
                let stop = {
                    let mut inner = this.inner.lock().unwrap();
                    let mut delta = 0;
                    if let Some(i) = inner.store.items.iter_mut().find(|i| i.id == id) {
                        delta = p.downloaded.saturating_sub(i.downloaded);
                        i.downloaded = p.downloaded;
                        i.size = p.total.or(i.size);
                        i.resumable = p.accepts_ranges && !i.live;
                    }
                    inner.count_quota(delta)
                };
                for token in stop {
                    token.cancel();
                }
                this.events.progress(id, p);
            };
            let engine = self.engine();
            let result: Result<Outcome, (String, bool)> = if let Some(format) = &item.site_format {
                site::download(&item.url, &item.headers, format, &item.save_path, connections, item.live, token, on_progress).await
            } else if item.stream {
                engine
                    .download_stream(&job, item.variant.as_deref(), Some(limiter), token, on_progress)
                    .await
                    .map_err(|e| (e.to_string(), e.is_retryable()))
            } else {
                engine.download(&job, Some(limiter), token, on_progress).await.map_err(|e| (e.to_string(), e.is_retryable()))
            };

            let (mut ended, saved_as) = match result {
                Ok(Outcome::Completed { size, path }) => (Ended::Completed(size), Some(path)),
                Ok(Outcome::Paused) => (Ended::Paused, None),
                Err((message, retryable)) => (Ended::Failed { message, retryable }, None),
            };
            if let (Ended::Completed(_), Some(expected)) = (&ended, &item.checksum) {
                self.set_note(id, Some("Verifying checksum…".into()));
                let path = saved_as.clone().unwrap_or_else(|| item.save_path.clone());
                if let Err(message) = verify(path, expected.clone()).await {
                    ended = Ended::Failed { message, retryable: false };
                }
            }

            let (updated, retry) = {
                let mut inner = self.inner.lock().unwrap();
                inner.running.remove(&id);
                let max_retries = inner.settings.auto_retry;
                let mut retry = None;
                let limited = inner.quota_paused.contains(&id).then(|| inner.quota_until).flatten();
                let updated = inner.store.items.iter_mut().find(|i| i.id == id).map(|i| {
                    i.note = None;
                    match &ended {
                        Ended::Completed(size) => {
                            i.status = Status::Completed;
                            i.size = Some(*size);
                            i.downloaded = *size;
                            i.finished_at = Some(now());
                            i.auto_retries = 0;
                            // yt-dlp picks the extension (.mp4, .m4a, .webm).
                            if let Some(path) = saved_as.as_ref().filter(|p| **p != i.save_path) {
                                i.file_name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                                i.category = category_for(&i.file_name).to_string();
                                i.save_path = path.clone();
                            }
                        }
                        Ended::Paused => {
                            i.status = Status::Paused;
                            if let Some(until) = limited {
                                i.note = Some(format!("Download limit reached; continues at {}", local_hhmm(until)));
                            }
                        }
                        Ended::Failed { message, retryable } => {
                            i.status = Status::Error;
                            i.error = Some(message.clone());
                            if *retryable && i.auto_retries < max_retries {
                                i.auto_retries += 1;
                                let delay = RETRY_DELAY * 2u32.pow(i.auto_retries - 1);
                                i.error = Some(format!("{message} (retrying in {} s)", delay.as_secs()));
                                retry = Some((delay, i.auto_retries));
                            }
                        }
                    }
                    i.clone()
                });
                if retry.is_some() {
                    inner.retry_pending.insert(id);
                }
                if updated.as_ref().is_some_and(|i| i.status == Status::Completed) {
                    inner.completed_since_idle = true;
                    inner.finishing += 1;
                }
                self.save(&inner);
                (updated, retry)
            };
            if let Some(item) = &updated {
                self.emit_item(item);
                if retry.is_none() && matches!(item.status, Status::Completed | Status::Error) {
                    self.events.finished(item);
                }
            }
            if let Some((delay, attempt)) = retry {
                self.clone().retry_later(id, delay, attempt);
            }
            self.pump();
            let completed = updated.filter(|i| i.status == Status::Completed);
            let just_completed = completed.is_some();
            if let Some(item) = completed {
                if item.sync_hours > 0 {
                    self.record_version(item.id).await;
                }
                self.after_download(item).await;
                self.inner.lock().unwrap().finishing -= 1;
            }
            self.check_idle(just_completed);
        });
    }

    /// Restart a download that failed with a network error, unless the user
    /// touched it in the meantime.
    fn retry_later(self: Arc<Self>, id: u64, delay: Duration, attempt: u32) {
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(delay).await;
            let item = {
                let mut inner = self.inner.lock().unwrap();
                if !inner.retry_pending.remove(&id) {
                    return;
                }
                let Some(item) = inner
                    .store
                    .items
                    .iter_mut()
                    .find(|i| i.id == id && i.status == Status::Error && i.auto_retries == attempt)
                else {
                    return;
                };
                item.status = Status::Queued;
                item.error = None;
                let item = item.clone();
                self.save(&inner);
                item
            };
            self.emit_item(&item);
            self.pump();
            self.check_idle(false);
        });
    }

    /// Scan, extract and open a finished file, as configured.
    async fn after_download(&self, item: Item) {
        let settings = self.settings();
        let path = item.save_path.clone();
        let mut notes = Vec::new();
        if !settings.scan_command.trim().is_empty() {
            self.set_note(item.id, Some("Scanning for viruses…".into()));
            let (cmd, file) = (settings.scan_command.clone(), path.clone());
            let scanned =
                tokio::task::spawn_blocking(move || post::scan(&cmd, &file)).await.unwrap_or_else(|e| Err(e.to_string()));
            if let Err(message) = scanned {
                // Never extract or open a file the scanner objected to.
                self.set_note(item.id, Some(message));
                return;
            }
            notes.push("No threats found".to_string());
        }
        if settings.extract_archives {
            if let Some(into) = post::extract_dir(&path).map(|d| post::unique_dir(&d)) {
                self.set_note(item.id, Some("Extracting…".into()));
                let (file, dir) = (path.clone(), into.clone());
                let extracted = tokio::task::spawn_blocking(move || post::extract(&file, &dir))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()));
                notes.push(match extracted {
                    Ok(()) => format!("Extracted to {}", into.display()),
                    Err(e) => e,
                });
            }
        }
        self.set_note(item.id, (!notes.is_empty()).then(|| notes.join(" · ")));
        // yt-dlp may have changed the file name; use the final one.
        let item = self.find(item.id).unwrap_or(item);
        if !settings.finish_command.trim().is_empty() {
            if let Err(e) = post::run_command(&settings.finish_command, &post::Vars::of(&item)) {
                tracing::error!(error = %e, "finish command failed");
            }
        }
        if item.open_when_done != OpenWhenDone::Nothing && self.find(item.id).is_some() {
            self.events.open(&item);
        }
    }

    /// Fire [`EventSink::idle`] when the download that just completed was the
    /// last one. Downloads the user stopped do not count as finishing the list.
    fn check_idle(&self, just_completed: bool) {
        let fire = {
            let mut inner = self.inner.lock().unwrap();
            let busy = !inner.running.is_empty()
                || inner.finishing > 0
                || !inner.retry_pending.is_empty()
                || inner.store.items.iter().any(|i| i.status == Status::Queued);
            let fire = !busy && just_completed && inner.completed_since_idle;
            if !busy {
                inner.completed_since_idle = false;
            }
            fire
        };
        if fire {
            self.events.idle();
        }
    }

    fn set_note(&self, id: u64, note: Option<String>) {
        let item = {
            let mut inner = self.inner.lock().unwrap();
            let item = inner.store.items.iter_mut().find(|i| i.id == id).map(|i| {
                i.note = note;
                i.clone()
            });
            if item.is_some() {
                self.save(&inner);
            }
            item
        };
        if let Some(item) = item {
            self.emit_item(&item);
        }
    }

    /// Hex digest of a downloaded file, for the "Verify checksum" dialog.
    pub async fn checksum(&self, id: u64, algo: Algo) -> Result<String, String> {
        let item = self.find(id).ok_or("download not found")?;
        if item.status != Status::Completed {
            return Err("the download has not finished yet".into());
        }
        tokio::task::spawn_blocking(move || checksum::hash_file(&item.save_path, algo))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| format!("could not read the file: {e}"))
    }

    /// Apply `change` to the listed downloads, save once and report them.
    fn update_items(self: &Arc<Self>, ids: &[u64], change: impl Fn(&mut Item)) {
        let changed: Vec<Item> = {
            let mut inner = self.inner.lock().unwrap();
            let changed: Vec<Item> = inner
                .store
                .items
                .iter_mut()
                .filter(|i| ids.contains(&i.id))
                .map(|i| {
                    change(i);
                    i.clone()
                })
                .collect();
            self.save(&inner);
            changed
        };
        for item in &changed {
            self.emit_item(item);
        }
        self.pump();
    }

    pub fn set_queue(self: &Arc<Self>, ids: &[u64], queue: &str) -> Result<(), String> {
        if !self.settings().has_queue(queue) {
            return Err(format!("there is no queue named \"{queue}\""));
        }
        self.update_items(ids, |i| i.queue = queue.to_string());
        Ok(())
    }

    pub fn set_priority(self: &Arc<Self>, ids: &[u64], priority: Priority) {
        self.update_items(ids, |i| i.priority = priority);
    }

    /// Change one download's own limit; applies immediately if it is running.
    pub fn set_speed_limit(self: &Arc<Self>, id: u64, kib: u64) {
        self.update_items(&[id], |i| i.speed_limit_kib = kib);
        if let Some(r) = self.inner.lock().unwrap().running.get(&id) {
            r.limiter.set_rate(kib * 1024);
        }
    }

    fn ids_in_queue(&self, queue: &str, statuses: &[Status]) -> Vec<u64> {
        let inner = self.inner.lock().unwrap();
        inner.store.items.iter().filter(|i| i.queue == queue && statuses.contains(&i.status)).map(|i| i.id).collect()
    }

    /// Resume every stopped or failed download in the queue.
    pub fn start_queue(self: &Arc<Self>, queue: &str) {
        for id in self.ids_in_queue(queue, &[Status::Paused, Status::Error]) {
            self.resume(id);
        }
    }

    pub fn stop_queue(&self, queue: &str) {
        for id in self.ids_in_queue(queue, &[Status::Downloading, Status::Queued, Status::Error]) {
            self.pause(id);
        }
    }

    /// Point a stopped download at a new address (e.g. an expired link that
    /// was regenerated). Resume re-validates size/ETag and restarts if needed.
    pub fn set_url(&self, id: u64, url: String) -> Result<Item, String> {
        let parsed = url::Url::parse(url.trim()).map_err(|e| format!("invalid URL: {e}"))?;
        if !matches!(parsed.scheme(), "http" | "https" | "ftp") {
            return Err("only http, https and ftp links are supported".into());
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.running.contains_key(&id) {
            return Err("stop the download before changing its address".into());
        }
        let item = inner.store.items.iter_mut().find(|i| i.id == id).ok_or("download not found")?;
        if item.url != parsed.as_str() {
            item.url = parsed.to_string();
            // Captured cookies belonged to the old address.
            item.headers.clear();
        }
        let item = item.clone();
        self.save(&inner);
        drop(inner);
        self.emit_item(&item);
        Ok(item)
    }

    /// Stop every running download so its progress is saved, waiting up to
    /// `timeout` for the engines to flush. Used when the app quits.
    pub async fn shutdown(&self, timeout: Duration) {
        let tokens: Vec<_> = self.inner.lock().unwrap().running.values().map(|r| r.token.clone()).collect();
        for t in tokens {
            t.cancel();
        }
        let deadline = tokio::time::Instant::now() + timeout;
        while !self.inner.lock().unwrap().running.is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Start/stop scheduled downloads. Called a few times per minute with the
    /// local time; `epoch_minute` de-duplicates calls within the same minute.
    pub fn scheduler_tick(self: &Arc<Self>, epoch_minute: i64, weekday: u8, minute_of_day: u32) {
        let (changed, to_stop) = {
            let mut inner = self.inner.lock().unwrap();
            let sched = inner.settings.schedule.clone();
            if !sched.enabled || !sched.days.contains(&weekday) || inner.last_schedule_minute == Some(epoch_minute) {
                return;
            }
            inner.last_schedule_minute = Some(epoch_minute);

            let mut changed = Vec::new();
            if parse_hhmm(&sched.start) == Some(minute_of_day) {
                for item in inner.store.items.iter_mut().filter(|i| i.scheduled) {
                    if matches!(item.status, Status::Paused | Status::Error) {
                        item.status = Status::Queued;
                        item.error = None;
                        changed.push(item.clone());
                    }
                }
            }
            for item in &changed {
                inner.retry_pending.remove(&item.id);
            }
            let mut to_stop = Vec::new();
            if parse_hhmm(&sched.stop) == Some(minute_of_day) {
                for item in inner.store.items.iter().filter(|i| i.scheduled) {
                    if let Some(r) = inner.running.get(&item.id) {
                        to_stop.push(r.token.clone());
                    }
                }
                for item in inner.store.items.iter_mut().filter(|i| i.scheduled && i.status == Status::Queued) {
                    item.status = Status::Paused;
                    changed.push(item.clone());
                }
            }
            if !changed.is_empty() {
                self.save(&inner);
            }
            (changed, to_stop)
        };
        for item in &changed {
            self.emit_item(item);
        }
        for token in to_stop {
            token.cancel();
        }
        self.pump();
    }

    pub fn find(&self, id: u64) -> Option<Item> {
        self.inner.lock().unwrap().store.items.iter().find(|i| i.id == id).cloned()
    }

    /// Start a new download-limit window when the current one is over, and
    /// restart what the limit stopped. Called every few seconds.
    pub fn quota_tick(self: &Arc<Self>, now: u64) {
        let restarted = {
            let mut inner = self.inner.lock().unwrap();
            let window = u64::from(inner.settings.quota_hours.max(1)) * 3600;
            if now < inner.quota_start + window {
                return;
            }
            inner.quota_start = now;
            inner.quota_bytes = 0;
            inner.quota_until = None;
            let ids: Vec<u64> = inner.quota_paused.drain().collect();
            let restarted: Vec<Item> = inner
                .store
                .items
                .iter_mut()
                .filter(|i| ids.contains(&i.id) && i.status == Status::Paused)
                .map(|i| {
                    i.status = Status::Queued;
                    i.note = None;
                    i.clone()
                })
                .collect();
            if !restarted.is_empty() {
                self.save(&inner);
            }
            restarted
        };
        for item in &restarted {
            self.emit_item(item);
        }
        self.pump();
    }

    /// The server's idea of the file's version (validators and size).
    async fn remote_version(&self, item: &Item) -> Option<String> {
        let (url, headers) = self.settings().authorize(&item.url, &item.headers);
        let info = probe(&self.client(), &RequestSpec { url, headers }).await.ok()?;
        Some(format!(
            "{}|{}|{}",
            info.etag.unwrap_or_default(),
            info.last_modified.unwrap_or_default(),
            info.total_size.map(|s| s.to_string()).unwrap_or_default()
        ))
    }

    /// Remember the server's current version as the one on disk.
    async fn record_version(&self, id: u64) {
        let Some(item) = self.find(id) else { return };
        let version = self.remote_version(&item).await;
        let mut inner = self.inner.lock().unwrap();
        if let Some(i) = inner.store.items.iter_mut().find(|i| i.id == id) {
            i.remote_version = version;
            i.last_checked = Some(now());
        }
        self.save(&inner);
    }

    /// Keep downloads in sync: re-check every `hours` (0 = off).
    pub fn set_sync(self: &Arc<Self>, ids: &[u64], hours: u32) {
        self.update_items(ids, |i| {
            i.sync_hours = hours;
            if hours == 0 {
                i.remote_version = None;
            }
        });
        for id in ids.iter().copied().filter(|_| hours > 0) {
            let this = self.clone();
            tauri::async_runtime::spawn(async move {
                if this.find(id).is_some_and(|i| i.status == Status::Completed && i.remote_version.is_none()) {
                    this.record_version(id).await;
                }
            });
        }
    }

    /// Download again the synced files whose server copy changed. Called
    /// from the background loop.
    pub async fn sync_tick(self: &Arc<Self>, now: u64) {
        let due: Vec<Item> = {
            let inner = self.inner.lock().unwrap();
            inner
                .store
                .items
                .iter()
                .filter(|i| i.sync_hours > 0 && i.status == Status::Completed && i.site_format.is_none() && !i.stream)
                .filter(|i| now.saturating_sub(i.last_checked.unwrap_or(0)) >= u64::from(i.sync_hours) * 3600)
                .cloned()
                .collect()
        };
        for item in due {
            let version = self.remote_version(&item).await;
            let changed = {
                let mut inner = self.inner.lock().unwrap();
                let Some(i) = inner.store.items.iter_mut().find(|i| i.id == item.id && i.status == Status::Completed) else {
                    continue;
                };
                i.last_checked = Some(now);
                let changed = version.is_some() && i.remote_version.is_some() && version != i.remote_version;
                if version.is_some() {
                    i.remote_version = version;
                }
                let changed = changed.then(|| {
                    i.status = Status::Queued;
                    i.downloaded = 0;
                    i.started_at = None;
                    i.finished_at = None;
                    i.note = Some("A newer version is on the server; downloading it".into());
                    i.clone()
                });
                self.save(&inner);
                changed
            };
            if let Some(i) = changed {
                self.emit_item(&i);
                self.pump();
            }
        }
    }

    /// The user wants a fresh address for an expired link: mark the download
    /// and return the page to open in the browser (the referrer if known).
    pub fn request_refresh(&self, id: u64) -> Result<String, String> {
        let item = {
            let mut inner = self.inner.lock().unwrap();
            if inner.running.contains_key(&id) {
                return Err("stop the download first".into());
            }
            let item = inner.store.items.iter_mut().find(|i| i.id == id).ok_or("download not found")?;
            if item.status == Status::Completed {
                return Err("the download is already complete".into());
            }
            item.awaiting_refresh = true;
            let item = item.clone();
            self.save(&inner);
            item
        };
        self.emit_item(&item);
        let referrer = item.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("referer")).map(|(_, v)| v.clone());
        Ok(referrer.unwrap_or(item.url))
    }

    /// A download captured from the browser: if it is the file a download is
    /// waiting a fresh address for (same name, or same size), take the new
    /// address and continue. Returns the resumed download.
    pub async fn try_refresh(self: &Arc<Self>, url: &str, headers: &[(String, String)], name: Option<&str>) -> Option<Item> {
        let waiting: Vec<Item> = self.list().into_iter().filter(|i| i.awaiting_refresh).collect();
        if waiting.is_empty() {
            return None;
        }
        let info = probe(&self.client(), &RequestSpec { url: url.to_string(), headers: headers.to_vec() }).await.ok();
        let names: Vec<String> = name.into_iter().map(dm_engine::probe::sanitize_filename).chain(info.as_ref().map(|i| i.file_name.clone())).collect();
        let size = info.as_ref().and_then(|i| i.total_size);
        let target = waiting.iter().find(|i| {
            let stem_of = |n: &str| n.rsplit_once('.').map_or(n.to_string(), |(s, _)| s.to_string());
            let same_name = names.iter().any(|n| *n == i.file_name || stem_of(n) == stem_of(&i.file_name));
            let same_size = size.is_some() && i.size == size;
            same_name || same_size
        })?;
        let id = target.id;
        let item = {
            let mut inner = self.inner.lock().unwrap();
            let i = inner.store.items.iter_mut().find(|i| i.id == id)?;
            i.url = url.to_string();
            i.headers = headers.to_vec();
            i.awaiting_refresh = false;
            i.status = Status::Queued;
            i.error = None;
            i.auto_retries = 0;
            let item = i.clone();
            self.save(&inner);
            item
        };
        self.emit_item(&item);
        self.pump();
        Some(item)
    }
}

/// Unix seconds → local "HH:MM".
fn local_hhmm(secs: u64) -> String {
    use chrono::TimeZone;
    chrono::Local.timestamp_opt(secs as i64, 0).single().map_or_else(String::new, |t| t.format("%H:%M").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Path as UrlPath;
    use dm_engine::{part_path, state_path};
    use axum::http::{header, HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use futures_util::StreamExt;

    const LEN: usize = 512 * 1024;

    fn body() -> Vec<u8> {
        (0..LEN).map(|i| (i * 31 % 251) as u8).collect()
    }

    /// `/fast/<name>` and `/slow/<name>` (slow = ~3s total), both with ranges.
    async fn serve(UrlPath((speed, _name)): UrlPath<(String, String)>, headers: HeaderMap) -> Response {
        let data = body();
        let start = headers
            .get(header::RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("bytes="))
            .and_then(|v| v.split('-').next()?.parse::<usize>().ok());
        let (status, from) = match start {
            Some(s) => (StatusCode::PARTIAL_CONTENT, s),
            None => (StatusCode::OK, 0),
        };
        let delay = if speed == "slow" { Some(Duration::from_millis(100)) } else { None };
        let chunks: Vec<Vec<u8>> = data[from..].chunks(16 * 1024).map(<[u8]>::to_vec).collect();
        let stream = futures_util::stream::iter(chunks).then(move |c| async move {
            if let Some(d) = delay {
                tokio::time::sleep(d).await;
            }
            Ok::<_, std::io::Error>(c)
        });
        let mut resp = (status, axum::body::Body::from_stream(stream)).into_response();
        let h = resp.headers_mut();
        h.insert(header::CONTENT_LENGTH, (LEN - from).into());
        h.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
        if status == StatusCode::PARTIAL_CONTENT {
            h.insert(header::CONTENT_RANGE, format!("bytes {from}-{}/{LEN}", LEN - 1).parse().unwrap());
        }
        resp
    }

    async fn server() -> String {
        let app = axum::Router::new().route("/{speed}/{name}", get(serve));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[derive(Default)]
    struct Recorder {
        items: Mutex<Vec<Item>>,
        removed: Mutex<Vec<u64>>,
        finished: Mutex<Vec<Item>>,
        idle: std::sync::atomic::AtomicUsize,
    }

    impl EventSink for Recorder {
        fn item(&self, item: &Item) {
            self.items.lock().unwrap().push(item.clone());
        }
        fn progress(&self, _id: u64, _p: &Progress) {}
        fn removed(&self, id: u64) {
            self.removed.lock().unwrap().push(id);
        }
        fn finished(&self, item: &Item) {
            self.finished.lock().unwrap().push(item.clone());
        }
        fn idle(&self) {
            self.idle.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    struct Fixture {
        mgr: Arc<Manager>,
        events: Arc<Recorder>,
        base: String,
        dir: tempfile::TempDir,
    }

    async fn fixture(max_concurrent: usize) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let events = Arc::new(Recorder::default());
        let opts = Options { progress_interval: Duration::from_millis(50), min_split: 64 * 1024, ..Options::default() };
        let mgr = Manager::load(events.clone(), dir.path().join("data"), opts).unwrap();
        mgr.set_settings(Settings {
            download_dir: dir.path().join("dl"),
            connections: 4,
            max_concurrent,
            sort_by_category: true,
            ..Settings::default()
        });
        Fixture { mgr, events, base: server().await, dir }
    }

    fn add_req(url: String) -> AddRequest {
        AddRequest {
            url,
            file_name: None,
            save_dir: None,
            headers: vec![],
            start: true,
            scheduled: false,
            size: None,
            resumable: None,
            queue: None,
            priority: Priority::Normal,
            checksum: None,
            open_when_done: OpenWhenDone::Nothing,
            done_command: None,
            speed_limit_kib: 0,
            stream: false,
            live: false,
            variant: None,
            site_format: None,
        }
    }

    async fn wait_for(mgr: &Manager, id: u64, want: Status) -> Item {
        for _ in 0..200 {
            let item = mgr.find(id).unwrap();
            if item.status == want {
                return item;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("download {id} never reached {want:?}: {:?}", mgr.find(id));
    }

    #[tokio::test]
    async fn completes_into_category_folder_and_persists() {
        let f = fixture(3).await;
        let item = f.mgr.add(add_req(format!("{}/fast/video.mp4", f.base))).await.unwrap();
        assert_eq!(item.category, "Video");
        assert_eq!(item.size, Some(LEN as u64));
        assert!(item.resumable);

        let done = wait_for(&f.mgr, item.id, Status::Completed).await;
        assert_eq!(done.save_path, f.dir.path().join("dl").join("Video").join("video.mp4"));
        assert_eq!(std::fs::read(&done.save_path).unwrap(), body());
        assert!(f.events.items.lock().unwrap().iter().any(|i| i.status == Status::Completed));

        let saved: Store = read_json(&f.dir.path().join("data").join("downloads.json"));
        assert_eq!(saved.items.len(), 1);
        assert_eq!(saved.items[0].status, Status::Completed);
    }

    #[tokio::test]
    async fn respects_max_concurrent() {
        let f = fixture(1).await;
        let a = f.mgr.add(add_req(format!("{}/slow/a.zip", f.base))).await.unwrap();
        let b = f.mgr.add(add_req(format!("{}/slow/b.zip", f.base))).await.unwrap();
        wait_for(&f.mgr, a.id, Status::Downloading).await;
        assert_eq!(f.mgr.find(b.id).unwrap().status, Status::Queued);

        f.mgr.pause(a.id);
        wait_for(&f.mgr, a.id, Status::Paused).await;
        // The freed slot goes to the next queued download.
        wait_for(&f.mgr, b.id, Status::Downloading).await;
        f.mgr.pause(b.id);
        wait_for(&f.mgr, b.id, Status::Paused).await;
    }

    #[tokio::test]
    async fn pause_then_resume_completes() {
        let f = fixture(3).await;
        let item = f.mgr.add(add_req(format!("{}/slow/doc.pdf", f.base))).await.unwrap();
        wait_for(&f.mgr, item.id, Status::Downloading).await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        f.mgr.pause(item.id);
        let paused = wait_for(&f.mgr, item.id, Status::Paused).await;
        assert!(paused.downloaded > 0 && paused.downloaded < LEN as u64, "{}", paused.downloaded);
        assert!(state_path(&paused.save_path).exists());

        f.mgr.resume(item.id);
        let done = wait_for(&f.mgr, item.id, Status::Completed).await;
        assert_eq!(std::fs::read(&done.save_path).unwrap(), body());
    }

    #[tokio::test]
    async fn same_file_twice_gets_numbered_name() {
        let f = fixture(3).await;
        let a = f.mgr.add(add_req(format!("{}/fast/song.mp3", f.base))).await.unwrap();
        let b = f.mgr.add(add_req(format!("{}/fast/song.mp3", f.base))).await.unwrap();
        assert_eq!(a.file_name, "song.mp3");
        assert_eq!(b.file_name, "song (1).mp3");
        wait_for(&f.mgr, a.id, Status::Completed).await;
        wait_for(&f.mgr, b.id, Status::Completed).await;
    }

    #[tokio::test]
    async fn remove_cleans_up_partial_files() {
        let f = fixture(3).await;
        let item = f.mgr.add(add_req(format!("{}/slow/big.iso", f.base))).await.unwrap();
        wait_for(&f.mgr, item.id, Status::Downloading).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        f.mgr.remove(item.id, true);
        assert!(f.mgr.find(item.id).is_none());
        assert_eq!(*f.events.removed.lock().unwrap(), vec![item.id]);
        for _ in 0..100 {
            if !part_path(&item.save_path).exists() && !state_path(&item.save_path).exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("partial files left behind");
    }

    #[tokio::test]
    async fn rejects_non_http_links() {
        let f = fixture(3).await;
        let err = f.mgr.add(add_req("gopher://example.com/x.zip".into())).await.unwrap_err();
        assert!(err.contains("http"), "{err}");
        let err = f.mgr.add(add_req("not a url".into())).await.unwrap_err();
        assert!(err.contains("invalid URL"), "{err}");
    }

    #[tokio::test]
    async fn restart_turns_running_into_paused() {
        let f = fixture(3).await;
        let data_dir = f.dir.path().join("data");
        let mut store: Store = read_json(&data_dir.join("downloads.json"));
        store.next_id = 7;
        store.items.push(Item {
            id: 7,
            url: "http://x/a.zip".into(),
            file_name: "a.zip".into(),
            save_path: f.dir.path().join("a.zip"),
            category: "Compressed".into(),
            size: Some(10),
            downloaded: 5,
            status: Status::Downloading,
            error: None,
            resumable: true,
            added_at: 0,
            started_at: None,
            finished_at: None,
            headers: vec![],
            scheduled: false,
            queue: "Deleted queue".into(),
            priority: Priority::Normal,
            speed_limit_kib: 0,
            checksum: None,
            open_when_done: OpenWhenDone::Nothing,
            done_command: None,
            note: None,
            auto_retries: 0,
            stream: false,
            live: false,
            variant: None,
            site_format: None,
            sync_hours: 0,
            last_checked: None,
            remote_version: None,
            awaiting_refresh: false,
        });
        write_json(&data_dir.join("downloads.json"), &store);

        let reloaded = Manager::load(Arc::new(Recorder::default()), data_dir, Options::default()).unwrap();
        assert_eq!(reloaded.find(7).unwrap().status, Status::Paused);
        assert_eq!(reloaded.find(7).unwrap().queue, MAIN_QUEUE, "downloads of a missing queue move to Main");
        assert_eq!(reloaded.settings().max_concurrent, 3, "settings survive restart");
    }

    #[tokio::test]
    async fn changed_address_resumes_same_file() {
        let f = fixture(3).await;
        let item = f.mgr.add(add_req(format!("{}/slow/old-link.bin", f.base))).await.unwrap();
        wait_for(&f.mgr, item.id, Status::Downloading).await;
        assert!(f.mgr.set_url(item.id, format!("{}/fast/new-link.bin", f.base)).is_err(), "must stop first");
        tokio::time::sleep(Duration::from_millis(300)).await;
        f.mgr.pause(item.id);
        wait_for(&f.mgr, item.id, Status::Paused).await;

        let updated = f.mgr.set_url(item.id, format!("{}/fast/new-link.bin", f.base)).unwrap();
        assert!(updated.url.ends_with("/fast/new-link.bin"));
        assert_eq!(updated.file_name, "old-link.bin", "file name and location are kept");
        f.mgr.resume(item.id);
        let done = wait_for(&f.mgr, item.id, Status::Completed).await;
        assert_eq!(std::fs::read(&done.save_path).unwrap(), body());
        assert!(f.mgr.set_url(item.id, "gopher://x/y".into()).is_err());
    }

    #[tokio::test]
    async fn add_with_dialog_info_keeps_size_and_resume_flag() {
        let f = fixture(3).await;
        let req = AddRequest {
            file_name: Some("renamed.bin".into()),
            size: Some(LEN as u64),
            resumable: Some(true),
            start: false,
            ..add_req(format!("{}/fast/original.bin", f.base))
        };
        let item = f.mgr.add(req).await.unwrap();
        assert_eq!(item.file_name, "renamed.bin");
        assert_eq!(item.size, Some(LEN as u64));
        assert!(item.resumable);
    }

    #[test]
    fn parses_times() {
        assert_eq!(parse_hhmm("02:00"), Some(120));
        assert_eq!(parse_hhmm(" 23:59 "), Some(1439));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm(""), None);
        assert_eq!(parse_hhmm("7"), None);
    }

    fn enable_schedule(mgr: &Arc<Manager>, start: &str, stop: &str, days: Vec<u8>) {
        let mut s = mgr.settings();
        s.schedule = Schedule { enabled: true, start: start.into(), stop: stop.into(), days };
        mgr.set_settings(s);
    }

    #[tokio::test]
    async fn scheduler_starts_only_scheduled_downloads_once() {
        let f = fixture(3).await;
        let later = AddRequest { start: false, scheduled: true, ..add_req(format!("{}/slow/night.iso", f.base)) };
        let manual = AddRequest { start: false, ..add_req(format!("{}/slow/manual.iso", f.base)) };
        let later = f.mgr.add(later).await.unwrap();
        let manual = f.mgr.add(manual).await.unwrap();
        enable_schedule(&f.mgr, "02:00", "", vec![0, 1, 2, 3, 4, 5, 6]);

        f.mgr.scheduler_tick(1000, 2, 119); // 01:59: nothing yet
        assert_eq!(f.mgr.find(later.id).unwrap().status, Status::Paused);

        f.mgr.scheduler_tick(1001, 2, 120); // 02:00
        wait_for(&f.mgr, later.id, Status::Downloading).await;
        assert_eq!(f.mgr.find(manual.id).unwrap().status, Status::Paused, "manual pauses are left alone");

        // User pauses it within the same minute: a repeated tick must not restart it.
        f.mgr.pause(later.id);
        wait_for(&f.mgr, later.id, Status::Paused).await;
        f.mgr.scheduler_tick(1001, 2, 120);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(f.mgr.find(later.id).unwrap().status, Status::Paused);
    }

    #[tokio::test]
    async fn scheduler_stop_time_pauses_and_respects_days() {
        let f = fixture(3).await;
        let later = AddRequest { start: false, scheduled: true, ..add_req(format!("{}/slow/night.iso", f.base)) };
        let later = f.mgr.add(later).await.unwrap();
        enable_schedule(&f.mgr, "02:00", "06:30", vec![5, 6]); // weekends only

        f.mgr.scheduler_tick(1, 0, 120); // Monday 02:00: not a scheduled day
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(f.mgr.find(later.id).unwrap().status, Status::Paused);

        f.mgr.scheduler_tick(2, 5, 120); // Saturday 02:00
        wait_for(&f.mgr, later.id, Status::Downloading).await;
        f.mgr.scheduler_tick(3, 5, 390); // Saturday 06:30
        let paused = wait_for(&f.mgr, later.id, Status::Paused).await;
        assert!(paused.scheduled);
    }

    fn digest_of_body(algo: Algo) -> String {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("body");
        std::fs::write(&path, body()).unwrap();
        checksum::hash_file(&path, algo).unwrap()
    }

    #[tokio::test]
    async fn checksum_is_verified_on_completion() {
        let f = fixture(3).await;
        let good = AddRequest {
            checksum: Some(digest_of_body(Algo::Sha256).to_uppercase()),
            ..add_req(format!("{}/fast/good.bin", f.base))
        };
        let bad = AddRequest { checksum: Some("0".repeat(32)), ..add_req(format!("{}/fast/bad.bin", f.base)) };
        let good = f.mgr.add(good).await.unwrap();
        let bad = f.mgr.add(bad).await.unwrap();
        wait_for(&f.mgr, good.id, Status::Completed).await;
        let failed = wait_for(&f.mgr, bad.id, Status::Error).await;
        assert!(failed.error.unwrap().contains("MD5 checksum mismatch"));
        assert_eq!(failed.auto_retries, 0, "a corrupt file is not retried");
        assert!(f.events.finished.lock().unwrap().iter().any(|i| i.id == bad.id && i.status == Status::Error));

        let err = f.mgr.add(AddRequest { checksum: Some("abc".into()), ..add_req(format!("{}/fast/x.bin", f.base)) }).await;
        assert!(err.unwrap_err().contains("MD5"));
        assert_eq!(f.mgr.checksum(good.id, Algo::Md5).await.unwrap(), digest_of_body(Algo::Md5));
    }

    #[tokio::test]
    async fn queues_have_their_own_limit() {
        let f = fixture(5).await;
        let mut s = f.mgr.settings();
        s.queues.push(Queue { name: "Night".into(), max_concurrent: 1 });
        f.mgr.set_settings(s);
        let night = |name: &str| AddRequest { queue: Some("Night".into()), ..add_req(format!("{}/slow/{name}", f.base)) };
        let a = f.mgr.add(night("a.bin")).await.unwrap();
        let b = f.mgr.add(night("b.bin")).await.unwrap();
        let main = f.mgr.add(add_req(format!("{}/slow/c.bin", f.base))).await.unwrap();
        assert_eq!(main.queue, MAIN_QUEUE);
        wait_for(&f.mgr, a.id, Status::Downloading).await;
        wait_for(&f.mgr, main.id, Status::Downloading).await;
        assert_eq!(f.mgr.find(b.id).unwrap().status, Status::Queued, "Night runs one at a time");

        f.mgr.stop_queue("Night");
        wait_for(&f.mgr, a.id, Status::Paused).await;
        wait_for(&f.mgr, b.id, Status::Paused).await;
        assert_eq!(f.mgr.find(main.id).unwrap().status, Status::Downloading, "other queues keep running");
        f.mgr.pause(main.id);
        wait_for(&f.mgr, main.id, Status::Paused).await;

        // Deleting the queue moves its downloads to Main.
        let mut s = f.mgr.settings();
        s.queues.retain(|q| q.name != "Night");
        f.mgr.set_settings(s);
        assert_eq!(f.mgr.find(a.id).unwrap().queue, MAIN_QUEUE);
        assert!(f.mgr.set_queue(&[a.id], "Night").is_err());
    }

    #[tokio::test]
    async fn higher_priority_starts_first() {
        let f = fixture(1).await;
        let first = f.mgr.add(add_req(format!("{}/slow/first.bin", f.base))).await.unwrap();
        wait_for(&f.mgr, first.id, Status::Downloading).await;
        let normal = f.mgr.add(add_req(format!("{}/slow/normal.bin", f.base))).await.unwrap();
        let low = AddRequest { priority: Priority::Low, ..add_req(format!("{}/slow/low.bin", f.base)) };
        let low = f.mgr.add(low).await.unwrap();
        let urgent = f.mgr.add(add_req(format!("{}/slow/urgent.bin", f.base))).await.unwrap();
        f.mgr.set_priority(&[urgent.id], Priority::High);

        f.mgr.pause(first.id);
        wait_for(&f.mgr, urgent.id, Status::Downloading).await;
        assert_eq!(f.mgr.find(normal.id).unwrap().status, Status::Queued);
        assert_eq!(f.mgr.find(low.id).unwrap().status, Status::Queued);
        f.mgr.pause(low.id);
        f.mgr.pause(urgent.id);
        wait_for(&f.mgr, normal.id, Status::Downloading).await;
        f.mgr.pause(normal.id);
        wait_for(&f.mgr, normal.id, Status::Paused).await;
    }

    #[tokio::test]
    async fn speed_limit_changes_while_running() {
        let f = fixture(3).await;
        let item = f.mgr.add(add_req(format!("{}/slow/limited.bin", f.base))).await.unwrap();
        wait_for(&f.mgr, item.id, Status::Downloading).await;
        f.mgr.set_speed_limit(item.id, 64);
        assert_eq!(f.mgr.find(item.id).unwrap().speed_limit_kib, 64);
        assert_eq!(f.mgr.inner.lock().unwrap().running[&item.id].limiter.rate(), 64 * 1024);
        f.mgr.pause(item.id);
        wait_for(&f.mgr, item.id, Status::Paused).await;
    }

    #[tokio::test]
    async fn idle_fires_once_after_last_completion() {
        let f = fixture(3).await;
        let idle = || f.events.idle.load(std::sync::atomic::Ordering::SeqCst);
        let a = f.mgr.add(add_req(format!("{}/fast/a.bin", f.base))).await.unwrap();
        let b = f.mgr.add(add_req(format!("{}/fast/b.bin", f.base))).await.unwrap();
        wait_for(&f.mgr, a.id, Status::Completed).await;
        wait_for(&f.mgr, b.id, Status::Completed).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(idle(), 1);

        // Stopping the last running download is not "finished".
        let c = f.mgr.add(add_req(format!("{}/slow/c.bin", f.base))).await.unwrap();
        wait_for(&f.mgr, c.id, Status::Downloading).await;
        f.mgr.pause(c.id);
        wait_for(&f.mgr, c.id, Status::Paused).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(idle(), 1);
    }

    #[tokio::test]
    async fn probe_reports_duplicates_and_free_space() {
        let f = fixture(3).await;
        let url = format!("{}/fast/dup.zip", f.base);
        assert!(f.mgr.probe(url.clone(), vec![]).await.unwrap().duplicate.is_none());
        let item = f.mgr.add(add_req(url.clone())).await.unwrap();
        let p = f.mgr.probe(url, vec![]).await.unwrap();
        assert_eq!(p.duplicate.unwrap().id, item.id);
        #[cfg(windows)]
        assert!(p.free_space.unwrap() > 0);
    }

    #[test]
    fn stream_names_avoid_generic_playlist_names() {
        assert_eq!(stream_file_name("Episode 4.m3u8", "https://tv.example/e4/Episode%204.m3u8", "mp4"), "Episode 4.mp4");
        assert_eq!(stream_file_name("index.m3u8", "https://www.tv.example/hls/index.m3u8", "ts"), "tv.example video.ts");
        assert_eq!(stream_file_name("chunklist_b800000.m3u8", "https://cdn.example/x", "mp4"), "cdn.example video.mp4");
        assert_eq!(stream_file_name("12345.m3u8", "https://cdn.example/x", "mp4"), "cdn.example video.mp4");
        assert_eq!(stream_file_name("manifest.mpd", "https://cdn.example/x", "mp4"), "cdn.example video.mp4");
    }

    #[test]
    fn host_rules_pick_connections() {
        let s = Settings {
            connections: 8,
            host_connections: vec![
                HostLimit { host: "example.com".into(), connections: 2 },
                HostLimit { host: "*.cdn.net".into(), connections: 64 },
            ],
            ..Settings::default()
        };
        assert_eq!(s.connections_for("https://example.com/a"), 2);
        assert_eq!(s.connections_for("https://dl.EXAMPLE.com/a"), 2);
        assert_eq!(s.connections_for("https://notexample.com/a"), 8);
        assert_eq!(s.connections_for("https://x.cdn.net/a"), 32, "clamped");
        assert_eq!(s.connections_for("not a url"), 8);
    }

    #[test]
    fn settings_always_keep_main_queue() {
        let mut s = Settings {
            queues: vec![
                Queue { name: " Night ".into(), max_concurrent: 1 },
                Queue { name: "night".into(), max_concurrent: 2 },
            ],
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.queues.iter().map(|q| q.name.as_str()).collect::<Vec<_>>(), ["Main", "Night"]);
    }

    #[test]
    fn logins_become_auth_headers_or_ftp_users() {
        let s = Settings {
            logins: vec![SiteLogin { host: "files.example".into(), username: "me".into(), password: "pw".into(), new_password: None }],
            ..Settings::default()
        };
        let (url, h) = s.authorize("https://dl.files.example/a.zip", &[]);
        assert_eq!(url, "https://dl.files.example/a.zip");
        assert_eq!(h, vec![("Authorization".to_string(), "Basic bWU6cHc=".to_string())]);
        let (url, _) = s.authorize("ftp://files.example/pub/a.iso", &[]);
        assert_eq!(url, "ftp://me:pw@files.example/pub/a.iso");
        let (url, _) = s.authorize("ftp://other:x@files.example/a", &[]);
        assert_eq!(url, "ftp://other:x@files.example/a", "credentials in the address win");
        assert!(s.authorize("https://elsewhere.example/a", &[]).1.is_empty());
    }

    #[test]
    fn saved_passwords_are_sealed_and_replaced_by_new_ones() {
        let mut s = Settings {
            logins: vec![SiteLogin { host: " https://files.example/ ".into(), username: "me".into(), password: "old".into(), new_password: Some("new".into()) }],
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.logins[0].host, "files.example");
        assert_eq!(s.logins[0].password, "new");
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("\"new\""), "{json}");
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.logins[0].password, "new");
    }

    #[tokio::test]
    async fn download_limit_pauses_and_resumes_with_the_window() {
        let f = fixture(3).await;
        let mut s = f.mgr.settings();
        s.quota_mb = 1;
        s.quota_hours = 1;
        f.mgr.set_settings(s);
        // 100 KiB left in this window; the next download crosses the limit early on.
        f.mgr.inner.lock().unwrap().quota_bytes = 1024 * 1024 - 100 * 1024;
        let b = f.mgr.add(add_req(format!("{}/slow/b.bin", f.base))).await.unwrap();
        let paused = wait_for(&f.mgr, b.id, Status::Paused).await;
        assert!(paused.note.unwrap().contains("limit"));
        let c = f.mgr.add(add_req(format!("{}/fast/c.bin", f.base))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(f.mgr.find(c.id).unwrap().status, Status::Queued, "nothing starts while over the limit");

        f.mgr.quota_tick(now() + 3600);
        wait_for(&f.mgr, b.id, Status::Completed).await;
        wait_for(&f.mgr, c.id, Status::Completed).await;
    }

    #[tokio::test]
    async fn refresh_takes_the_new_address_of_the_same_file() {
        let f = fixture(3).await;
        let item = f.mgr.add(AddRequest { start: false, ..add_req(format!("{}/fast/report.pdf", f.base)) }).await.unwrap();
        // Every file on the test server has the same size; make this one stand out.
        f.mgr.inner.lock().unwrap().store.items[0].size = Some(1);
        let page = f.mgr.request_refresh(item.id).unwrap();
        assert!(page.ends_with("/fast/report.pdf"));
        assert!(f.mgr.find(item.id).unwrap().awaiting_refresh);
        // A different file is not taken.
        assert!(f.mgr.try_refresh(&format!("{}/fast/other.zip", f.base), &[], Some("other.zip")).await.is_none());
        let resumed = f.mgr.try_refresh(&format!("{}/fast/new-token-link", f.base), &[], Some("report.pdf")).await.unwrap();
        assert_eq!(resumed.id, item.id);
        let done = wait_for(&f.mgr, item.id, Status::Completed).await;
        assert!(done.url.ends_with("/new-token-link"));
        assert!(!done.awaiting_refresh);
    }

    #[tokio::test]
    async fn sync_downloads_again_when_the_server_copy_changes() {
        let f = fixture(3).await;
        let item = f.mgr.add(add_req(format!("{}/fast/feed.xml", f.base))).await.unwrap();
        wait_for(&f.mgr, item.id, Status::Completed).await;
        f.mgr.set_sync(&[item.id], 1);
        for _ in 0..50 {
            if f.mgr.find(item.id).unwrap().remote_version.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Unchanged on the server: nothing happens.
        f.mgr.sync_tick(now() + 3600).await;
        assert_eq!(f.mgr.find(item.id).unwrap().status, Status::Completed);
        // Pretend the copy on disk is an older version.
        f.mgr.inner.lock().unwrap().store.items[0].remote_version = Some("old|old|1".into());
        f.mgr.sync_tick(now() + 7200).await;
        let again = f.mgr.find(item.id).unwrap();
        assert!(matches!(again.status, Status::Queued | Status::Downloading | Status::Completed));
        wait_for(&f.mgr, item.id, Status::Completed).await;
        assert_eq!(std::fs::read(&again.save_path).unwrap(), body());
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn start_and_finish_commands_run_with_placeholders() {
        let f = fixture(3).await;
        let started = f.dir.path().join("started.txt");
        let finished = f.dir.path().join("finished.txt");
        let mut s = f.mgr.settings();
        s.start_command = format!(r#"echo %n> "{}""#, started.display());
        s.finish_command = format!(r#"echo %u> "{}""#, finished.display());
        f.mgr.set_settings(s);
        let url = format!("{}/fast/hooked.bin", f.base);
        let item = f.mgr.add(add_req(url.clone())).await.unwrap();
        wait_for(&f.mgr, item.id, Status::Completed).await;
        for _ in 0..100 {
            if finished.exists() && started.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(std::fs::read_to_string(&started).unwrap().trim(), "hooked.bin");
        assert_eq!(std::fs::read_to_string(&finished).unwrap().trim(), url);
    }
}
