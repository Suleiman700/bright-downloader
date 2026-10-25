//! FTP downloads against a tiny in-process FTP server (USER/PASS, TYPE,
//! SIZE, MDTM, EPSV, REST, RETR).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dm_engine::{probe, Engine, Job, Options, Outcome, RateLimiter, RequestSpec};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const LEN: usize = 3 * 1024 * 1024;

fn data() -> Vec<u8> {
    (0..LEN).map(|i| (i * 31 % 253) as u8).collect()
}

struct Server {
    data: Vec<u8>,
    sessions: AtomicUsize,
    /// Pause between 32 KiB chunks so tests can stop half way.
    delay: Duration,
}

async fn session(server: Arc<Server>, sock: tokio::net::TcpStream) {
    server.sessions.fetch_add(1, Ordering::SeqCst);
    let (r, mut w) = sock.into_split();
    let mut r = BufReader::new(r);
    w.write_all(b"220-Welcome\r\n220 Test FTP ready\r\n").await.unwrap();
    let mut rest = 0usize;
    let mut passive: Option<TcpListener> = None;
    let mut authed = false;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await.unwrap_or(0) == 0 {
            return;
        }
        let (cmd, arg) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
        let reply = match cmd {
            "USER" => "331 Password please".to_string(),
            "PASS" if arg == "secret" => {
                authed = true;
                "230 Logged in".into()
            }
            "PASS" => "530 Login incorrect".into(),
            _ if !authed => "530 Please login".into(),
            "TYPE" => "200 Binary".into(),
            "SIZE" if arg == "/pub/big.bin" => format!("213 {}", server.data.len()),
            "MDTM" if arg == "/pub/big.bin" => "213 20260101120000".into(),
            "SIZE" | "MDTM" => "550 No such file".into(),
            "REST" => {
                rest = arg.parse().unwrap();
                "350 Restarting".into()
            }
            "EPSV" => {
                let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let port = l.local_addr().unwrap().port();
                passive = Some(l);
                format!("229 Entering Extended Passive Mode (|||{port}|)")
            }
            "RETR" if arg != "/pub/big.bin" => "550 No such file".into(),
            "RETR" => {
                let l = passive.take().unwrap();
                w.write_all(b"150 Opening data connection\r\n").await.unwrap();
                let (mut d, _) = l.accept().await.unwrap();
                for chunk in server.data[rest..].chunks(32 * 1024) {
                    if !server.delay.is_zero() {
                        tokio::time::sleep(server.delay).await;
                    }
                    if d.write_all(chunk).await.is_err() {
                        break; // client stopped reading (segment was split)
                    }
                }
                let _ = d.shutdown().await;
                rest = 0;
                "226 Transfer complete".into()
            }
            "QUIT" => {
                let _ = w.write_all(b"221 Bye\r\n").await;
                return;
            }
            _ => "502 Not implemented".into(),
        };
        if w.write_all(format!("{reply}\r\n").as_bytes()).await.is_err() {
            return;
        }
    }
}

async fn start(delay: Duration) -> (String, Arc<Server>) {
    let server = Arc::new(Server { data: data(), sessions: AtomicUsize::new(0), delay });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let s = server.clone();
    tokio::spawn(async move {
        loop {
            let (sock, _) = listener.accept().await.unwrap();
            tokio::spawn(session(s.clone(), sock));
        }
    });
    (format!("ftp://user:secret@127.0.0.1:{port}"), server)
}

fn engine() -> Engine {
    let opts = Options { progress_interval: Duration::from_millis(50), min_split: 128 * 1024, ..Options::default() };
    Engine::new(opts, Arc::new(RateLimiter::unlimited())).unwrap()
}

#[tokio::test]
async fn probes_size_resume_and_date() {
    let (base, _) = start(Duration::ZERO).await;
    let info = probe(engine().client(), &RequestSpec::new(format!("{base}/pub/big.bin"))).await.unwrap();
    assert_eq!(info.total_size, Some(LEN as u64));
    assert!(info.accepts_ranges);
    assert_eq!(info.file_name, "big.bin");
    assert_eq!(info.last_modified.as_deref(), Some("20260101120000"));
}

#[tokio::test]
async fn multi_connection_ftp_download_is_byte_exact() {
    let (base, server) = start(Duration::ZERO).await;
    let dir = tempfile::tempdir().unwrap();
    let job = Job { spec: RequestSpec::new(format!("{base}/pub/big.bin")), save_path: dir.path().join("big.bin"), connections: 4 };
    let out = engine().download(&job, None, CancellationToken::new(), |_| {}).await.unwrap();
    assert!(matches!(out, Outcome::Completed { .. }));
    assert_eq!(std::fs::read(&job.save_path).unwrap(), data());
    assert!(server.sessions.load(Ordering::SeqCst) >= 4, "one session per connection plus the probe");
}

#[tokio::test]
async fn ftp_pause_and_resume() {
    let (base, _) = start(Duration::from_millis(15)).await;
    let dir = tempfile::tempdir().unwrap();
    let job = Job { spec: RequestSpec::new(format!("{base}/pub/big.bin")), save_path: dir.path().join("big.bin"), connections: 3 };
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        stopper.cancel();
    });
    let last = std::sync::Mutex::new(0);
    let out = engine().download(&job, None, cancel, |p| *last.lock().unwrap() = p.downloaded).await.unwrap();
    assert_eq!(out, Outcome::Paused);
    let done = *last.lock().unwrap();
    assert!(done > 0 && done < LEN as u64, "{done}");

    let out = engine().download(&job, None, CancellationToken::new(), |_| {}).await.unwrap();
    assert!(matches!(out, Outcome::Completed { .. }));
    assert_eq!(std::fs::read(&job.save_path).unwrap(), data());
}

#[tokio::test]
async fn ftp_errors_are_reported() {
    let (base, _) = start(Duration::ZERO).await;
    let e = engine();
    let wrong = base.replace("secret", "nope");
    let err = probe(e.client(), &RequestSpec::new(format!("{wrong}/pub/big.bin"))).await.unwrap_err();
    assert!(err.to_string().contains("Login incorrect"), "{err}");
    assert!(!err.is_retryable());
    let err = probe(e.client(), &RequestSpec::new(format!("{base}/pub/missing.bin"))).await.unwrap_err();
    assert!(matches!(err, dm_engine::Error::Status(404)), "{err}");
}
