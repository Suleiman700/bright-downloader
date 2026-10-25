//! Helper programs Bright Downloader can fetch on request: ffmpeg (saves streams as
//! MP4 and merges separate audio) and yt-dlp (video sites such as YouTube).
//! Both come from their official GitHub releases and are checked against
//! the SHA-256 sums published with them before they are installed into the
//! app's own `tools` folder. Nothing is downloaded unless the user asks.

use std::path::{Path, PathBuf};

use dm_engine::checksum::{hash_file, Algo};
use dm_engine::{Engine, Job, Outcome, Progress, RequestSpec};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tool {
    Ffmpeg,
    YtDlp,
}

struct Release {
    /// File to download.
    url: &'static str,
    /// Its name in the sums file.
    asset: &'static str,
    sums: &'static str,
}

impl Tool {
    pub fn program(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::YtDlp => "yt-dlp",
        }
    }

    fn release(self) -> Release {
        match self {
            Tool::Ffmpeg => Release {
                url: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl.zip",
                asset: "ffmpeg-master-latest-win64-gpl.zip",
                sums: "https://github.com/BtbN/FFmpeg-Builds/releases/download/latest/checksums.sha256",
            },
            Tool::YtDlp => Release {
                url: "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe",
                asset: "yt-dlp.exe",
                sums: "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS",
            },
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsStatus {
    pub ffmpeg: Option<PathBuf>,
    pub yt_dlp: Option<PathBuf>,
    /// Where one-click installs go.
    pub dir: PathBuf,
}

pub fn dir(data_dir: &Path) -> PathBuf {
    data_dir.join("tools")
}

pub fn status(data_dir: &Path) -> ToolsStatus {
    ToolsStatus { ffmpeg: dm_engine::hls::find_ffmpeg(), yt_dlp: dm_engine::hls::find_tool("yt-dlp"), dir: dir(data_dir) }
}

/// `<sha256>  <file>` lines → the hash listed for `asset`.
pub fn expected_hash(sums: &str, asset: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hash, name) = l.trim().split_once(char::is_whitespace)?;
        (name.trim().trim_start_matches('*') == asset && hash.len() == 64).then(|| hash.to_ascii_lowercase())
    })
}

/// First file called `name` below `dir`.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|n| n.eq_ignore_ascii_case(name)) {
            return Some(path);
        }
    }
    None
}

/// Download, verify and install `tool`. Returns the installed program.
pub async fn install<F>(engine: &Engine, data_dir: &Path, tool: Tool, on_progress: F) -> Result<PathBuf, String>
where
    F: Fn(&Progress) + Send + Sync,
{
    if !cfg!(windows) {
        return Err("automatic installation is only available on Windows; install it with your package manager".into());
    }
    let release = tool.release();
    let tools = dir(data_dir);
    let work = tools.join(format!("{}-download", tool.program()));
    let _ = tokio::fs::remove_dir_all(&work).await;
    tokio::fs::create_dir_all(&work).await.map_err(|e| e.to_string())?;

    let sums = engine
        .client()
        .get(release.sums)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("could not get the checksum list: {}", dm_engine::Error::from(e)))?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let expected = expected_hash(&sums, release.asset).ok_or("the release lists no checksum for the download")?;

    let archive = work.join(release.asset);
    let job = Job { spec: RequestSpec::new(release.url), save_path: archive.clone(), connections: 8 };
    match engine.download(&job, None, CancellationToken::new(), on_progress).await.map_err(|e| e.to_string())? {
        Outcome::Completed { .. } => {}
        Outcome::Paused => return Err("the download was interrupted".into()),
    }
    let file = archive.clone();
    let actual = tokio::task::spawn_blocking(move || hash_file(&file, Algo::Sha256))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if actual != expected {
        let _ = tokio::fs::remove_dir_all(&work).await;
        return Err("the downloaded file does not match its published checksum; nothing was installed".into());
    }

    let installed = tools.join(format!("{}.exe", tool.program()));
    match tool {
        Tool::YtDlp => tokio::fs::copy(&archive, &installed).await.map(drop).map_err(|e| e.to_string())?,
        Tool::Ffmpeg => {
            let unpacked = work.join("unpacked");
            let (zip, into) = (archive.clone(), unpacked.clone());
            tokio::task::spawn_blocking(move || crate::post::extract(&zip, &into)).await.map_err(|e| e.to_string())??;
            // ffprobe is used by yt-dlp for some videos; copy it along.
            for name in ["ffmpeg.exe", "ffprobe.exe"] {
                let found = find_file(&unpacked, name).ok_or_else(|| format!("{name} is missing from the download"))?;
                tokio::fs::copy(found, tools.join(name)).await.map_err(|e| e.to_string())?;
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&work).await;
    Ok(installed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_sha256_lists() {
        let h = "a".repeat(64);
        let sums = format!("{}  other.zip\n{h}  ffmpeg-master-latest-win64-gpl.zip\n{} *yt-dlp.exe\n", "b".repeat(64), "C".repeat(64));
        assert_eq!(expected_hash(&sums, "ffmpeg-master-latest-win64-gpl.zip"), Some(h));
        assert_eq!(expected_hash(&sums, "yt-dlp.exe"), Some("c".repeat(64)));
        assert_eq!(expected_hash(&sums, "missing"), None);
    }
}
