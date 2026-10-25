//! First contact with the server: size, range support, file name, validators.

use percent_encoding::percent_decode_str;
use reqwest::header::{self, HeaderMap};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::RequestSpec;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProbeInfo {
    /// URL after redirects; segment requests go here directly.
    pub final_url: String,
    pub total_size: Option<u64>,
    pub accepts_ranges: bool,
    pub file_name: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Ask for the first byte only. A `206` tells us ranges work and carries the
/// full length in `Content-Range`; a `200` means the server ignores ranges.
/// The body is never read, so a `200` costs only the headers.
pub async fn probe(client: &Client, spec: &RequestSpec) -> Result<ProbeInfo> {
    if crate::is_ftp(&spec.url) {
        return crate::ftp::probe(&spec.url, std::time::Duration::from_secs(20)).await;
    }
    let resp = spec.apply(client.get(&spec.url)).header(header::RANGE, "bytes=0-0").send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Status(status.as_u16()));
    }
    let final_url = resp.url().to_string();
    let headers = resp.headers();

    let (total_size, accepts_ranges) = if status == StatusCode::PARTIAL_CONTENT {
        (content_range_total(headers), true)
    } else {
        (resp.content_length().filter(|&n| n > 0), false)
    };

    let file_name = content_disposition_filename(headers)
        .or_else(|| filename_from_url(&final_url))
        .map(|n| sanitize_filename(&n))
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "download".to_string());

    Ok(ProbeInfo {
        final_url,
        total_size,
        accepts_ranges,
        file_name,
        content_type: header_str(headers, header::CONTENT_TYPE),
        etag: header_str(headers, header::ETAG),
        last_modified: header_str(headers, header::LAST_MODIFIED),
    })
}

fn header_str(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_string)
}

/// `Content-Range: bytes 0-0/12345` → 12345 (`/*` → unknown).
fn content_range_total(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    value.rsplit('/').next()?.trim().parse().ok()
}

pub fn content_disposition_filename(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::CONTENT_DISPOSITION)?.to_str().ok()?;
    parse_content_disposition(value)
}

/// Prefers RFC 5987 `filename*=UTF-8''...` over plain `filename=`.
pub fn parse_content_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';').map(str::trim) {
        let Some((key, val)) = part.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        if key == "filename*" {
            let encoded = val.splitn(3, '\'').nth(2).unwrap_or(val);
            if let Ok(decoded) = percent_decode_str(encoded).decode_utf8() {
                return Some(decoded.into_owned());
            }
        } else if key == "filename" {
            plain = Some(val.trim_matches('"').to_string());
        }
    }
    plain
}

pub fn filename_from_url(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let last = parsed.path_segments()?.rev().find(|s| !s.is_empty())?;
    Some(percent_decode_str(last).decode_utf8_lossy().into_owned())
}

/// Make a name safe for Windows: strip path separators and reserved
/// characters, trailing dots/spaces, and dodge reserved device names.
pub fn sanitize_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || r#"<>:"|?*"#.contains(c) { '_' } else { c })
        .collect();
    cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: &[&str] = &[
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1",
        "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if RESERVED.contains(&stem.as_str()) {
        cleaned.insert(0, '_');
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_variants() {
        assert_eq!(parse_content_disposition(r#"attachment; filename="a b.zip""#).as_deref(), Some("a b.zip"));
        assert_eq!(parse_content_disposition("attachment; filename=plain.iso").as_deref(), Some("plain.iso"));
        assert_eq!(
            parse_content_disposition(r#"attachment; filename="fallback.txt"; filename*=UTF-8''%D9%85%D9%84%D9%81.txt"#)
                .as_deref(),
            Some("ملف.txt")
        );
        assert_eq!(parse_content_disposition("inline"), None);
    }

    #[test]
    fn url_filenames() {
        assert_eq!(filename_from_url("https://x.com/a/b/My%20File.exe?x=1").as_deref(), Some("My File.exe"));
        assert_eq!(filename_from_url("https://x.com/dir/").as_deref(), Some("dir"));
        assert_eq!(filename_from_url("https://x.com/"), None);
    }

    #[test]
    fn sanitizes_windows_names() {
        assert_eq!(sanitize_filename("a:b?c.txt"), "a_b_c.txt");
        assert_eq!(sanitize_filename("../../evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename("con.txt"), "_con.txt");
        assert_eq!(sanitize_filename("name. . "), "name");
    }
}
