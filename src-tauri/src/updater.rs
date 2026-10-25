//! Updates from the GitHub releases of this project. The installer is only
//! run when its SHA-256 matches both the digest GitHub computed for the
//! release asset and the `SHA256SUMS.txt` published with the release (at
//! least one of the two must exist, and they must agree). The new version
//! is installed silently and started again; it then shows the release notes.

use std::path::{Path, PathBuf};

use dm_engine::checksum::{hash_file, Algo};
use dm_engine::{Engine, Job, Outcome, Progress, RequestSpec};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

pub const REPO_URL: &str = "https://github.com/Suleiman700/Bright-Downloader";
pub const SITE_URL: &str = "https://suleiman700.github.io/Bright-Downloader/";
const LATEST_API: &str = "https://api.github.com/repos/Suleiman700/Bright-Downloader/releases/latest";
const SUMS_ASSET: &str = "SHA256SUMS.txt";
/// Written before the installer runs, read by the new version.
const NOTES_FILE: &str = "update-notes.json";

/// Debug builds can be pointed at a test server with `BRIGHTDL_UPDATE_API`.
fn latest_api() -> String {
    #[cfg(debug_assertions)]
    if let Ok(url) = std::env::var("BRIGHTDL_UPDATE_API") {
        return url;
    }
    LATEST_API.to_string()
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub current: String,
    /// Release notes as written on GitHub (Markdown).
    pub notes: String,
    pub page: String,
    pub asset: String,
    pub url: String,
    pub size: u64,
    /// SHA-256 the installer must have.
    pub sha256: String,
}

/// Left behind for the version that is being installed.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AfterUpdate {
    pub version: String,
    pub notes: String,
    pub page: String,
    /// Downloads that were running and continue once the new version starts.
    pub resume: Vec<u64>,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    /// `sha256:<hex>`, computed by GitHub when the file was uploaded.
    #[serde(default)]
    digest: Option<String>,
}

/// `1.2.3` (an optional leading `v` and `-suffix` are ignored).
fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let v = v.split(['-', '+']).next()?;
    let mut parts = v.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().unwrap_or(Some(0))?;
    let patch = parts.next().unwrap_or(Some(0))?;
    Some((major, minor, patch))
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((parse_version(candidate), parse_version(current)), (Some(a), Some(b)) if a > b)
}

/// The NSIS installer Tauri builds: `Bright Downloader_1.2.3_x64-setup.exe`.
fn is_installer(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.ends_with("-setup.exe") && (name.contains("x64") || !name.contains("arm64"))
}

fn digest_sha256(asset: &GhAsset) -> Option<String> {
    let hex = asset.digest.as_deref()?.strip_prefix("sha256:")?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex.to_ascii_lowercase())
}

/// Newest published release, if it is newer than this program.
pub async fn check(engine: &Engine) -> Result<Option<UpdateInfo>, String> {
    let text = engine
        .client()
        .get(latest_api())
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("could not reach GitHub: {}", dm_engine::Error::from(e)))?
        .error_for_status()
        .map_err(|e| match e.status().map(|s| s.as_u16()) {
            Some(404) => "no release has been published yet".to_string(),
            Some(403 | 429) => "GitHub is limiting requests right now; try again later".to_string(),
            _ => format!("GitHub answered with an error: {}", dm_engine::Error::from(e)),
        })?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let release: GhRelease = serde_json::from_str(&text).map_err(|e| format!("GitHub sent an unexpected answer: {e}"))?;
    if release.draft || release.prerelease || !is_newer(&release.tag_name, current_version()) {
        return Ok(None);
    }
    let installer = release
        .assets
        .iter()
        .find(|a| is_installer(&a.name))
        .ok_or("the new release has no Windows installer yet")?;

    let from_github = digest_sha256(installer);
    let from_sums = match release.assets.iter().find(|a| a.name.eq_ignore_ascii_case(SUMS_ASSET)) {
        Some(sums) => {
            let text = engine
                .client()
                .get(&sums.browser_download_url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("could not get the checksum list: {}", dm_engine::Error::from(e)))?
                .text()
                .await
                .map_err(|e| e.to_string())?;
            Some(crate::tools::expected_hash(&text, &installer.name).ok_or("the checksum list does not include the installer")?)
        }
        None => None,
    };
    let sha256 = match (from_github, from_sums) {
        (Some(a), Some(b)) if a != b => return Err("the release's checksums disagree; the update was not offered".into()),
        (Some(h), _) | (None, Some(h)) => h,
        (None, None) => return Err("the release has no checksum for its installer, so it cannot be verified".into()),
    };

    Ok(Some(UpdateInfo {
        version: release.tag_name.trim_start_matches(['v', 'V']).to_string(),
        current: current_version().to_string(),
        notes: release.body.unwrap_or_default(),
        page: release.html_url,
        asset: installer.name.clone(),
        url: installer.browser_download_url.clone(),
        size: installer.size,
        sha256,
    }))
}

