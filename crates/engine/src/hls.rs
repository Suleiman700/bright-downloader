//! Video streams: HLS (`.m3u8`) playlist parsing, and a downloader shared
//! with DASH (`.mpd`, see [`crate::dash`]) that fetches segments over
//! several connections, resumes after a stop or crash, decrypts AES-128 and
//! records live streams until it is stopped.
//!
//! Segments land in a folder next to the target file and are joined at the
//! end. With `ffmpeg` on the PATH the result is remuxed into the target's
//! container (and a separate audio track is merged); without it the joined
//! MPEG-TS is kept as is.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::StreamExt;
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::limiter::RateLimiter;
use crate::task::{part_path, state_path, with_suffix, Outcome, Progress};
use crate::{Options, RequestSpec};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    /// "1920x1080"
    pub resolution: Option<String>,
    /// `GROUP-ID` of the audio renditions to play with this variant.
    #[serde(skip)]
    pub audio_group: Option<String>,
}

impl Variant {
    /// "1080p · 4.5 Mbps", or just the bitrate when the size is not given.
    pub fn label(&self) -> String {
        let rate = if self.bandwidth >= 1_000_000 {
            format!("{:.1} Mbps", self.bandwidth as f64 / 1e6)
        } else {
            format!("{} kbps", self.bandwidth / 1000)
        };
        match self.resolution.as_deref().and_then(|r| r.split_once('x')).map(|(_, h)| h) {
            Some(h) => format!("{h}p · {rate}"),
            None => rate,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioRendition {
    pub group: String,
    pub url: String,
    pub default: bool,
}

/// A byte range of a resource: `(offset, length)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resource {
    pub url: String,
    pub range: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key {
    pub uri: String,
    /// Explicit IV; otherwise the segment's sequence number is used.
    pub iv: Option<[u8; 16]>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaSegment {
    pub sequence: u64,
    pub duration: f64,
    pub resource: Resource,
    pub key: Option<Key>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaPlaylist {
    pub segments: Vec<MediaSegment>,
    pub target_duration: f64,
    /// `#EXT-X-ENDLIST`: a finished video. Without it the stream is live.
    pub ended: bool,
    /// `#EXT-X-MAP`: initialization section for fMP4 streams.
    pub init: Option<Resource>,
}

impl MediaPlaylist {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Playlist {
    Master { variants: Vec<Variant>, audio: Vec<AudioRendition> },
    Media(MediaPlaylist),
}

/// `KEY=VALUE,KEY="quoted, value"` → map.
fn attributes(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else { break };
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.split_once('"') {
                Some((v, n)) => (v, n),
                None => (quoted, ""),
            }
        } else {
            after.split_once(',').map_or((after, ""), |(v, n)| (v, n))
        };
        out.insert(key.trim().to_ascii_uppercase(), value.to_string());
        rest = next.trim_start_matches(',').trim();
    }
    out
}

/// `length[@offset]`
fn byte_range(s: &str) -> Option<(u64, Option<u64>)> {
    let (len, off) = match s.trim().split_once('@') {
        Some((l, o)) => (l, Some(o.trim().parse().ok()?)),
        None => (s, None),
    };
    Some((len.trim().parse().ok()?, off))
}

fn hex_iv(s: &str) -> Option<[u8; 16]> {
    let hex = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    if hex.len() != 32 {
        return None;
    }
    let mut iv = [0u8; 16];
    for (i, byte) in iv.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(iv)
}

fn stream_err(msg: impl Into<String>) -> Error {
    Error::Stream(msg.into())
}

/// Parse a playlist; relative URIs resolve against `base` (the playlist's own URL).
pub fn parse(text: &str, base: &url::Url) -> Result<Playlist> {
    let text = text.trim_start_matches('\u{feff}');
    if !text.trim_start().starts_with("#EXTM3U") {
        return Err(stream_err("the address is not an HLS playlist"));
    }
    let resolve = |u: &str| base.join(u.trim()).map(|u| u.to_string()).map_err(|e| stream_err(format!("bad link in playlist: {e}")));

    let mut variants = Vec::new();
    let mut audio = Vec::new();
    let mut pending_variant: Option<HashMap<String, String>> = None;
    let mut segments = Vec::new();
    let mut target_duration = 0.0;
    let mut first_sequence = 0u64;
    let mut ended = false;
    let mut init = None;
    let mut duration = None;
    let mut key: Option<Key> = None;
    let mut range: Option<(u64, Option<u64>)> = None;
    let mut next_offset = 0u64;

    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(rest) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            pending_variant = Some(attributes(rest));
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = attributes(rest);
            if a.get("TYPE").map(String::as_str) == Some("AUDIO") {
                if let (Some(group), Some(uri)) = (a.get("GROUP-ID"), a.get("URI")) {
                    audio.push(AudioRendition {
                        group: group.clone(),
                        url: resolve(uri)?,
                        default: a.get("DEFAULT").map(String::as_str) == Some("YES"),
                    });
                }
            }
        } else if let Some(rest) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            target_duration = rest.trim().parse().unwrap_or(0.0);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            first_sequence = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("#EXTINF:") {
            duration = rest.split(',').next().and_then(|d| d.trim().parse().ok());
        } else if let Some(rest) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            range = byte_range(rest);
        } else if let Some(rest) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attributes(rest);
            key = match a.get("METHOD").map(String::as_str) {
                Some("NONE") | None => None,
                Some("AES-128") => {
                    let uri = a.get("URI").ok_or_else(|| stream_err("encryption key without an address"))?;
                    Some(Key { uri: resolve(uri)?, iv: a.get("IV").and_then(|iv| hex_iv(iv)) })
                }
                Some(other) => {
                    return Err(stream_err(format!(
                        "this stream is DRM-protected ({other}) and can't be downloaded"
                    )))
                }
            };
        } else if let Some(rest) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attributes(rest);
            let uri = a.get("URI").ok_or_else(|| stream_err("#EXT-X-MAP without an address"))?;
            let range = a.get("BYTERANGE").and_then(|r| byte_range(r)).map(|(len, off)| (off.unwrap_or(0), len));
            init = Some(Resource { url: resolve(uri)?, range });
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if line.starts_with('#') {
            continue;
        } else if let Some(a) = pending_variant.take() {
            variants.push(Variant {
                url: resolve(line)?,
                bandwidth: a.get("BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0),
                resolution: a.get("RESOLUTION").cloned(),
                audio_group: a.get("AUDIO").cloned(),
            });
        } else {
            let range = range.take().map(|(len, off)| {
                let off = off.unwrap_or(next_offset);
                next_offset = off + len;
                (off, len)
            });
            segments.push(MediaSegment {
                sequence: first_sequence + segments.len() as u64,
                duration: duration.take().unwrap_or(0.0),
                resource: Resource { url: resolve(line)?, range },
                key: key.clone(),
            });
        }
    }
    if !variants.is_empty() {
        Ok(Playlist::Master { variants, audio })
    } else {
        Ok(Playlist::Media(MediaPlaylist { segments, target_duration, ended, init }))
    }
}

/// The wanted variant if listed, else the best quality.
pub fn pick_variant<'a>(variants: &'a [Variant], wanted: Option<&str>) -> Option<&'a Variant> {
    wanted
        .and_then(|w| variants.iter().find(|v| v.url == w))
        .or_else(|| variants.iter().max_by_key(|v| v.bandwidth))
}

