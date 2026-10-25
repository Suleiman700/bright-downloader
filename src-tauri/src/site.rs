//! Video sites (YouTube and the many others yt-dlp knows) through the
//! yt-dlp program: list the qualities of a page's video, then download one
//! with progress, pause (stops the process; yt-dlp resumes its `.part`
//! files next time) and live recording.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use dm_engine::{Outcome, Progress};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteFormat {
    /// yt-dlp format selector.
    pub id: String,
    pub label: String,
    pub size: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteInfo {
    pub title: String,
    /// "Youtube", "Vimeo", ...
    pub site: String,
    pub duration: Option<f64>,
    pub live: bool,
    /// Best first; the last one is audio only.
    pub formats: Vec<SiteFormat>,
    /// ffmpeg is needed to merge the separate video and audio of most sites.
    pub ffmpeg: bool,
}

pub fn yt_dlp() -> Option<PathBuf> {
    dm_engine::hls::find_tool("yt-dlp")
}

pub const MISSING: &str =
    "This address is a web page, not a file. To download videos from sites like this, get yt-dlp in Options → Video.";

fn command(program: &Path, headers: &[(String, String)]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(["--no-playlist", "--no-warnings", "--no-color"]);
    for (name, value) in headers {
        // yt-dlp sets its own user agent per site unless told otherwise.
        cmd.arg("--add-header").arg(format!("{name}:{value}"));
    }
    if let Some(ff) = dm_engine::hls::find_ffmpeg() {
        cmd.arg("--ffmpeg-location").arg(ff);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

/// yt-dlp's last `ERROR:` line, without the prefix.
fn error_text(stderr: &str) -> String {
    stderr
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("ERROR: "))
        .map(|e| e.trim().to_string())
        .unwrap_or_else(|| stderr.lines().last().unwrap_or("yt-dlp failed").trim().to_string())
}

fn size_of(f: &serde_json::Value) -> Option<u64> {
    f["filesize"].as_u64().or_else(|| f["filesize_approx"].as_u64())
}

/// Quality choices from yt-dlp's format list: best, each height, audio only.
pub fn formats_from(info: &serde_json::Value) -> Vec<SiteFormat> {
    let formats = info["formats"].as_array().cloned().unwrap_or_default();
    let has_video = |f: &serde_json::Value| f["vcodec"].as_str().is_some_and(|v| v != "none");
    let audio_size = formats.iter().filter(|f| !has_video(f)).filter_map(size_of).max();
    let mut heights: Vec<u64> = formats.iter().filter(|f| has_video(f)).filter_map(|f| f["height"].as_u64()).collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));
    heights.dedup();
    let video_size = |h: u64| {
        let v = formats.iter().filter(|f| has_video(f) && f["height"].as_u64() == Some(h)).filter_map(size_of).max();
        // Video-only formats need the audio added; combined ones already have it.
        v.map(|v| v + audio_size.unwrap_or(0))
    };
    let mut out = vec![SiteFormat {
        id: "bv*+ba/b".into(),
        label: heights.first().map_or("Best quality".into(), |h| format!("Best quality ({h}p)")),
        size: heights.first().and_then(|h| video_size(*h)),
    }];
    for h in heights.iter().skip(1) {
        out.push(SiteFormat { id: format!("bv*[height<={h}]+ba/b[height<={h}]"), label: format!("{h}p"), size: video_size(*h) });
    }
    if !heights.is_empty() || audio_size.is_some() {
        out.push(SiteFormat { id: "ba/b".into(), label: "Audio only".into(), size: audio_size });
    }
    out
}

pub async fn inspect(url: &str, headers: &[(String, String)]) -> Result<SiteInfo, String> {
    let program = yt_dlp().ok_or(MISSING)?;
    let out = command(&program, headers)
        .arg("-J")
        .arg("--")
        .arg(url)
        .output()
        .await
        .map_err(|e| format!("could not start yt-dlp: {e}"))?;
    if !out.status.success() {
        let err = error_text(&String::from_utf8_lossy(&out.stderr));
        return Err(if err.contains("Unsupported URL") { "This address is a web page, not a file, and yt-dlp found no video on it.".into() } else { err });
    }
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|e| format!("unexpected yt-dlp output: {e}"))?;
    Ok(SiteInfo {
        title: info["title"].as_str().unwrap_or("video").to_string(),
        site: info["extractor_key"].as_str().unwrap_or("").to_string(),
        duration: info["duration"].as_f64(),
        live: info["is_live"].as_bool().unwrap_or(false),
        formats: formats_from(&info),
        ffmpeg: dm_engine::hls::find_ffmpeg().is_some(),
    })
}

