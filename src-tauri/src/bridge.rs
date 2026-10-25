//! Local endpoint the browser extension talks to.
//!
//! Only requests whose `Origin` is a browser extension are accepted. Web pages
//! cannot forge that header, so arbitrary sites cannot push downloads into the
//! app. The listener binds to loopback only.

use std::net::{Ipv4Addr, SocketAddr};

use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

pub const PORT: u16 = 47809;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CapturedDownload {
    pub url: String,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub referrer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Quality picked on the page for an HLS stream (variant playlist URL).
    #[serde(default)]
    pub variant: Option<String>,
}

impl CapturedDownload {
    /// Headers the engine must send so the server treats us like the browser.
    pub fn headers(&self) -> Vec<(String, String)> {
        let mut h = Vec::new();
        if let Some(c) = self.cookies.as_deref().filter(|c| !c.is_empty()) {
            h.push(("Cookie".into(), c.into()));
        }
        if let Some(r) = self.referrer.as_deref().filter(|r| !r.is_empty()) {
            h.push(("Referer".into(), r.into()));
        }
        if let Some(ua) = self.user_agent.as_deref().filter(|u| !u.is_empty()) {
            h.push(("User-Agent".into(), ua.into()));
        }
        h
    }
}

/// Called for every accepted capture; the app shows its Add dialog.
pub type OnCapture = std::sync::Arc<dyn Fn(CapturedDownload) + Send + Sync>;

fn extension_origin(headers: &HeaderMap) -> Option<HeaderValue> {
    let origin = headers.get(header::ORIGIN)?;
    let s = origin.to_str().ok()?;
    let ok = ["chrome-extension://", "moz-extension://", "extension://"].iter().any(|p| s.starts_with(p));
    ok.then(|| origin.clone())
}

fn with_cors(origin: HeaderValue, resp: impl IntoResponse) -> Response {
    let mut resp = resp.into_response();
    let h = resp.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, OPTIONS"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_static("content-type"));
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    resp
}

async fn ping(headers: HeaderMap) -> Response {
    match extension_origin(&headers) {
        Some(o) => with_cors(o, Json(serde_json::json!({ "app": "Bright Downloader", "version": env!("CARGO_PKG_VERSION") }))),
        None => StatusCode::FORBIDDEN.into_response(),
    }
}

async fn add(State(on_capture): State<OnCapture>, headers: HeaderMap, body: String) -> Response {
    let Some(origin) = extension_origin(&headers) else { return StatusCode::FORBIDDEN.into_response() };
    let Ok(capture) = serde_json::from_str::<CapturedDownload>(&body) else {
        return with_cors(origin, StatusCode::BAD_REQUEST);
    };
    if !["http://", "https://", "ftp://"].iter().any(|p| capture.url.starts_with(p)) {
        return with_cors(origin, StatusCode::UNPROCESSABLE_ENTITY);
    }
    on_capture(capture);
    with_cors(origin, StatusCode::ACCEPTED)
}

async fn preflight(headers: HeaderMap) -> Response {
    match extension_origin(&headers) {
        Some(o) => with_cors(o, StatusCode::NO_CONTENT),
        None => StatusCode::FORBIDDEN.into_response(),
    }
}

pub fn router(on_capture: OnCapture) -> Router {
    Router::new()
        .route("/ping", get(ping).options(preflight))
        .route("/add", post(add).options(preflight))
        .method_not_allowed_fallback(|m: Method| async move { (StatusCode::METHOD_NOT_ALLOWED, m.to_string()) })
        .with_state(on_capture)
}

/// Serve on loopback. Fails if another instance already owns the port.
pub async fn serve(port: u16, on_capture: OnCapture) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).await?;
    axum::serve(listener, router(on_capture)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    async fn start() -> (String, Arc<Mutex<Vec<CapturedDownload>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let on: OnCapture = Arc::new(move |c| s.lock().unwrap().push(c));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(on)).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    const EXT: &str = "chrome-extension://abcdefghijklmnop";

    #[tokio::test]
    async fn accepts_extension_and_builds_headers() {
        let (base, seen) = start().await;
        let body = r#"{"url":"https://example.com/a.zip","referrer":"https://example.com/","cookies":"sid=1","userAgent":"UA"}"#;
        let resp = reqwest::Client::new()
            .post(format!("{base}/add"))
            .header("Origin", EXT)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 202);
        assert_eq!(resp.headers()["access-control-allow-origin"], EXT);
        let got = seen.lock().unwrap()[0].clone();
        assert_eq!(
            got.headers(),
            vec![
                ("Cookie".to_string(), "sid=1".to_string()),
                ("Referer".to_string(), "https://example.com/".to_string()),
                ("User-Agent".to_string(), "UA".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn rejects_web_pages_and_missing_origin() {
        let (base, seen) = start().await;
        let client = reqwest::Client::new();
        for origin in [Some("https://evil.example"), None] {
            let mut req = client.post(format!("{base}/add")).body(r#"{"url":"https://x/y"}"#);
            if let Some(o) = origin {
                req = req.header("Origin", o);
            }
            assert_eq!(req.send().await.unwrap().status(), 403);
        }
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rejects_non_http_urls() {
        let (base, seen) = start().await;
        let resp = reqwest::Client::new()
            .post(format!("{base}/add"))
            .header("Origin", EXT)
            .body(r#"{"url":"file:///C:/Windows/win.ini"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 422);
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn ping_identifies_app() {
        let (base, _) = start().await;
        let resp = reqwest::Client::new().get(format!("{base}/ping")).header("Origin", EXT).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        assert!(resp.text().await.unwrap().contains("Bright Downloader"));
    }
}