fn pick_audio<'a>(audio: &'a [AudioRendition], variant: &Variant) -> Option<&'a AudioRendition> {
    let group = variant.audio_group.as_ref()?;
    audio.iter().filter(|a| &a.group == group).max_by_key(|a| a.default)
}

async fn fetch_text(client: &Client, spec: &RequestSpec, url: &str) -> Result<(url::Url, String)> {
    let resp = spec.apply(client.get(url)).send().await?;
    if !resp.status().is_success() {
        return Err(Error::Status(resp.status().as_u16()));
    }
    let base = resp.url().clone();
    Ok((base, resp.text().await?))
}

async fn fetch_playlist(client: &Client, spec: &RequestSpec, url: &str) -> Result<Playlist> {
    let (base, text) = fetch_text(client, spec, url).await?;
    parse(&text, &base)
}

fn unix_now() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Where a stream's tracks come from. Re-read every round for live streams.
enum Source {
    /// Media playlist URLs: video, then audio if separate.
    Hls(Vec<String>),
    /// Ids of the chosen representations in the job's manifest.
    Dash(Vec<String>),
}

impl Source {
    /// Identifies the tracks in the saved state, so a changed choice restarts.
    fn ids(&self) -> Vec<String> {
        match self {
            Source::Hls(urls) => urls.clone(),
            Source::Dash(ids) => ids.iter().map(|id| format!("dash:{id}")).collect(),
        }
    }

