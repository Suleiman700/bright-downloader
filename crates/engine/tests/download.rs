//! End-to-end tests against an in-process HTTP server with controllable
//! behaviour (range support, throttling, dropped connections).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use dm_engine::{part_path, state_path, Engine, Error, Job, Options, Outcome, RateLimiter, RequestSpec};
use tokio_util::sync::CancellationToken;

#[derive(Default, Clone)]
struct Behaviour {
    ranges: bool,
    /// Delay per 16 KiB chunk for every request.
    chunk_delay: Option<Duration>,
    /// Extra delay per chunk for requests starting at byte 0 (forces steals).
    slow_from_zero: Option<Duration>,
    /// Number of responses that are cut off after 64 KiB.
    truncate_first: usize,
    status: Option<StatusCode>,
}

struct Server {
    data: Vec<u8>,
    behaviour: Behaviour,
    range_starts: Mutex<Vec<u64>>,
    bytes_served: AtomicU64,
    truncations_left: AtomicUsize,
}

fn test_data(len: usize) -> Vec<u8> {
    // Deterministic, non-repeating-ish content so misplaced bytes are caught.
    let mut x: u32 = 0x1234_5678;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

async fn serve(State(s): State<Arc<Server>>, headers: HeaderMap) -> Response {
    if let Some(status) = s.behaviour.status {
        return Response::builder().status(status).body(Body::empty()).unwrap();
    }
    let total = s.data.len() as u64;
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| {
            let (a, b) = v.split_once('-')?;
            let start: u64 = a.parse().ok()?;
            let end: u64 = if b.is_empty() { total - 1 } else { b.parse().ok()? };
            Some((start, end.min(total - 1)))
        })
        .filter(|_| s.behaviour.ranges);

    let (status, start, end) = match range {
        Some((a, b)) => (StatusCode::PARTIAL_CONTENT, a, b),
        None => (StatusCode::OK, 0, total - 1),
    };
    s.range_starts.lock().unwrap().push(start);

    let truncate = s
        .truncations_left
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok();
    let mut delay = s.behaviour.chunk_delay;
    if start == 0 && range.is_some() && end > 0 {
        delay = s.behaviour.slow_from_zero.or(delay);
    }

    let body_slice = s.data[start as usize..=end as usize].to_vec();
    let server = s.clone();
    let stream = async_stream(body_slice, delay, truncate, server);

    let mut builder = Response::builder()
        .status(status)
        .header(header::CONTENT_LENGTH, end - start + 1)
        .header(header::ETAG, "\"v1\"")
        .header(header::CONTENT_DISPOSITION, "attachment; filename=\"test file.bin\"");
    if s.behaviour.ranges {
        builder = builder.header(header::ACCEPT_RANGES, "bytes");
    }
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{total}"));
    }
    builder.body(stream).unwrap()
}

fn async_stream(data: Vec<u8>, delay: Option<Duration>, truncate: bool, server: Arc<Server>) -> Body {
    let limit = if truncate { 64 * 1024 } else { data.len() };
    let chunks: Vec<Vec<u8>> = data[..limit.min(data.len())].chunks(16 * 1024).map(<[u8]>::to_vec).collect();
    let stream = futures_util::stream::unfold((chunks.into_iter(), truncate), move |(mut it, truncate)| {
        let server = server.clone();
        async move {
            match it.next() {
                Some(c) => {
                    if let Some(d) = delay {
                        tokio::time::sleep(d).await;
                    }
                    server.bytes_served.fetch_add(c.len() as u64, Ordering::Relaxed);
                    Some((Ok::<_, std::io::Error>(c), (it, truncate)))
                }
                None if truncate => Some((Err(std::io::Error::other("cut")), (it, false))),
                None => None,
            }
        }
    });
    Body::from_stream(stream)
}

