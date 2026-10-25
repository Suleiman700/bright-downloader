//! Running one download: workers, progress reporting, persistence, finalize.

use std::ffi::OsString;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, Result};
use crate::limiter::RateLimiter;
use crate::probe::{probe, ProbeInfo};
use crate::segment::{Segment, SegmentTable};
use crate::{build_client, Options, RequestSpec};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Job {
    pub spec: RequestSpec,
    /// Final location of the file. While downloading, data lives in
    /// [`part_path`] and state in [`state_path`] next to it.
    pub save_path: PathBuf,
    pub connections: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Completed { path: PathBuf, size: u64 },
    Paused,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct SegmentView {
    pub start: u64,
    pub end: Option<u64>,
    pub written: u64,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Progress {
    pub total: Option<u64>,
    pub downloaded: u64,
    /// Smoothed bytes per second.
    pub speed: u64,
    pub connections: usize,
    pub accepts_ranges: bool,
    pub segments: Vec<SegmentView>,
}

#[derive(Serialize, Deserialize)]
struct PersistedState {
    version: u32,
    probe: ProbeInfo,
    segments: Vec<Segment>,
}

const STATE_VERSION: u32 = 1;

pub(crate) fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s: OsString = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn part_path(save_path: &Path) -> PathBuf {
    with_suffix(save_path, ".obpart")
}

pub fn state_path(save_path: &Path) -> PathBuf {
    with_suffix(save_path, ".obpart.json")
}

/// Suffixes used by early builds; still recognised so unfinished downloads
/// from those builds resume instead of restarting.
const LEGACY_PART: &str = ".idmcpart";
const LEGACY_STATE: &str = ".idmcpart.json";

/// Every temporary file a download may have, current and legacy names.
pub fn temp_paths(save_path: &Path) -> [PathBuf; 4] {
    [
        part_path(save_path),
        state_path(save_path),
        with_suffix(save_path, LEGACY_PART),
        with_suffix(save_path, LEGACY_STATE),
    ]
}

/// Rename legacy temp files to the current names (only if no current ones exist).
async fn migrate_legacy(save_path: &Path) {
    let (part, state) = (part_path(save_path), state_path(save_path));
    let (old_part, old_state) = (with_suffix(save_path, LEGACY_PART), with_suffix(save_path, LEGACY_STATE));
    if tokio::fs::metadata(&part).await.is_err() && tokio::fs::metadata(&old_part).await.is_ok() {
        let _ = tokio::fs::rename(&old_part, &part).await;
        if tokio::fs::metadata(&state).await.is_err() {
            let _ = tokio::fs::rename(&old_state, &state).await;
        }
    }
}

pub struct Engine {
    client: Client,
    opts: Options,
    global_limiter: Arc<RateLimiter>,
}

struct Ctx {
    client: Client,
    opts: Options,
    spec: RequestSpec,
    probe: ProbeInfo,
    part_path: PathBuf,
    table: Mutex<SegmentTable>,
    limiters: Vec<Arc<RateLimiter>>,
    /// Stops workers: user cancel or a fatal error in one connection.
    stop: CancellationToken,
    received: AtomicU64,
}

enum SegmentEnd {
    Done,
    Stopped,
}

impl Engine {
    pub fn new(opts: Options, global_limiter: Arc<RateLimiter>) -> Result<Self> {
        Ok(Self { client: build_client(&opts)?, opts, global_limiter })
    }

    pub fn options(&self) -> &Options {
        &self.opts
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Run `job` until it completes, is cancelled (→ [`Outcome::Paused`], state
    /// kept for resume) or fails (state also kept, so it can be retried).
    pub async fn download<F>(
        &self,
        job: &Job,
        limiter: Option<Arc<RateLimiter>>,
        cancel: CancellationToken,
        on_progress: F,
    ) -> Result<Outcome>
    where
        F: Fn(&Progress) + Send + Sync,
    {
        let probe_info = tokio::select! {
            _ = cancel.cancelled() => return Ok(Outcome::Paused),
            p = probe(&self.client, &job.spec) => p?,
        };
        let part = part_path(&job.save_path);
        let state = state_path(&job.save_path);
        if let Some(parent) = job.save_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        migrate_legacy(&job.save_path).await;

        let table = match self.load_state(&state, &part, &probe_info).await {
            Some(t) => t,
            None => {
                if let Some(needed) = probe_info.total_size {
                    // A leftover part file from an unusable state is replaced, so its space counts as free.
                    let reclaim = tokio::fs::metadata(&part).await.map_or(0, |m| m.len());
                    if let Some(available) = crate::disk::available_space(&part) {
                        if needed > available + reclaim {
                            return Err(Error::DiskFull { needed, available });
                        }
                    }
                }
                let f = tokio::fs::File::create(&part).await?;
                if let Some(total) = probe_info.total_size {
                    f.set_len(total).await?;
                }
                match (probe_info.total_size, probe_info.accepts_ranges) {
                    (Some(total), true) => SegmentTable::sized(total, job.connections, self.opts.min_split),
                    (Some(total), false) => SegmentTable::sized(total, 1, u64::MAX),
                    (None, _) => SegmentTable::open_ended(),
                }
            }
        };

        let workers = if probe_info.accepts_ranges && probe_info.total_size.is_some() {
            job.connections.max(1)
        } else {
            1
        };

        let mut limiters = vec![self.global_limiter.clone()];
        limiters.extend(limiter);
        let ctx = Arc::new(Ctx {
            client: self.client.clone(),
            opts: self.opts.clone(),
            spec: RequestSpec { url: probe_info.final_url.clone(), headers: job.spec.headers.clone() },
            probe: probe_info,
            part_path: part.clone(),
            table: Mutex::new(table),
            limiters,
            stop: cancel.child_token(),
            received: AtomicU64::new(0),
        });

        let mut set = JoinSet::new();
        for wid in 0..workers {
            set.spawn(worker(wid, ctx.clone()));
        }

        let mut first_error: Option<Error> = None;
        let mut ticker = tokio::time::interval(self.opts.progress_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_received = 0u64;
        let mut speed = 0f64;
        let mut ticks = 0u64;
        loop {
            tokio::select! {
                joined = set.join_next() => match joined {
                    None => break,
                    Some(Ok(Ok(()))) => {}
                    Some(Ok(Err(e))) => {
                        if !e.is_retryable() {
                            ctx.stop.cancel();
                        }
                        first_error.get_or_insert(e);
                    }
                    Some(Err(join_err)) => {
                        ctx.stop.cancel();
                        first_error.get_or_insert(Error::AllWorkersFailed(join_err.to_string()));
                    }
                },
                _ = ticker.tick() => {
                    let received = ctx.received.load(Ordering::Relaxed);
                    let secs = self.opts.progress_interval.as_secs_f64();
                    let instant = (received - last_received) as f64 / secs;
                    last_received = received;
                    speed = if ticks == 0 { instant } else { speed * 0.7 + instant * 0.3 };
                    ticks += 1;
                    on_progress(&snapshot(&ctx, speed as u64));
                    if ticks % 4 == 0 {
                        persist(&ctx, &state).await?;
                    }
                }
            }
        }

        on_progress(&snapshot(&ctx, 0));
        let complete = ctx.table.lock().unwrap().is_complete();
        if complete {
            let size = ctx.table.lock().unwrap().total_written();
            let f = OpenOptions::new().write(true).open(&part).await?;
            f.set_len(size).await?;
            f.sync_all().await?;
            drop(f);
            tokio::fs::rename(&part, &job.save_path).await?;
            let _ = tokio::fs::remove_file(&state).await;
            return Ok(Outcome::Completed { path: job.save_path.clone(), size });
        }

        persist(&ctx, &state).await?;
        if cancel.is_cancelled() {
            return Ok(Outcome::Paused);
        }
        Err(first_error.unwrap_or_else(|| Error::AllWorkersFailed("download incomplete".into())))
    }

    /// Download an HLS stream (see [`crate::hls`]). `job.save_path` decides
    /// the container: `.ts` keeps the joined segments, anything else is
    /// remuxed with ffmpeg when it is available.
    pub async fn download_stream<F>(
        &self,
        job: &Job,
        variant: Option<&str>,
        limiter: Option<Arc<RateLimiter>>,
        cancel: CancellationToken,
        on_progress: F,
    ) -> Result<Outcome>
    where
        F: Fn(&Progress) + Send + Sync,
    {
        let mut limiters = vec![self.global_limiter.clone()];
        limiters.extend(limiter);
        crate::hls::download(
            &self.client,
            &self.opts,
            limiters,
            &job.spec,
            &job.save_path,
            job.connections,
            variant,
            cancel,
            on_progress,
        )
        .await
    }

    /// Reuse saved segments only if the remote file is provably the same.
    async fn load_state(&self, state: &Path, part: &Path, current: &ProbeInfo) -> Option<SegmentTable> {
        if !current.accepts_ranges || tokio::fs::metadata(part).await.is_err() {
            return None;
        }
        let bytes = tokio::fs::read(state).await.ok()?;
        let saved: PersistedState = serde_json::from_slice(&bytes).ok()?;
        let same = saved.version == STATE_VERSION
            && saved.probe.total_size == current.total_size
            && validator_matches(&saved.probe.etag, &current.etag)
            && validator_matches(&saved.probe.last_modified, &current.last_modified);
        same.then(|| SegmentTable::restore(saved.segments, self.opts.min_split))
    }
}

fn validator_matches(a: &Option<String>, b: &Option<String>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

fn snapshot(ctx: &Ctx, speed: u64) -> Progress {
    let table = ctx.table.lock().unwrap();
    Progress {
        total: ctx.probe.total_size,
        downloaded: table.total_written(),
        speed,
        connections: table.active_owners(),
        accepts_ranges: ctx.probe.accepts_ranges,
        segments: table
            .segments()
            .iter()
            .map(|s| SegmentView { start: s.start, end: s.end, written: s.written, active: s.owner.is_some() })
            .collect(),
    }
}

/// Atomic write: a crash mid-save never leaves a corrupt state file.
async fn persist(ctx: &Ctx, state: &Path) -> Result<()> {
    let saved = PersistedState {
        version: STATE_VERSION,
        probe: ctx.probe.clone(),
        segments: ctx.table.lock().unwrap().segments().to_vec(),
    };
    let tmp = with_suffix(state, ".tmp");
    tokio::fs::write(&tmp, serde_json::to_vec_pretty(&saved)?).await?;
    tokio::fs::rename(&tmp, state).await?;
    Ok(())
}

async fn worker(wid: usize, ctx: Arc<Ctx>) -> Result<()> {
    let mut file = OpenOptions::new().write(true).open(&ctx.part_path).await?;
    let mut failures = 0u32;
    loop {
        if ctx.stop.is_cancelled() {
            return Ok(());
        }
        let Some(seg) = ctx.table.lock().unwrap().claim(wid) else { return Ok(()) };
        let before = ctx.table.lock().unwrap().get(seg).map_or(0, |s| s.written);

        match fetch_segment(&ctx, &mut file, seg).await {
            Ok(SegmentEnd::Done) => failures = 0,
            Ok(SegmentEnd::Stopped) => {
                ctx.table.lock().unwrap().release(seg);
                return Ok(());
            }
            Err(e) => {
                let progressed = {
                    let mut table = ctx.table.lock().unwrap();
                    let progressed = table.get(seg).map_or(0, |s| s.written) > before;
                    table.release(seg);
                    progressed
                };
                if progressed {
                    failures = 0;
                }
                // Over the host's connection cap: drop this extra worker and
                // let worker 0 pick up the released segment.
                if wid > 0 && e.is_connection_limit() {
                    tracing::debug!(worker = wid, error = %e, "connection limit, worker exiting");
                    return Ok(());
                }
                failures += 1;
                if !e.is_retryable() || failures > ctx.opts.max_retries {
                    return Err(e);
                }
                tracing::debug!(worker = wid, attempt = failures, error = %e, "retrying segment");
                let backoff = ctx.opts.retry_base_delay * 2u32.saturating_pow(failures - 1);
                tokio::select! {
                    _ = ctx.stop.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(backoff.min(Duration::from_secs(30))) => {}
                }
            }
        }
    }
}

/// The file's bytes from `cursor` on: an open-ended HTTP range request, or
/// an FTP `REST` + `RETR`.
async fn open_stream(ctx: &Ctx, cursor: u64) -> Result<crate::ftp::ByteStream> {
    if crate::is_ftp(&ctx.spec.url) {
        return crate::ftp::open_at(&ctx.spec.url, cursor, ctx.opts.connect_timeout).await;
    }
    let mut req = ctx.spec.apply(ctx.client.get(&ctx.spec.url));
    if ctx.probe.accepts_ranges {
        // Open-ended so a later split only shortens how far we read.
        req = req.header(header::RANGE, format!("bytes={cursor}-"));
        if let Some(validator) = ctx.probe.etag.as_ref().or(ctx.probe.last_modified.as_ref()) {
            req = req.header(header::IF_RANGE, validator);
        }
    }
    let resp = req.send().await?;
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {}
        StatusCode::OK if cursor == 0 => {}
        s if s.is_success() => return Err(Error::RangeNotHonored),
        s => return Err(Error::Status(s.as_u16())),
    }
    Ok(Box::pin(resp.bytes_stream().map(|r| r.map_err(Error::from))))
}

async fn fetch_segment(ctx: &Ctx, file: &mut tokio::fs::File, seg: usize) -> Result<SegmentEnd> {
    let cursor = {
        let mut table = ctx.table.lock().unwrap();
        if !ctx.probe.accepts_ranges {
            // Without range support the only way to continue is from scratch.
            table.reset(seg);
        }
        let Some(s) = table.get(seg) else { return Ok(SegmentEnd::Done) };
        if s.is_done() {
            return Ok(SegmentEnd::Done);
        }
        s.cursor()
    };

    let mut stream = tokio::select! {
        _ = ctx.stop.cancelled() => return Ok(SegmentEnd::Stopped),
        s = open_stream(ctx, cursor) => s?,
    };

    file.seek(SeekFrom::Start(cursor)).await?;
    loop {
        let next = tokio::select! {
            _ = ctx.stop.cancelled() => return Ok(SegmentEnd::Stopped),
            n = tokio::time::timeout(ctx.opts.read_timeout, stream.next()) => n.map_err(|_| Error::Timeout)?,
        };
        let Some(chunk) = next else {
            let mut table = ctx.table.lock().unwrap();
            table.close_open_ended(seg);
            return match table.get(seg).and_then(|s| s.remaining()) {
                Some(0) => Ok(SegmentEnd::Done),
                _ => Err(Error::Truncated),
            };
        };
        let chunk = chunk?;
        let (offset, allowed) = ctx.table.lock().unwrap().reserve(seg, chunk.len() as u64);
        if allowed == 0 {
            return Ok(SegmentEnd::Done);
        }
        debug_assert_eq!(offset, file.stream_position().await?);
        for limiter in &ctx.limiters {
            limiter.acquire(allowed).await;
        }
        file.write_all(&chunk[..allowed as usize]).await?;
        // tokio's File completes writes in the background; flush waits for it
        // so `written` never runs ahead of the bytes actually handed to the OS.
        file.flush().await?;
        ctx.received.fetch_add(allowed, Ordering::Relaxed);
        let mut table = ctx.table.lock().unwrap();
        table.commit(seg, allowed);
        if table.get(seg).and_then(|s| s.remaining()) == Some(0) {
            return Ok(SegmentEnd::Done);
        }
    }
}