    fn audio_ext(&self) -> &'static str {
        match self {
            Source::Hls(_) => "ts",
            Source::Dash(_) => "m4a",
        }
    }

    async fn resolve(client: &Client, spec: &RequestSpec, variant: Option<&str>) -> Result<Self> {
        let (base, text) = fetch_text(client, spec, &spec.url).await?;
        if crate::dash::looks_like_mpd(&text) {
            let mpd = crate::dash::parse(&text, &base, unix_now())?;
            return Ok(Source::Dash(mpd.pick(variant)?.iter().map(|r| r.id.clone()).collect()));
        }
        Ok(match parse(&text, &base)? {
            Playlist::Master { variants, audio } => {
                let v = pick_variant(&variants, variant).ok_or_else(|| stream_err("the playlist lists no streams"))?;
                Source::Hls(std::iter::once(v.url.clone()).chain(pick_audio(&audio, v).map(|a| a.url.clone())).collect())
            }
            Playlist::Media(_) => Source::Hls(vec![spec.url.clone()]),
        })
    }

    async fn read(&self, client: &Client, spec: &RequestSpec) -> Result<Vec<MediaPlaylist>> {
        match self {
            Source::Hls(urls) => {
                let mut out = Vec::new();
                for url in urls {
                    match fetch_playlist(client, spec, url).await? {
                        Playlist::Media(m) => out.push(m),
                        Playlist::Master { .. } => return Err(stream_err("nested master playlists are not supported")),
                    }
                }
                Ok(out)
            }
            Source::Dash(ids) => {
                let (base, text) = fetch_text(client, spec, &spec.url).await?;
                let mpd = crate::dash::parse(&text, &base, unix_now())?;
                ids.iter()
                    .map(|id| {
                        mpd.sets
                            .iter()
                            .flat_map(|s| &s.reps)
                            .find(|r| &r.id == id)
                            .map(|r| r.media.clone())
                            .ok_or_else(|| stream_err("the chosen quality is no longer offered"))
                    })
                    .collect()
            }
        }
    }
}

/// What the Add dialog shows for a stream.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamInfo {
    /// Best first; empty for a single-quality playlist.
    pub variants: Vec<VariantInfo>,
    pub live: bool,
    /// Seconds, for finished videos.
    pub duration: Option<f64>,
    /// Audio comes as its own track; merging it needs ffmpeg.
    pub separate_audio: bool,
    /// ffmpeg was found, so the stream can be saved as MP4.
    pub ffmpeg: bool,
    /// Rough size from the best variant's bitrate and the duration.
    pub estimated_size: Option<u64>,
    /// "hls" or "dash".
    pub format: &'static str,
    /// File extension the download will get: "mp4", or "ts" for HLS without ffmpeg.
    pub container: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VariantInfo {
    pub url: String,
    pub label: String,
    pub bandwidth: u64,
}

/// Describe a DASH manifest for the Add dialog.
fn inspect_dash(text: &str, base: &url::Url) -> Result<StreamInfo> {
    let mpd = crate::dash::parse(text, base, unix_now())?;
    let picked = mpd.pick(None)?;
    let videos = mpd.video();
    let mut variants: Vec<VariantInfo> =
        videos.iter().map(|r| VariantInfo { url: r.id.clone(), label: r.label(), bandwidth: r.bandwidth }).collect();
    if variants.len() == 1 {
        variants.clear();
    }
    let bandwidth: u64 = picked.iter().map(|r| r.bandwidth).sum();
    let duration = mpd.duration.filter(|_| !mpd.dynamic);
    Ok(StreamInfo {
        variants,
        live: mpd.dynamic,
        duration,
        separate_audio: picked.len() > 1,
        ffmpeg: find_ffmpeg().is_some(),
        estimated_size: duration.filter(|_| bandwidth > 0).map(|d| (d * bandwidth as f64 / 8.0) as u64),
        format: "dash",
        // Joined fragmented MP4 plays as is, even without ffmpeg.
        container: "mp4",
    })
}

