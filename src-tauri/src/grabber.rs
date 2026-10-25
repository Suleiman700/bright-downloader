//! Site grabber: read a web page (and, with depth > 0, the pages it links to
//! on the same site) and collect the links that point at files.

use std::collections::{HashSet, VecDeque};
use std::time::Duration;

use serde::Serialize;

/// Pages fetched per grab, and the overall time budget; what was found by
/// then is returned.
const MAX_PAGES: usize = 50;
const BUDGET: Duration = Duration::from_secs(90);
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_PAGE_BYTES: usize = 5 << 20;

/// Extensions of links that are followed as pages rather than offered as files.
const PAGE_EXTS: &[&str] = &["html", "htm", "shtml", "xhtml", "php", "asp", "aspx", "jsp", "cgi"];

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    pub url: String,
    pub file_name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Grabbed {
    pub files: Vec<Found>,
    pub pages: usize,
    /// The crawl stopped at the page or time limit.
    pub truncated: bool,
}

/// Values of every `href` / `src` attribute, entity `&amp;` decoded.
pub fn extract_links(html: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Compare bytes: `i` need not be on a char boundary in non-ASCII pages.
        let attr_len = if bytes[i..].starts_with(b"href") {
            4
        } else if bytes[i..].starts_with(b"src") {
            3
        } else {
            i += 1;
            continue;
        };
        let preceded = i > 0 && (bytes[i - 1].is_ascii_whitespace() || bytes[i - 1] == b'"' || bytes[i - 1] == b'\'');
        let mut j = i + attr_len;
        i = j;
        if !preceded {
            continue;
        }
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'=' {
            continue;
        }
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let (start, end) = match bytes.get(j) {
            Some(&q @ (b'"' | b'\'')) => {
                let Some(len) = lower[j + 1..].find(q as char) else { break };
                (j + 1, j + 1 + len)
            }
            Some(_) => {
                let len = lower[j..].find(|c: char| c.is_ascii_whitespace() || c == '>').unwrap_or(lower.len() - j);
                (j, j + len)
            }
            None => break,
        };
        // Slice the original text: URLs are case-sensitive.
        let value = html[start..end].trim().replace("&amp;", "&");
        if !value.is_empty() {
            out.push(value);
        }
        i = end;
    }
    out
}

fn extension(url: &url::Url) -> Option<String> {
    let last = url.path_segments()?.next_back()?;
    let (_, ext) = last.rsplit_once('.')?;
    (!ext.is_empty()).then(|| ext.to_ascii_lowercase())
}

/// Crawl from `start`. Pages are only followed on the start page's host;
/// files may live anywhere (download mirrors, CDNs).
pub async fn grab(client: &reqwest::Client, start: &str, depth: u32, exts: &[String]) -> Result<Grabbed, String> {
    let start = url::Url::parse(start.trim()).map_err(|e| format!("invalid URL: {e}"))?;
    if !matches!(start.scheme(), "http" | "https") {
        return Err("only http and https pages can be grabbed".into());
    }
    let exts: HashSet<String> = exts.iter().map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase()).collect();
    let deadline = tokio::time::Instant::now() + BUDGET;
    let mut queue = VecDeque::from([(start.clone(), 0u32)]);
    let mut seen_pages = HashSet::from([start.to_string()]);
    let mut seen_files = HashSet::new();
    let mut files = Vec::new();
    let mut pages = 0;
    let mut truncated = false;

    while let Some((page, level)) = queue.pop_front() {
        if pages >= MAX_PAGES || tokio::time::Instant::now() >= deadline {
            truncated = true;
            break;
        }
        let fetched = tokio::time::timeout(PAGE_TIMEOUT, fetch_html(client, &page)).await.unwrap_or(Err("timed out".into()));
        let (base, html) = match fetched {
            Ok(Some(found)) => found,
            // Only the start page must be readable; broken links further down are skipped.
            Ok(None) if pages == 0 => return Err("the address is not a web page".into()),
            Err(e) if pages == 0 => return Err(e),
            _ => continue,
        };
        pages += 1;
        for link in extract_links(&html) {
            let Ok(mut url) = base.join(&link) else { continue };
            if !matches!(url.scheme(), "http" | "https") {
                continue;
            }
            url.set_fragment(None);
            let ext = extension(&url);
            if ext.as_ref().is_some_and(|e| exts.contains(e)) {
                if seen_files.insert(url.to_string()) {
                    let file_name = dm_engine::probe::filename_from_url(url.as_str()).unwrap_or_default();
                    files.push(Found { url: url.to_string(), file_name });
                }
            } else if level < depth
                && url.host_str() == start.host_str()
                && ext.as_deref().is_none_or(|e| PAGE_EXTS.contains(&e))
                && seen_pages.insert(url.to_string())
            {
                queue.push_back((url, level + 1));
            }
        }
    }
    Ok(Grabbed { files, pages, truncated })
}

