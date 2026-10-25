//! Segmented, resumable HTTP download engine.
//!
//! The download strategy mirrors classic accelerators: probe the server for
//! range support, split the file across several connections, and whenever a
//! connection runs out of work let it take over half of the largest remaining
//! segment (see [`segment`]). Progress is persisted next to the target file so
//! a paused or crashed download resumes where it stopped.

pub mod checksum;
pub mod dash;
pub mod disk;
pub mod error;
pub mod ftp;
pub mod hls;
pub mod limiter;
pub mod probe;
pub mod segment;
mod task;

use std::time::Duration;

pub use error::{Error, Result};
pub use limiter::RateLimiter;
pub use probe::{probe, ProbeInfo};
pub use task::{part_path, state_path, temp_paths, Engine, Job, Outcome, Progress, SegmentView};

/// What to fetch and with which extra headers (cookies, referer, user agent
/// captured from the browser).
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct RequestSpec {
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

impl RequestSpec {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), headers: Vec::new() }
    }

    pub(crate) fn apply(&self, mut builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (name, value) in &self.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    /// Segments are never split into pieces smaller than this.
    pub min_split: u64,
    /// Consecutive failures a connection tolerates before giving up.
    pub max_retries: u32,
    pub retry_base_delay: Duration,
    pub connect_timeout: Duration,
    /// Maximum silence on a connection before it is recycled.
    pub read_timeout: Duration,
    pub progress_interval: Duration,
    pub user_agent: String,
    /// `http://`, `https://` or `socks5://` proxy, optionally with
    /// `user:pass@`. `None` uses the system / environment proxy settings.
    pub proxy: Option<String>,
    /// Ignore every proxy, including the system one.
    pub no_proxy: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            min_split: 256 * 1024,
            max_retries: 5,
            retry_base_delay: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(30),
            progress_interval: Duration::from_millis(500),
            user_agent: concat!("Bright Downloader/", env!("CARGO_PKG_VERSION")).to_string(),
            proxy: None,
            no_proxy: false,
        }
    }
}

/// HTTP client tuned for downloading: no transparent decompression (byte
/// offsets must refer to the bytes on the wire) and generous connection reuse.
pub fn build_client(opts: &Options) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(&opts.user_agent)
        .connect_timeout(opts.connect_timeout)
        .pool_max_idle_per_host(32);
    if opts.no_proxy {
        builder = builder.no_proxy();
    } else if let Some(proxy) = opts.proxy.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|e| Error::Proxy(e.to_string()))?);
    }
    Ok(builder.build()?)
}

/// `ftp://` addresses go through [`ftp`]; everything else through HTTP.
pub fn is_ftp(url: &str) -> bool {
    url.get(..6).is_some_and(|s| s.eq_ignore_ascii_case("ftp://"))
}