/// Read a playlist or manifest (and, for a master playlist, its best variant).
pub async fn inspect(client: &Client, spec: &RequestSpec) -> Result<StreamInfo> {
    let (base, text) = fetch_text(client, spec, &spec.url).await?;
    if crate::dash::looks_like_mpd(&text) {
        return inspect_dash(&text, &base);
    }
    let (mut variants, separate_audio, media, bandwidth) = match parse(&text, &base)? {
        Playlist::Master { variants, audio } => {
            let best = pick_variant(&variants, None).ok_or_else(|| stream_err("the playlist lists no streams"))?.clone();
            let media = match fetch_playlist(client, spec, &best.url).await? {
                Playlist::Media(m) => m,
                Playlist::Master { .. } => return Err(stream_err("nested master playlists are not supported")),
            };
            let separate = pick_audio(&audio, &best).is_some();
            let infos = variants
                .iter()
                .map(|v| VariantInfo { url: v.url.clone(), label: v.label(), bandwidth: v.bandwidth })
                .collect::<Vec<_>>();
            (infos, separate, media, best.bandwidth)
        }
        Playlist::Media(m) => (Vec::new(), false, m, 0),
    };
    variants.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
    variants.dedup_by(|a, b| a.url == b.url);
    if variants.len() == 1 {
        variants.clear();
    }
    let ffmpeg = find_ffmpeg().is_some();
    let duration = media.ended.then(|| media.duration());
    Ok(StreamInfo {
        variants,
        live: !media.ended,
        duration,
        separate_audio,
        ffmpeg,
        estimated_size: duration.filter(|_| bandwidth > 0).map(|d| (d * bandwidth as f64 / 8.0) as u64),
        format: "hls",
        container: if ffmpeg { "mp4" } else { "ts" },
    })
}

static TOOLS_DIR: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// A folder searched first for helper programs (ffmpeg), e.g. the app's
/// own tools folder where it installs them.
pub fn set_tools_dir(dir: PathBuf) {
    *TOOLS_DIR.write().unwrap() = Some(dir);
}

/// `name` (plus `.exe` on Windows) in the tools folder, next to the app, or
/// on the PATH.
pub fn find_tool(name: &str) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let tools = TOOLS_DIR.read().unwrap().clone();
    let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.to_path_buf()));
    let path_dirs = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect::<Vec<_>>()).unwrap_or_default();
    tools.into_iter().chain(beside).chain(path_dirs).map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// `ffmpeg` from `BRIGHTDL_FFMPEG`, else via [`find_tool`].
pub fn find_ffmpeg() -> Option<PathBuf> {
    std::env::var_os("BRIGHTDL_FFMPEG").map(PathBuf::from).filter(|p| p.is_file()).or_else(|| find_tool("ffmpeg"))
}

/// Folder holding a stream's downloaded segments until they are joined.
pub fn segments_dir(save_path: &Path) -> PathBuf {
    with_suffix(save_path, ".obpart.d")
}

fn audio_part(save_path: &Path) -> PathBuf {
    with_suffix(save_path, ".audio.obpart")
}