/// The page's final URL and text, or `None` if it is not HTML.
async fn fetch_html(client: &reqwest::Client, url: &url::Url) -> Result<Option<(url::Url, String)>, String> {
    let resp = client.get(url.clone()).send().await.map_err(|e| dm_engine::Error::from(e).to_string())?;
    if !resp.status().is_success() {
        return Err(format!("the server responded with HTTP {}", resp.status().as_u16()));
    }
    let is_html = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.contains("html"));
    if !is_html || resp.content_length().is_some_and(|n| n as usize > MAX_PAGE_BYTES) {
        return Ok(None);
    }
    let base = resp.url().clone();
    let mut body = resp.bytes().await.map_err(|e| dm_engine::Error::from(e).to_string())?.to_vec();
    body.truncate(MAX_PAGE_BYTES);
    Ok(Some((base, String::from_utf8_lossy(&body).into_owned())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::{Html, IntoResponse};
    use axum::routing::get;

    #[test]
    fn finds_attributes_in_messy_html() {
        let html = r#"<A HREF="Files/One.ZIP">x</a> <img src='/pic.png'> <a href=plain.exe>y</a>
            <a data-href="nope.zip"></a> <a href = "a.php?x=1&amp;y=2#top">z</a> <a href="">e</a>"#;
        assert_eq!(extract_links(html), ["Files/One.ZIP", "/pic.png", "plain.exe", "a.php?x=1&y=2#top"]);
    }

    async fn site() -> String {
        let app = axum::Router::new()
            .route(
                "/",
                get(|| async {
                    Html(r#"<a href="a.zip">a</a><a href="sub/">sub</a><a href="https://other.example/x.html">ext</a>
                        <a href="https://mirror.example/b.iso">b</a><a href="a.zip#dup">dup</a><a href="pic.png">p</a>"#)
                }),
            )
            .route("/sub/", get(|| async { Html(r#"<a href="../c.exe">c</a><a href="deeper.html">d</a>"#) }))
            .route("/sub/deeper.html", get(|| async { Html(r#"<a href="/d.msi">d</a>"#) }))
            .route("/a.zip", get(|| async { "zip".into_response() }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    fn names(g: &Grabbed) -> Vec<&str> {
        g.files.iter().map(|f| f.file_name.as_str()).collect()
    }

    #[tokio::test]
    async fn crawls_to_the_requested_depth() {
        let base = site().await;
        let exts: Vec<String> = ["zip", "iso", "exe", "msi"].map(String::from).to_vec();
        let client = reqwest::Client::new();

        let g = grab(&client, &base, 0, &exts).await.unwrap();
        assert_eq!(names(&g), ["a.zip", "b.iso"]);
        assert_eq!(g.pages, 1);

        let g = grab(&client, &base, 1, &exts).await.unwrap();
        assert_eq!(names(&g), ["a.zip", "b.iso", "c.exe"]);

        let g = grab(&client, &base, 2, &exts).await.unwrap();
        assert_eq!(names(&g), ["a.zip", "b.iso", "c.exe", "d.msi"]);
        assert_eq!(g.pages, 3, "the other site's page is never fetched");
    }

    #[tokio::test]
    async fn start_page_must_be_html() {
        let base = site().await;
        let err = grab(&reqwest::Client::new(), &format!("{base}a.zip"), 1, &[]).await.unwrap_err();
        assert!(err.contains("not a web page"), "{err}");
    }
}