/// "123", "NA" → number.
fn num(s: Option<&str>) -> Option<f64> {
    s.and_then(|v| v.parse::<f64>().ok())
}

/// Running totals across the separate files yt-dlp downloads (video, then audio).
#[derive(Default)]
struct Tally {
    done_before: u64,
    total_before: u64,
    last: u64,
    last_total: u64,
}

impl Tally {
    /// One `OBPROG downloaded total estimate speed` line.
    fn update(&mut self, line: &str) -> Option<Progress> {
        let mut parts = line.strip_prefix("OBPROG ")?.split_whitespace();
        let done = num(parts.next())? as u64;
        let total = num(parts.next()).or(num(parts.next())).map(|t| t as u64);
        let speed = num(parts.next()).unwrap_or(0.0) as u64;
        if done < self.last {
            // The next file started.
            self.done_before += self.last;
            self.total_before += self.last_total.max(self.last);
        }
        self.last = done;
        self.last_total = total.unwrap_or(0);
        Some(Progress {
            total: total.map(|t| self.total_before + t),
            downloaded: self.done_before + done,
            speed,
            connections: 1,
            accepts_ranges: true,
            segments: Vec::new(),
        })
    }
}

/// Download with yt-dlp into `save_path`'s folder, named after its stem.
/// The extension yt-dlp picks may differ; the outcome has the real path.
pub async fn download<F>(
    url: &str,
    headers: &[(String, String)],
    format: &str,
    save_path: &Path,
    connections: usize,
    live: bool,
    cancel: CancellationToken,
    on_progress: F,
) -> Result<Outcome, (String, bool)>
where
    F: Fn(&Progress) + Send + Sync,
{
    let program = yt_dlp().ok_or((MISSING.to_string(), false))?;
    let dir = save_path.parent().ok_or(("no folder".to_string(), false))?;
    tokio::fs::create_dir_all(dir).await.map_err(|e| (e.to_string(), false))?;
    let stem = save_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "video".into());
    let template = dir.join(format!("{}.%(ext)s", stem.replace('%', "%%")));
    let mut cmd = command(&program, headers);
    cmd.args(["-f", format, "--newline", "--progress", "--merge-output-format", "mp4"])
        .args(["-N", &connections.clamp(1, 16).to_string()])
        .args(["--progress-template", "download:OBPROG %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s"])
        .args(["--print", "after_move:OBDONE %(filepath)s"])
        .arg("-o")
        .arg(&template);
    if live {
        // Write a plain MPEG-TS that stays playable when recording is stopped.
        cmd.args(["--hls-use-mpegts", "--no-part"]);
    }
    let mut child = cmd.arg("--").arg(url).spawn().map_err(|e| (format!("could not start yt-dlp: {e}"), false))?;
    let pid = child.id();
    let mut stdout = BufReader::new(child.stdout.take().expect("piped")).lines();
    let stderr = child.stderr.take().expect("piped");
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            text.push_str(&l);
            text.push('\n');
        }
        text
    });

    let mut tally = Tally::default();
    let mut final_path: Option<PathBuf> = None;
    let mut stopped = false;
    loop {
        tokio::select! {
            _ = cancel.cancelled(), if !stopped => {
                stopped = true;
                kill_tree(pid, &mut child).await;
            }
            line = stdout.next_line() => match line {
                Ok(Some(l)) => {
                    if let Some(p) = tally.update(&l) {
                        on_progress(&p);
                    } else if let Some(path) = l.strip_prefix("OBDONE ") {
                        final_path = Some(PathBuf::from(path.trim()));
                    }
                }
                _ => break,
            },
        }
    }
    let status = child.wait().await.map_err(|e| (e.to_string(), true))?;
    let stderr = errors.await.unwrap_or_default();

    if stopped {
        // A stopped live recording is kept as it is; a video resumes later.
        if live {
            if let Some(path) = newest_with_stem(dir, &stem) {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                return Ok(Outcome::Completed { path, size });
            }
        }
        return Ok(Outcome::Paused);
    }
    if !status.success() {
        let err = error_text(&stderr);
        let retryable = ["timed out", "Connection", "HTTP Error 5", "Temporary failure", "Remote end closed"].iter().any(|w| err.contains(w));
        return Err((err, retryable));
    }
    let path = final_path.or_else(|| newest_with_stem(dir, &stem)).ok_or(("yt-dlp did not report the saved file".to_string(), false))?;
    let size = std::fs::metadata(&path).map(|m| m.len()).map_err(|e| (e.to_string(), false))?;
    Ok(Outcome::Completed { path, size })
}