fn updates_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("updates")
}

/// Download and verify the installer. Returns its path.
pub async fn download<F>(engine: &Engine, data_dir: &Path, info: &UpdateInfo, on_progress: F) -> Result<PathBuf, String>
where
    F: Fn(&Progress) + Send + Sync,
{
    if !cfg!(windows) {
        return Err("automatic updates are only available on Windows".into());
    }
    // The name comes from GitHub; never let it point outside our folder.
    if info.asset.contains(['/', '\\']) || info.asset.contains("..") {
        return Err("the installer has an unexpected name".into());
    }
    let dir = updates_dir(data_dir);
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let path = dir.join(&info.asset);
    let job = Job { spec: RequestSpec::new(info.url.clone()), save_path: path.clone(), connections: 4 };
    match engine.download(&job, None, CancellationToken::new(), on_progress).await.map_err(|e| e.to_string())? {
        Outcome::Completed { .. } => {}
        Outcome::Paused => return Err("the download was interrupted".into()),
    }
    let file = path.clone();
    let actual = tokio::task::spawn_blocking(move || hash_file(&file, Algo::Sha256))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    if !actual.eq_ignore_ascii_case(&info.sha256) {
        let _ = tokio::fs::remove_dir_all(&dir).await;
        return Err("the downloaded update does not match the checksum published on GitHub; nothing was installed".into());
    }
    Ok(path)
}

/// Remember what to show and resume after the update, then start the
/// installer: `/P` shows only a progress bar, `/R` starts the app again and
/// `/UPDATE` keeps the user's shortcuts as they are.
pub fn run_installer(data_dir: &Path, installer: &Path, info: &UpdateInfo, resume: Vec<u64>) -> Result<(), String> {
    let after = AfterUpdate { version: info.version.clone(), notes: info.notes.clone(), page: info.page.clone(), resume };
    let json = serde_json::to_vec_pretty(&after).map_err(|e| e.to_string())?;
    std::fs::write(data_dir.join(NOTES_FILE), json).map_err(|e| e.to_string())?;
    std::process::Command::new(installer)
        .args(["/P", "/R", "/UPDATE"])
        .spawn()
        .map(drop)
        .map_err(|e| format!("could not start the installer: {e}"))
}

/// What was left by the update that installed this version (once), and
/// leftovers of earlier updates removed.
pub fn take_after_update(data_dir: &Path) -> Option<AfterUpdate> {
    let path = data_dir.join(NOTES_FILE);
    let text = std::fs::read(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(updates_dir(data_dir));
    let after: AfterUpdate = serde_json::from_slice(&text).ok()?;
    (parse_version(&after.version) == parse_version(current_version())).then_some(after)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("0.2.0-beta.1", "0.2.0"));
    }

    #[test]
    fn finds_the_installer() {
        assert!(is_installer("Bright Downloader_0.2.0_x64-setup.exe"));
        assert!(!is_installer("Bright Downloader_0.2.0_arm64-setup.exe"));
        assert!(!is_installer("Bright Downloader_0.2.0_x64_en-US.msi"));
        assert!(!is_installer("SHA256SUMS.txt"));
    }

    #[test]
    fn reads_github_digests() {
        let asset = |d: &str| GhAsset { name: "a".into(), browser_download_url: "u".into(), size: 1, digest: Some(d.into()) };
        let hex = "AB".repeat(32);
        assert_eq!(digest_sha256(&asset(&format!("sha256:{hex}"))), Some("ab".repeat(32)));
        assert_eq!(digest_sha256(&asset("sha512:00")), None);
        assert_eq!(digest_sha256(&asset("sha256:xyz")), None);
    }

    #[test]
    fn after_update_is_read_once_and_only_for_this_version() {
        let dir = tempfile::tempdir().unwrap();
        let info = UpdateInfo {
            version: current_version().into(),
            current: "0.0.1".into(),
            notes: "- New".into(),
            page: "p".into(),
            asset: "x".into(),
            url: "u".into(),
            size: 0,
            sha256: String::new(),
        };
        let after = AfterUpdate { version: info.version.clone(), notes: info.notes.clone(), page: info.page, resume: vec![3] };
        std::fs::write(dir.path().join(NOTES_FILE), serde_json::to_vec(&after).unwrap()).unwrap();
        let got = take_after_update(dir.path()).unwrap();
        assert_eq!((got.notes.as_str(), got.resume), ("- New", vec![3]));
        assert!(take_after_update(dir.path()).is_none());

        std::fs::write(dir.path().join(NOTES_FILE), br#"{"version":"999.0.0"}"#).unwrap();
        assert!(take_after_update(dir.path()).is_none(), "a failed install must not show notes");
    }
}
