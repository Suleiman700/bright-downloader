//! HLS downloads against an in-process server: variant choice, AES-128,
//! pause/resume and live recording.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use dm_engine::{hls, Engine, Job, Options, Outcome, RateLimiter, RequestSpec};
use tokio_util::sync::CancellationToken;

const SEGMENTS: u64 = 12;
const KEY: [u8; 16] = *b"0123456789abcdef";

fn segment(n: u64) -> Vec<u8> {
    (0..20_000u64).map(|i| ((i * 7 + n * 13) % 251) as u8).collect()
}

struct Site {
    /// Delay per segment, so tests can stop half way.
    delay: Duration,
    /// Seconds since start → how many segments the live playlist offers.
    started: Instant,
    served: AtomicU64,
}

fn iv(n: u64) -> [u8; 16] {
    (n as u128).to_be_bytes()
}

async fn route(State(site): State<Arc<Site>>, Path(file): Path<String>) -> Response {
    match file.as_str() {
        "master.m3u8" => "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=300000,RESOLUTION=320x180\nlow.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\nhigh.m3u8\n"
            .into_response(),
        "high.m3u8" | "low.m3u8" => {
            let mut text = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-KEY:METHOD=AES-128,URI=\"key\"\n");
            for n in 0..SEGMENTS {
                text += &format!("#EXTINF:2.0,\nseg{n}.ts?q={}\n", &file[..file.len() - 5]);
            }
            text += "#EXT-X-ENDLIST\n";
            text.into_response()
        }
        "live.m3u8" => {
            // A sliding window of 3 segments, one new segment every 300 ms.
            let newest = (site.started.elapsed().as_millis() / 300) as u64;
            let first = newest.saturating_sub(2);
            let mut text = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
            for n in first..=newest {
                text += &format!("#EXTINF:1.0,\nplain{n}.ts\n");
            }
            text.into_response()
        }
        "key" => KEY.to_vec().into_response(),
        name => {
            let (plain, rest) = match name.strip_prefix("plain") {
                Some(r) => (true, r),
                None => (false, name.strip_prefix("seg").unwrap_or("x")),
            };
            let Some(n) = rest.strip_suffix(".ts").and_then(|n| n.parse::<u64>().ok()) else {
                return StatusCode::NOT_FOUND.into_response();
            };
            tokio::time::sleep(site.delay).await;
            site.served.fetch_add(1, Ordering::SeqCst);
            let data = segment(n);
            if plain {
                return data.into_response();
            }
            cbc::Encryptor::<aes::Aes128>::new(&KEY.into(), &iv(n).into())
                .encrypt_padded_vec_mut::<Pkcs7>(&data)
                .into_response()
        }
    }
}

async fn start(delay: Duration) -> (String, Arc<Site>) {
    let site = Arc::new(Site { delay, started: Instant::now(), served: AtomicU64::new(0) });
    let app = Router::new().route("/{file}", get(route)).with_state(site.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), site)
}

fn engine() -> Engine {
    let opts = Options { progress_interval: Duration::from_millis(50), ..Options::default() };
    Engine::new(opts, Arc::new(RateLimiter::unlimited())).unwrap()
}

fn expected(range: std::ops::Range<u64>) -> Vec<u8> {
    range.flat_map(segment).collect()
}

#[tokio::test]
async fn inspect_lists_qualities() {
    let (base, _) = start(Duration::ZERO).await;
    let info = hls::inspect(engine().client(), &RequestSpec::new(format!("{base}/master.m3u8"))).await.unwrap();
    assert_eq!(info.variants.len(), 2);
    assert_eq!(info.variants[0].label, "720p · 2.0 Mbps", "best first");
    assert!(!info.live);
    assert_eq!(info.duration, Some(24.0));
    assert_eq!(info.estimated_size, Some(6_000_000));
}

