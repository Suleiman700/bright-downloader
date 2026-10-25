use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{}", describe_http(.0))]
    Http(#[from] reqwest::Error),

    #[error("server responded with HTTP {0}")]
    Status(u16),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("invalid state file: {0}")]
    State(#[from] serde_json::Error),

    /// The FTP server answered with an error reply.
    #[error("FTP server: {message} ({code})")]
    Ftp { code: u16, message: String },

    /// A connection failed or dropped outside of the HTTP client (FTP).
    #[error("network error ({0})")]
    Network(String),

    /// The proxy address in the settings is not usable.
    #[error("invalid proxy: {0}")]
    Proxy(String),

    #[error("server did not honor the byte range request")]
    RangeNotHonored,

    #[error("connection closed before the segment was complete")]
    Truncated,

    #[error("no data received within the read timeout")]
    Timeout,

    #[error("not enough disk space: {} needed, {} free", fmt_size(*.needed), fmt_size(*.available))]
    DiskFull { needed: u64, available: u64 },

    /// A problem with an HLS stream that retrying will not fix.
    #[error("{0}")]
    Stream(String),

    #[error("all connections failed; last error: {0}")]
    AllWorkersFailed(String),
}

impl Error {
    /// Errors worth retrying on a fresh connection. Client errors such as
    /// 404/403 will not fix themselves, so they abort the whole download.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http(_) | Error::Truncated | Error::Timeout | Error::RangeNotHonored | Error::Network(_) => true,
            // 4xx FTP replies are transient (busy, too many users); 5xx are permanent.
            Error::Ftp { code, .. } => (400..500).contains(code),
            Error::Proxy(_) => false,
            Error::Status(code) => *code == 408 || *code == 429 || *code == 460 || *code >= 500,
            Error::Io(_) | Error::State(_) | Error::DiskFull { .. } | Error::Stream(_) | Error::AllWorkersFailed(_) => false,
        }
    }
}

impl Error {
    /// The server refuses extra parallel connections (429, 503, or the
    /// non-standard 460 some file hosts use). Fewer workers will succeed.
    pub fn is_connection_limit(&self) -> bool {
        // 421: FTP "too many connections from your address".
        matches!(self, Error::Status(429 | 460 | 503) | Error::Ftp { code: 421, .. })
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn fmt_size(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut v = n as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

/// reqwest's own message ("error sending request for url (...)") hides the
/// cause; name the kind of failure and append the innermost OS/TLS reason.
fn describe_http(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "connection timed out"
    } else if e.is_connect() {
        "could not connect to the server"
    } else if e.is_redirect() {
        "too many redirects"
    } else if e.is_body() || e.is_decode() {
        "connection dropped while receiving data"
    } else {
        "network error"
    };
    let mut cause: &dyn std::error::Error = e;
    while let Some(next) = cause.source() {
        cause = next;
    }
    let cause = cause.to_string();
    if cause.is_empty() || cause == e.to_string() {
        kind.to_string()
    } else {
        format!("{kind} ({cause})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_failures_name_the_cause() {
        // Port 9 on loopback: nothing listens, so the connection is refused.
        let err: Error = reqwest::get("http://127.0.0.1:9/").await.unwrap_err().into();
        let msg = err.to_string();
        assert!(msg.starts_with("could not connect to the server ("), "{msg}");
        assert!(!msg.contains("error sending request"), "{msg}");
    }
}
