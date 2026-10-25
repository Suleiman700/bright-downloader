//! Minimal FTP client: enough to probe a file and stream it from any offset,
//! so FTP downloads get the same multi-connection segmenting and resume as
//! HTTP (each connection is its own FTP session with `REST` + `RETR`).
//!
//! Plain FTP only (no FTPS). Credentials come from the URL
//! (`ftp://user:pass@host/path`), else anonymous.

use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use percent_encoding::percent_decode_str;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio_util::io::ReaderStream;

use crate::error::{Error, Result};
use crate::probe::{filename_from_url, sanitize_filename, ProbeInfo};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>;

struct Control {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    peer: std::net::IpAddr,
}

fn net(e: std::io::Error) -> Error {
    Error::Network(e.to_string())
}

impl Control {
    async fn connect(url: &url::Url, timeout: Duration) -> Result<Self> {
        let host = url.host_str().ok_or_else(|| Error::Ftp { code: 0, message: "no host in address".into() })?;
        let port = url.port().unwrap_or(21);
        let stream = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(net)?;
        let peer = stream.peer_addr().map_err(net)?.ip();
        let (r, w) = stream.into_split();
        let mut c = Self { reader: BufReader::new(r), writer: w, peer };
        c.expect(&[220]).await?;
        Ok(c)
    }

    /// One reply, joining multi-line replies (`123-...` up to `123 ...`).
    async fn reply(&mut self) -> Result<(u16, String)> {
        let mut first = String::new();
        if self.reader.read_line(&mut first).await.map_err(net)? == 0 {
            return Err(Error::Network("the FTP server closed the connection".into()));
        }
        let code: u16 = first.get(..3).and_then(|c| c.parse().ok()).ok_or_else(|| Error::Ftp {
            code: 0,
            message: format!("unexpected reply: {}", first.trim()),
        })?;
        let mut text = first.get(4..).unwrap_or("").trim().to_string();
        if first.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            loop {
                let mut line = String::new();
                if self.reader.read_line(&mut line).await.map_err(net)? == 0 {
                    break;
                }
                if line.starts_with(&end) {
                    break;
                }
                text.push('\n');
                text.push_str(line.trim());
            }
        }
        Ok((code, text))
    }

    async fn expect(&mut self, ok: &[u16]) -> Result<(u16, String)> {
        let (code, text) = self.reply().await?;
        if ok.contains(&code) {
            Ok((code, text))
        } else {
            Err(Error::Ftp { code, message: text })
        }
    }

    async fn cmd(&mut self, line: &str, ok: &[u16]) -> Result<(u16, String)> {
        self.writer.write_all(format!("{line}\r\n").as_bytes()).await.map_err(net)?;
        self.expect(ok).await
    }

    async fn login(&mut self, url: &url::Url) -> Result<()> {
        let user = match url.username() {
            "" => "anonymous".to_string(),
            u => percent_decode_str(u).decode_utf8_lossy().into_owned(),
        };
        let pass = url.password().map_or("brightdl@".to_string(), |p| percent_decode_str(p).decode_utf8_lossy().into_owned());
        let (code, _) = self.cmd(&format!("USER {user}"), &[230, 331]).await?;
        if code == 331 {
            self.cmd(&format!("PASS {pass}"), &[230, 202]).await?;
        }
        self.cmd("TYPE I", &[200]).await?;
        Ok(())
    }

    /// Open a passive data connection (EPSV, falling back to PASV). The
    /// server's control address is used, never the one in a PASV reply, so a
    /// server cannot point us at another host.
    async fn data(&mut self, timeout: Duration) -> Result<TcpStream> {
        let port = match self.cmd("EPSV", &[229]).await {
            Ok((_, text)) => text
                .split('|')
                .filter(|p| !p.is_empty())
                .find_map(|p| p.trim_end_matches(')').parse::<u16>().ok()),
            Err(_) => {
                let (_, text) = self.cmd("PASV", &[227]).await?;
                let inner = text.split('(').nth(1).and_then(|t| t.split(')').next()).unwrap_or("");
                let nums: Vec<u16> = inner.split(',').filter_map(|n| n.trim().parse().ok()).collect();
                (nums.len() == 6).then(|| nums[4] * 256 + nums[5])
            }
        }
        .ok_or_else(|| Error::Ftp { code: 0, message: "could not open a passive data connection".into() })?;
        tokio::time::timeout(timeout, TcpStream::connect((self.peer, port)))
            .await
            .map_err(|_| Error::Timeout)?
            .map_err(net)
    }
}

fn path_of(url: &url::Url) -> String {
    percent_decode_str(url.path()).decode_utf8_lossy().into_owned()
}

/// Size, resume support (`REST`) and modification time of an FTP file.
pub async fn probe(raw_url: &str, timeout: Duration) -> Result<ProbeInfo> {
    let url = url::Url::parse(raw_url).map_err(|e| Error::Ftp { code: 0, message: e.to_string() })?;
    let mut c = Control::connect(&url, timeout).await?;
    c.login(&url).await?;
    let path = path_of(&url);
    let total_size = match c.cmd(&format!("SIZE {path}"), &[213]).await {
        Ok((_, t)) => t.trim().parse().ok(),
        Err(Error::Ftp { code: 550, .. }) => return Err(Error::Status(404)),
        Err(_) => None,
    };
    let accepts_ranges = c.cmd("REST 0", &[350]).await.is_ok();
    let last_modified = c.cmd(&format!("MDTM {path}"), &[213]).await.ok().map(|(_, t)| t.trim().to_string());
    let _ = c.cmd("QUIT", &[221]).await;
    let file_name = filename_from_url(raw_url).map(|n| sanitize_filename(&n)).filter(|n| !n.is_empty()).unwrap_or_else(|| "download".into());
    Ok(ProbeInfo {
        final_url: raw_url.to_string(),
        total_size,
        accepts_ranges,
        file_name,
        content_type: None,
        etag: None,
        last_modified,
    })
}

/// Stream the file starting at byte `offset`, on a fresh FTP session.
/// Dropping the stream closes both connections.
pub async fn open_at(raw_url: &str, offset: u64, timeout: Duration) -> Result<ByteStream> {
    let url = url::Url::parse(raw_url).map_err(|e| Error::Ftp { code: 0, message: e.to_string() })?;
    let mut c = Control::connect(&url, timeout).await?;
    c.login(&url).await?;
    let data = c.data(timeout).await?;
    if offset > 0 {
        c.cmd(&format!("REST {offset}"), &[350]).await?;
    }
    match c.cmd(&format!("RETR {}", path_of(&url)), &[125, 150]).await {
        Err(Error::Ftp { code: 550, .. }) => return Err(Error::Status(404)),
        other => other?,
    };
    // Keep the control connection alive for as long as the data is read.
    let stream = ReaderStream::with_capacity(data, 64 * 1024).map(move |r| {
        let _keep = &c;
        r.map_err(net)
    });
    Ok(Box::pin(stream))
}