#[tokio::test]
async fn downloads_encrypted_stream_of_chosen_quality() {
    let (base, _) = start(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let job = Job {
        spec: RequestSpec::new(format!("{base}/master.m3u8")),
        save_path: dir.path().join("video.ts"),
        connections: 4,
    };
    let low = format!("{base}/low.m3u8");
    let out = engine().download_stream(&job, Some(&low), None, CancellationToken::new(), |_| {}).await.unwrap();
    assert!(matches!(out, Outcome::Completed { .. }));
    assert_eq!(std::fs::read(&job.save_path).unwrap(), expected(0..SEGMENTS));
    assert!(!hls::segments_dir(&job.save_path).exists(), "temporary segments are removed");
}

#[tokio::test]
async fn pause_and_resume_skip_finished_segments() {
    let (base, site) = start(Duration::from_millis(60)).await;
    let dir = tempfile::tempdir().unwrap();
    let job = Job {
        spec: RequestSpec::new(format!("{base}/high.m3u8")),
        save_path: dir.path().join("show.ts"),
        connections: 2,
    };
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        stopper.cancel();
    });
    let out = engine().download_stream(&job, None, None, cancel, |_| {}).await.unwrap();
    assert_eq!(out, Outcome::Paused);
    let before = site.served.load(Ordering::SeqCst);
    assert!(before > 0 && before < SEGMENTS, "{before}");

    let out = engine().download_stream(&job, None, None, CancellationToken::new(), |_| {}).await.unwrap();
    assert!(matches!(out, Outcome::Completed { .. }));
    assert_eq!(std::fs::read(&job.save_path).unwrap(), expected(0..SEGMENTS));
    // At most the segments in flight when stopping are fetched twice.
    assert!(site.served.load(Ordering::SeqCst) <= SEGMENTS + 2);
}

#[tokio::test]
async fn live_stream_records_until_stopped() {
    let (base, _) = start(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let job = Job {
        spec: RequestSpec::new(format!("{base}/live.m3u8")),
        save_path: dir.path().join("live.ts"),
        connections: 2,
    };
    let info = hls::inspect(engine().client(), &job.spec).await.unwrap();
    assert!(info.live);

    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1600)).await;
        stopper.cancel();
    });
    let out = engine().download_stream(&job, None, None, cancel, |_| {}).await.unwrap();
    let Outcome::Completed { size, .. } = out else { panic!("live recording should be saved when stopped") };
    let data = std::fs::read(&job.save_path).unwrap();
    assert_eq!(data.len() as u64, size);
    // Consecutive segments starting at the first one seen, without gaps or repeats.
    let count = data.len() as u64 / 20_000;
    assert!(count >= 4, "recorded only {count} segments");
    let first = (0..10).find(|f| data[..20_000] == segment(*f)[..]).expect("starts with a known segment");
    assert_eq!(data, expected(first..first + count));
}

async fn mpd(State(_): State<Arc<Site>>) -> Response {
    let mut segs = String::new();
    segs.push_str(r#"<S t="0" d="2" r="11"/>"#);
    format!(
        r#"<?xml version="1.0"?><MPD type="static" mediaPresentationDuration="PT24S"><Period><AdaptationSet mimeType="video/mp4">
        <SegmentTemplate timescale="1" media="plain$Number$.ts" startNumber="0"><SegmentTimeline>{segs}</SegmentTimeline></SegmentTemplate>
        <Representation id="hi" bandwidth="2000000" height="720"/><Representation id="lo" bandwidth="300000" height="180"/>
        </AdaptationSet></Period></MPD>"#
    )
    .into_response()
}

#[tokio::test]
async fn downloads_dash_manifest() {
    let site = Arc::new(Site { delay: Duration::ZERO, started: Instant::now(), served: AtomicU64::new(0) });
    let app = Router::new().route("/show.mpd", get(mpd)).route("/{file}", get(route)).with_state(site);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let spec = RequestSpec::new(format!("http://{addr}/show.mpd"));

    let info = hls::inspect(engine().client(), &spec).await.unwrap();
    assert_eq!(info.format, "dash");
    assert_eq!(info.variants.iter().map(|v| v.url.as_str()).collect::<Vec<_>>(), ["hi", "lo"]);
    assert_eq!(info.duration, Some(24.0));

    let dir = tempfile::tempdir().unwrap();
    let job = Job { spec, save_path: dir.path().join("show.ts"), connections: 4 };
    let out = engine().download_stream(&job, Some("lo"), None, CancellationToken::new(), |_| {}).await.unwrap();
    assert!(matches!(out, Outcome::Completed { .. }));
    assert_eq!(std::fs::read(&job.save_path).unwrap(), expected(0..SEGMENTS));
}
