//! Token-bucket bandwidth limiter shared by any number of connections.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct RateLimiter {
    /// Bytes per second; 0 means unlimited.
    rate: AtomicU64,
    bucket: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            rate: AtomicU64::new(bytes_per_sec),
            bucket: Mutex::new(Bucket { tokens: bytes_per_sec as f64, last: Instant::now() }),
        }
    }

    pub fn unlimited() -> Self {
        Self::new(0)
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Wait until `n` bytes may pass. The bucket may go negative so a large
    /// chunk is never starved; the debt is paid off by sleeping.
    pub async fn acquire(&self, n: u64) {
        let wait = {
            let rate = self.rate();
            if rate == 0 {
                return;
            }
            let mut b = self.bucket.lock().unwrap();
            let now = Instant::now();
            let rate = rate as f64;
            b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate).min(rate);
            b.last = now;
            b.tokens -= n as f64;
            if b.tokens >= 0.0 {
                return;
            }
            Duration::from_secs_f64(-b.tokens / rate)
        };
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_never_waits() {
        let l = RateLimiter::unlimited();
        let t = Instant::now();
        for _ in 0..1000 {
            l.acquire(1 << 20).await;
        }
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn limits_throughput() {
        let l = RateLimiter::new(100_000);
        let t = Instant::now();
        // First 100K is the initial burst, the next 100K must take ~1s.
        for _ in 0..20 {
            l.acquire(10_000).await;
        }
        let e = t.elapsed();
        assert!(e >= Duration::from_millis(900), "too fast: {e:?}");
        assert!(e < Duration::from_millis(1500), "too slow: {e:?}");
    }
}