async fn start_server(data: Vec<u8>, behaviour: Behaviour) -> (SocketAddr, Arc<Server>) {
    let truncations = behaviour.truncate_first;
    let server = Arc::new(Server {
        data,
        behaviour,
        range_starts: Mutex::new(Vec::new()),
        bytes_served: AtomicU64::new(0),
        truncations_left: AtomicUsize::new(truncations),
    });
    let app = Router::new().route("/file", get(serve)).with_state(server.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (addr, server)
}

fn engine(min_split: u64) -> Engine {
    let opts = Options {
        min_split,
        retry_base_delay: Duration::from_millis(20),
        progress_interval: Duration::from_millis(50),
        read_timeout: Duration::from_secs(5),
        ..Options::default()
    };
    Engine::new(opts, Arc::new(RateLimiter::unlimited())).unwrap()
}

fn job(addr: SocketAddr, dir: &std::path::Path, connections: usize) -> Job {
    Job {
        spec: RequestSpec::new(format!("http://{addr}/file")),
        save_path: dir.join("out.bin"),
        connections,
    }
}

#[tokio::test]
async fn multi_connection_download_is_byte_exact() {
    let data = test_data(4 * 1024 * 1024 + 123);
    let (addr, server) = start_server(data.clone(), Behaviour { ranges: true, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let job = job(addr, dir.path(), 8);

    let outcome = engine(64 * 1024).download(&job, None, CancellationToken::new(), |_| {}).await.unwrap();

    assert_eq!(outcome, Outcome::Completed { path: job.save_path.clone(), size: data.len() as u64 });
    assert_eq!(std::fs::read(&job.save_path).unwrap(), data);
    assert!(!part_path(&job.save_path).exists());
    assert!(!state_path(&job.save_path).exists());
    // probe + one request per initial segment at least
    assert!(server.range_starts.lock().unwrap().len() >= 9);
}

#[tokio::test]
async fn idle_connections_steal_from_slow_segment() {
    let data = test_data(2 * 1024 * 1024);
    let behaviour = Behaviour {
        ranges: true,
        chunk_delay: Some(Duration::from_millis(1)),
        slow_from_zero: Some(Duration::from_millis(40)),
        ..Default::default()
    };
    let (addr, server) = start_server(data.clone(), behaviour).await;
    let dir = tempfile::tempdir().unwrap();
    let job = job(addr, dir.path(), 4);

    engine(32 * 1024).download(&job, None, CancellationToken::new(), |_| {}).await.unwrap();

    assert_eq!(std::fs::read(&job.save_path).unwrap(), data);
    let quarter = data.len() as u64 / 4;
    let starts = server.range_starts.lock().unwrap().clone();
    let stolen: Vec<_> = starts.iter().filter(|&&s| s % quarter != 0).collect();
    assert!(!stolen.is_empty(), "expected split requests inside the slow first quarter, got {starts:?}");
    assert!(stolen.iter().all(|&&s| s < quarter), "steals should target the slow segment: {starts:?}");
}

#[tokio::test]
async fn pause_then_resume_continues_without_refetching() {
    let data = test_data(3 * 1024 * 1024);
    let behaviour = Behaviour { ranges: true, chunk_delay: Some(Duration::from_millis(5)), ..Default::default() };
    let (addr, server) = start_server(data.clone(), behaviour).await;
    let dir = tempfile::tempdir().unwrap();
    let job = job(addr, dir.path(), 4);
    let eng = engine(64 * 1024);

    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        c.cancel();
    });
    let last = Arc::new(Mutex::new(0u64));
    let l = last.clone();
    let outcome = eng.download(&job, None, cancel, move |p| *l.lock().unwrap() = p.downloaded).await.unwrap();
    assert_eq!(outcome, Outcome::Paused);
    assert!(state_path(&job.save_path).exists());
    let paused_at = *last.lock().unwrap();
    assert!(paused_at > 0 && paused_at < data.len() as u64, "paused_at={paused_at}");

    let served_before = server.bytes_served.load(Ordering::Relaxed);
    let outcome = eng.download(&job, None, CancellationToken::new(), |_| {}).await.unwrap();
    assert!(matches!(outcome, Outcome::Completed { .. }));
    assert_eq!(std::fs::read(&job.save_path).unwrap(), data);

    let served_after = server.bytes_served.load(Ordering::Relaxed) - served_before;
    assert!(
        served_after < data.len() as u64,
        "resume should not re-download everything: served {served_after} of {}",
        data.len()
    );
}

#[tokio::test]
async fn resumes_download_saved_under_legacy_temp_names() {
    let data = test_data(3 * 1024 * 1024);
    let behaviour = Behaviour { ranges: true, chunk_delay: Some(Duration::from_millis(5)), ..Default::default() };
    let (addr, server) = start_server(data.clone(), behaviour).await;
    let dir = tempfile::tempdir().unwrap();
    let job = job(addr, dir.path(), 4);
    let eng = engine(64 * 1024);

    let cancel = CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        c.cancel();
    });
    assert_eq!(eng.download(&job, None, cancel, |_| {}).await.unwrap(), Outcome::Paused);

    // Simulate a download paused by an older build.
    let [part, state, legacy_part, legacy_state] = dm_engine::temp_paths(&job.save_path);
    std::fs::rename(&part, &legacy_part).unwrap();
    std::fs::rename(&state, &legacy_state).unwrap();

    let served_before = server.bytes_served.load(Ordering::Relaxed);
    let outcome = eng.download(&job, None, CancellationToken::new(), |_| {}).await.unwrap();
    assert!(matches!(outcome, Outcome::Completed { .. }));
    assert_eq!(std::fs::read(&job.save_path).unwrap(), data);
    assert!(server.bytes_served.load(Ordering::Relaxed) - served_before < data.len() as u64, "progress was kept");
    for temp in dm_engine::temp_paths(&job.save_path) {
        assert!(!temp.exists(), "{} left behind", temp.display());
    }
}