/// Stop yt-dlp and the ffmpeg it may have started.
async fn kill_tree(pid: Option<u32>, child: &mut tokio::process::Child) {
    #[cfg(windows)]
    if let Some(pid) = pid {
        let mut kill = tokio::process::Command::new("taskkill");
        kill.args(["/PID", &pid.to_string(), "/T", "/F"]).stdout(Stdio::null()).stderr(Stdio::null());
        kill.creation_flags(0x0800_0000);
        if kill.status().await.is_ok_and(|s| s.success()) {
            return;
        }
    }
    let _ = pid;
    let _ = child.kill().await;
}

/// The most recently changed finished file named `<stem>.<ext>` in `dir`.
fn newest_with_stem(dir: &Path, stem: &str) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix(stem).and_then(|r| r.strip_prefix('.')).is_some_and(|ext| !ext.contains('.'))
        })
        .max_by_key(|e| e.metadata().and_then(|m| m.modified()).ok())
        .map(|e| e.path())
}

/// yt-dlp's leftovers for a download (`stem.f137.mp4.part`, `.ytdl`, ...).
pub fn temp_files(save_path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (save_path.parent(), save_path.file_stem().map(|s| s.to_string_lossy().into_owned())) else {
        return Vec::new();
    };
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    name.starts_with(&format!("{stem}.")) && [".part", ".ytdl", ".temp"].iter().any(|s| name.contains(s))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_list_from_formats() {
        let info = serde_json::json!({ "formats": [
            { "format_id": "140", "vcodec": "none", "acodec": "mp4a", "filesize": 3_000_000 },
            { "format_id": "137", "vcodec": "avc1", "height": 1080, "filesize": 50_000_000 },
            { "format_id": "136", "vcodec": "avc1", "height": 720, "filesize_approx": 20_000_000 },
            { "format_id": "18", "vcodec": "avc1", "acodec": "mp4a", "height": 360 }
        ]});
        let f = formats_from(&info);
        let labels: Vec<_> = f.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["Best quality (1080p)", "720p", "360p", "Audio only"]);
        assert_eq!(f[0].size, Some(53_000_000));
        assert_eq!(f[1].id, "bv*[height<=720]+ba/b[height<=720]");
        assert_eq!(f[2].size, None);
        assert_eq!(f[3].size, Some(3_000_000));
    }

    #[test]
    fn progress_adds_up_video_then_audio() {
        let mut t = Tally::default();
        let p = t.update("OBPROG 500 1000 NA 100.5").unwrap();
        assert_eq!((p.downloaded, p.total, p.speed), (500, Some(1000), 100));
        t.update("OBPROG 1000 1000 NA NA");
        let p = t.update("OBPROG 50 NA 200 NA").unwrap();
        assert_eq!((p.downloaded, p.total), (1050, Some(1200)));
        assert!(t.update("[download] something else").is_none());
    }

    #[test]
    fn error_lines() {
        assert_eq!(error_text("WARNING: x\nERROR: [youtube] abc: Video unavailable\n"), "[youtube] abc: Video unavailable");
        assert_eq!(error_text("plain failure"), "plain failure");
    }
}