/// Every temporary path a stream download may leave behind (besides the
/// regular part / state files).
pub fn stream_temp_paths(save_path: &Path) -> [PathBuf; 2] {
    [segments_dir(save_path), audio_part(save_path)]
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct TrackState {
    playlist: String,
    /// Sequence number → decrypted size of the segment on disk.
    done: BTreeMap<u64, u64>,
    init_done: bool,
}

#[derive(Serialize, Deserialize, Default)]
struct StreamState {
    version: u32,
    source: String,
    tracks: Vec<TrackState>,
}

const STREAM_STATE_VERSION: u32 = 1;
const TRACK_NAMES: [&str; 2] = ["video", "audio"];

struct Shared {
    client: Client,
    opts: Options,
    spec: RequestSpec,
    dir: PathBuf,
    limiters: Vec<Arc<RateLimiter>>,
    state: Mutex<StreamState>,
    keys: Mutex<HashMap<String, [u8; 16]>>,
    received: AtomicU64,
    active: AtomicUsize,
    stop: CancellationToken,
}

impl Shared {
    fn segment_file(&self, track: usize, sequence: u64) -> PathBuf {
        self.dir.join(format!("{}-{sequence}.seg", TRACK_NAMES[track]))
    }

    fn init_file(&self, track: usize) -> PathBuf {
        self.dir.join(format!("{}-init.seg", TRACK_NAMES[track]))
    }

    async fn key(&self, uri: &str) -> Result<[u8; 16]> {
        if let Some(k) = self.keys.lock().unwrap().get(uri) {
            return Ok(*k);
        }
        let resp = self.spec.apply(self.client.get(uri)).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Status(resp.status().as_u16()));
        }
        let bytes = resp.bytes().await?;
        let key: [u8; 16] = bytes.as_ref().try_into().map_err(|_| stream_err("the stream's encryption key is not 16 bytes"))?;
        self.keys.lock().unwrap().insert(uri.to_string(), key);
        Ok(key)
    }

    async fn persist(&self, path: &Path) -> Result<()> {
        let bytes = serde_json::to_vec(&*self.state.lock().unwrap())?;
        let tmp = with_suffix(path, ".tmp");
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

enum Job {
    Init(usize, Resource),
    Segment(usize, MediaSegment),
}

enum Fetched {
    Done(Vec<u8>),
    Stopped,
}

/// Download one resource completely (range-aware, rate-limited, cancellable).
async fn fetch(sh: &Shared, res: &Resource) -> Result<Fetched> {
    let mut req = sh.spec.apply(sh.client.get(&res.url));
    if let Some((off, len)) = res.range {
        req = req.header(header::RANGE, format!("bytes={off}-{}", off + len.max(1) - 1));
    }
    let resp = tokio::select! {
        _ = sh.stop.cancelled() => return Ok(Fetched::Stopped),
        r = req.send() => r?,
    };
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Status(status.as_u16()));
    }
    let mut data = Vec::new();
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::select! {
            _ = sh.stop.cancelled() => return Ok(Fetched::Stopped),
            n = tokio::time::timeout(sh.opts.read_timeout, stream.next()) => n.map_err(|_| Error::Timeout)?,
        };
        let Some(chunk) = next else { break };
        let chunk = chunk?;
        for limiter in &sh.limiters {
            limiter.acquire(chunk.len() as u64).await;
        }
        sh.received.fetch_add(chunk.len() as u64, Ordering::Relaxed);
        data.extend_from_slice(&chunk);
    }
    // A server that ignores the Range header sends the whole resource.
    if let (Some((off, len)), StatusCode::OK) = (res.range, status) {
        let (off, len) = (off as usize, len as usize);
        if data.len() < off + len {
            return Err(Error::Truncated);
        }
        data = data[off..off + len].to_vec();
    }
    Ok(Fetched::Done(data))
}