#[tokio::test]
async fn server_without_ranges_uses_single_connection() {
    let data = test_data(1024 * 1024 + 7);
    let (addr, server) = start_server(data.clone(), Behaviour { ranges: false, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let job = job(addr, dir.path(), 8);

    engine(64 * 1024).download(&job, None, CancellationToken::new(), |_| {}).await.unwrap();

    assert_eq!(std::fs::read(&job.save_path).unwrap(), data);
    assert_eq!(server.range_starts.lock().unwrap().len(), 2, "probe + one full GET");
}

#[tokio::test]
async fn dropped_connections_are_retried() {
    let data = test_data(2 * 1024 * 1024);
    let behaviour = Behaviour { ranges: true, truncate_first: 4, ..Default::default() };
    let (addr, _server) = start_server(data.clone(), behaviour).await;
    let dir = tempfile::tempdir().unwrap();
    let job = job(addr, dir.path(), 4);

    engine(64 * 1024).download(&job, None, CancellationToken::new(), |_| {}).await.unwrap();

    assert_eq!(std::fs::read(&job.save_path).unwrap(), data);
}

#[tokio::test]
async fn not_found_fails_fast() {
    let behaviour = Behaviour { status: Some(StatusCode::NOT_FOUND), ..Default::default() };
    let (addr, _server) = start_server(test_data(10), behaviour).await;
    let dir = tempfile::tempdir().unwrap();
    let job = job(addr, dir.path(), 4);

    let err = engine(64 * 1024).download(&job, None, CancellationToken::new(), |_| {}).await.unwrap_err();
    assert!(matches!(err, Error::Status(404)), "{err:?}");
}

#[tokio::test]
async fn per_download_limiter_caps_speed() {
    let data = test_data(600 * 1024);
    let (addr, _server) = start_server(data.clone(), Behaviour { ranges: true, ..Default::default() }).await;
    let dir = tempfile::tempdir().unwrap();
    let job = job(addr, dir.path(), 4);

    let limiter = Arc::new(RateLimiter::new(300 * 1024));
    let t = std::time::Instant::now();
    engine(64 * 1024).download(&job, Some(limiter), CancellationToken::new(), |_| {}).await.unwrap();
    // 300K burst + 300K at 300K/s ≈ 1s.
    assert!(t.elapsed() >= Duration::from_millis(800), "{:?}", t.elapsed());
    assert_eq!(std::fs::read(&job.save_path).unwrap(), data);
}

#[tokio::test]
async fn requests_go_through_the_configured_proxy() {
    // The test server doubles as an HTTP proxy: it answers the absolute-form
    // request for a host that does not exist, so success proves the proxy was used.
    let (addr, _server) = start_server(test_data(64 * 1024), Behaviour { ranges: true, ..Default::default() }).await;
    let opts = Options { proxy: Some(format!("http://{addr}")), ..Options::default() };
    let engine = Engine::new(opts, Arc::new(RateLimiter::unlimited())).unwrap();
    let info = dm_engine::probe(engine.client(), &RequestSpec::new("http://no-such-host.invalid/file")).await.unwrap();
    assert_eq!(info.total_size, Some(64 * 1024));

    let bad = Options { proxy: Some("not a proxy".into()), ..Options::default() };
    assert!(matches!(Engine::new(bad, Arc::new(RateLimiter::unlimited())), Err(Error::Proxy(_))));
}