async fn run_job(sh: &Shared, job: &Job) -> Result<bool> {
    let (track, res, key, file) = match job {
        Job::Init(t, res) => (*t, res, None, sh.init_file(*t)),
        Job::Segment(t, seg) => (*t, &seg.resource, seg.key.as_ref().map(|k| (k, seg.sequence)), sh.segment_file(*t, seg.sequence)),
    };
    let Fetched::Done(mut data) = fetch(sh, res).await? else { return Ok(false) };
    if let Some((key, sequence)) = key {
        let secret = sh.key(&key.uri).await?;
        let iv = key.iv.unwrap_or_else(|| (sequence as u128).to_be_bytes());
        data = cbc::Decryptor::<aes::Aes128>::new(&secret.into(), &iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(&data)
            .map_err(|_| stream_err("could not decrypt a segment (wrong key?)"))?;
    }
    let tmp = with_suffix(&file, ".tmp");
    tokio::fs::write(&tmp, &data).await?;
    tokio::fs::rename(&tmp, &file).await?;
    let mut state = sh.state.lock().unwrap();
    match job {
        Job::Init(..) => state.tracks[track].init_done = true,
        Job::Segment(_, seg) => {
            state.tracks[track].done.insert(seg.sequence, data.len() as u64);
        }
    }
    Ok(true)
}

async fn worker(sh: Arc<Shared>, queue: Arc<Mutex<VecDeque<Job>>>) -> Result<()> {
    loop {
        let Some(job) = queue.lock().unwrap().pop_front() else { return Ok(()) };
        let mut failures = 0u32;
        loop {
            if sh.stop.is_cancelled() {
                return Ok(());
            }
            sh.active.fetch_add(1, Ordering::Relaxed);
            let result = run_job(&sh, &job).await;
            sh.active.fetch_sub(1, Ordering::Relaxed);
            match result {
                Ok(true) => break,
                Ok(false) => return Ok(()),
                Err(e) => {
                    failures += 1;
                    if !e.is_retryable() || failures > sh.opts.max_retries {
                        return Err(e);
                    }
                    let backoff = sh.opts.retry_base_delay * 2u32.saturating_pow(failures - 1);
                    tokio::select! {
                        _ = sh.stop.cancelled() => return Ok(()),
                        _ = tokio::time::sleep(backoff.min(Duration::from_secs(30))) => {}
                    }
                }
            }
        }
    }
}

fn snapshot(sh: &Shared, known: &[usize], all_ended: bool, speed: u64) -> Progress {
    let state = sh.state.lock().unwrap();
    let downloaded: u64 = state.tracks.iter().flat_map(|t| t.done.values()).sum();
    let done: usize = state.tracks.iter().map(|t| t.done.len()).sum();
    let count: usize = known.iter().sum();
    // Segments are about the same size, so extrapolate from the finished ones.
    let total = (all_ended && done > 0 && count > 0).then(|| (downloaded as f64 * count as f64 / done as f64) as u64);
    Progress {
        total: total.map(|t| t.max(downloaded)),
        downloaded,
        speed,
        connections: sh.active.load(Ordering::Relaxed),
        accepts_ranges: true,
        segments: Vec::new(),
    }
}

/// Join a track's segments (after its init section) into `out`. Blocking.
fn join_track(sh: &Shared, track: usize, out: &Path) -> std::io::Result<u64> {
    let mut file = std::fs::File::create(out)?;
    let state = sh.state.lock().unwrap();
    let t = &state.tracks[track];
    let mut parts: Vec<PathBuf> = Vec::new();
    if t.init_done {
        parts.push(sh.init_file(track));
    }
    parts.extend(t.done.keys().map(|seq| sh.segment_file(track, *seq)));
    drop(state);
    let mut total = 0;
    for part in parts {
        total += std::io::copy(&mut std::fs::File::open(part)?, &mut file)?;
    }
    file.sync_all()?;
    Ok(total)
}

fn ffmpeg_mux(ffmpeg: &Path, video: &Path, audio: Option<&Path>, out: &Path) -> Result<()> {
    let mut cmd = std::process::Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-loglevel", "error", "-y", "-i"]).arg(video);
    if let Some(a) = audio {
        cmd.arg("-i").arg(a).args(["-map", "0:v?", "-map", "1:a?"]);
    }
    cmd.args(["-c", "copy"]).arg(out);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output()?;
    if out.status.success() {
        Ok(())
    } else {
        let msg = String::from_utf8_lossy(&out.stderr);
        Err(stream_err(format!("ffmpeg could not create the video: {}", msg.lines().last().unwrap_or("unknown error"))))
    }
}

/// Join the downloaded segments into `save_path`.
async fn finalize(sh: Arc<Shared>, save_path: PathBuf, tracks: usize, audio_ext: &'static str) -> Result<u64> {
    tokio::task::spawn_blocking(move || -> Result<u64> {
        let video = part_path(&save_path);
        join_track(&sh, 0, &video)?;
        let audio = (tracks > 1).then(|| audio_part(&save_path));
        if let Some(a) = &audio {
            join_track(&sh, 1, a)?;
        }
        let is_ts = save_path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ts"));
        match find_ffmpeg() {
            Some(ff) if audio.is_some() || !is_ts => {
                ffmpeg_mux(&ff, &video, audio.as_deref(), &save_path)?;
                std::fs::remove_file(&video)?;
                if let Some(a) = &audio {
                    std::fs::remove_file(a)?;
                }
            }
            _ => {
                std::fs::rename(&video, &save_path)?;
                if let Some(a) = &audio {
                    // No ffmpeg to merge: keep the audio as its own file beside the video.
                    let stem = save_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    std::fs::rename(a, save_path.with_file_name(format!("{stem} (audio).{audio_ext}")))?;
                }
            }
        }
        Ok(std::fs::metadata(&save_path)?.len())
    })
    .await
    .map_err(|e| Error::AllWorkersFailed(e.to_string()))?
}

/// Download (or record, for a live stream) the playlist at `spec.url` into
/// `save_path`. `variant` picks the quality from a master playlist.
///
/// Cancelling a finished video pauses it (state is kept for resume);
/// cancelling a live recording ends it and saves what was recorded.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn download<F>(
    client: &Client,
    opts: &Options,
    limiters: Vec<Arc<RateLimiter>>,
    spec: &RequestSpec,
    save_path: &Path,
    connections: usize,
    variant: Option<&str>,
    cancel: CancellationToken,
    on_progress: F,
) -> Result<Outcome>
where
    F: Fn(&Progress) + Send + Sync,
{
    let source = tokio::select! {
        _ = cancel.cancelled() => return Ok(Outcome::Paused),
        s = Source::resolve(client, spec, variant) => s?,
    };
    let playlists = source.ids();

    if let Some(parent) = save_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let dir = segments_dir(save_path);
    let state_file = state_path(save_path);
    let saved: Option<StreamState> = tokio::fs::read(&state_file).await.ok().and_then(|b| serde_json::from_slice(&b).ok());
    let state = match saved {
        Some(s)
            if s.version == STREAM_STATE_VERSION
                && s.source == spec.url
                && s.tracks.iter().map(|t| &t.playlist).eq(playlists.iter())
                && dir.is_dir() =>
        {
            s
        }
        _ => {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            StreamState {
                version: STREAM_STATE_VERSION,
                source: spec.url.clone(),
                tracks: playlists.iter().map(|p| TrackState { playlist: p.clone(), ..TrackState::default() }).collect(),
            }
        }
    };
    tokio::fs::create_dir_all(&dir).await?;

    let sh = Arc::new(Shared {
        client: client.clone(),
        opts: opts.clone(),
        spec: spec.clone(),
        dir: dir.clone(),
        limiters,
        state: Mutex::new(state),
        keys: Mutex::new(HashMap::new()),
        received: AtomicU64::new(0),
        active: AtomicUsize::new(0),
        stop: cancel.child_token(),
    });

    // Every segment seen per track (a live playlist slides; old entries drop out).
    let mut known: Vec<BTreeMap<u64, MediaSegment>> = vec![BTreeMap::new(); playlists.len()];
    let mut ended = vec![false; playlists.len()];
    let mut target = 6.0f64;
    let mut last_received = 0u64;
    let mut speed = 0f64;
    let mut ticks = 0u64;

    loop {
        // (Re)read the media playlists and queue whatever is not on disk yet.
        let queue = Arc::new(Mutex::new(VecDeque::new()));
        let medias = tokio::select! {
            _ = sh.stop.cancelled() => Vec::new(),
            m = source.read(client, spec) => m?,
        };
        for (t, media) in medias.into_iter().enumerate() {
            ended[t] = media.ended;
            if media.target_duration > 0.0 {
                target = media.target_duration;
            }
            let state = sh.state.lock().unwrap();
            if let Some(init) = media.init.filter(|_| !state.tracks[t].init_done) {
                queue.lock().unwrap().push_back(Job::Init(t, init));
            }
            for seg in media.segments {
                if !known[t].contains_key(&seg.sequence) {
                    if !state.tracks[t].done.contains_key(&seg.sequence) {
                        queue.lock().unwrap().push_back(Job::Segment(t, seg.clone()));
                    }
                    known[t].insert(seg.sequence, seg);
                }
            }
        }
        let all_ended = ended.iter().all(|e| *e);
        let counts: Vec<usize> = known.iter().map(BTreeMap::len).collect();

        let workers = connections.clamp(1, 16).min(queue.lock().unwrap().len());
        let mut set = JoinSet::new();
        for _ in 0..workers {
            set.spawn(worker(sh.clone(), queue.clone()));
        }
        let mut first_error = None;
        let mut ticker = tokio::time::interval(opts.progress_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                joined = set.join_next() => match joined {
                    None => break,
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(e))) => {
                        sh.stop.cancel();
                        first_error.get_or_insert(e);
                    }
                    Some(Err(e)) => {
                        sh.stop.cancel();
                        first_error.get_or_insert(Error::AllWorkersFailed(e.to_string()));
                    }
                },
                _ = ticker.tick() => {
                    let received = sh.received.load(Ordering::Relaxed);
                    let instant = (received - last_received) as f64 / opts.progress_interval.as_secs_f64();
                    last_received = received;
                    speed = if ticks == 0 { instant } else { speed * 0.7 + instant * 0.3 };
                    ticks += 1;
                    on_progress(&snapshot(&sh, &counts, all_ended, speed as u64));
                    if ticks % 4 == 0 {
                        sh.persist(&state_file).await?;
                    }
                }
            }
        }
        sh.persist(&state_file).await?;
        on_progress(&snapshot(&sh, &counts, all_ended, 0));

        if let Some(e) = first_error {
            return Err(e);
        }
        let recorded = sh.state.lock().unwrap().tracks[0].done.len();
        if cancel.is_cancelled() {
            // Stopping a live recording keeps what was recorded.
            if !all_ended && recorded > 0 {
                break;
            }
            return Ok(Outcome::Paused);
        }
        if all_ended {
            let complete = {
                let state = sh.state.lock().unwrap();
                known.iter().zip(&state.tracks).all(|(k, t)| k.keys().all(|s| t.done.contains_key(s)))
            };
            if complete {
                break;
            }
            return Err(Error::AllWorkersFailed("some segments could not be downloaded".into()));
        }
        // Live: wait for the playlist to grow.
        let wait = Duration::from_secs_f64((target / 2.0).clamp(1.0, 10.0));
        tokio::select! {
            _ = cancel.cancelled() => {
                if recorded > 0 {
                    break;
                }
                return Ok(Outcome::Paused);
            }
            _ = tokio::time::sleep(wait) => {}
        }
    }

    let size = finalize(sh.clone(), save_path.to_path_buf(), playlists.len(), source.audio_ext()).await?;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    let _ = tokio::fs::remove_file(&state_file).await;
    Ok(Outcome::Completed { path: save_path.to_path_buf(), size })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> url::Url {
        url::Url::parse("https://cdn.example/vod/master.m3u8?token=1").unwrap()
    }

    #[test]
    fn parses_master_with_audio_groups() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English, US\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud\"\n\
            low/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,AUDIO=\"aud\"\n\
            https://other.example/hi.m3u8\n";
        let Playlist::Master { variants, audio } = parse(text, &base()).unwrap() else { panic!("not master") };
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].url, "https://cdn.example/vod/low/index.m3u8");
        assert_eq!(variants[0].label(), "360p · 800 kbps");
        assert_eq!(variants[1].label(), "1080p · 5.0 Mbps");
        assert_eq!(pick_variant(&variants, None).unwrap().bandwidth, 5_000_000);
        assert_eq!(pick_variant(&variants, Some(&variants[0].url)).unwrap().bandwidth, 800_000);
        assert_eq!(audio[0].url, "https://cdn.example/vod/audio/en.m3u8");
        assert_eq!(pick_audio(&audio, &variants[1]).unwrap().group, "aud");
    }

    #[test]
    fn parses_media_with_keys_ranges_and_map() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:7\n\
            #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\",IV=0x000102030405060708090a0b0c0d0e0f\n\
            #EXTINF:4.0,\n#EXT-X-BYTERANGE:1000@720\nmain.mp4\n\
            #EXT-X-KEY:METHOD=NONE\n#EXTINF:3.5,\n#EXT-X-BYTERANGE:500\nmain.mp4\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(text, &base()).unwrap() else { panic!("not media") };
        assert!(m.ended);
        assert_eq!(m.target_duration, 4.0);
        assert_eq!(m.duration(), 7.5);
        assert_eq!(m.init.unwrap().range, Some((0, 720)));
        assert_eq!(m.segments[0].sequence, 7);
        assert_eq!(m.segments[0].resource.range, Some((720, 1000)));
        assert_eq!(m.segments[1].resource.range, Some((1720, 500)), "offset continues after the previous range");
        assert_eq!(m.segments[0].key.as_ref().unwrap().iv.unwrap()[15], 15);
        assert!(m.segments[1].key.is_none());
    }

    #[test]
    fn rejects_drm_and_non_playlists() {
        let drm = "#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:4,\na.ts\n";
        assert!(parse(drm, &base()).unwrap_err().to_string().contains("DRM"));
        assert!(parse("<html>", &base()).unwrap_err().to_string().contains("not an HLS playlist"));
    }
}
