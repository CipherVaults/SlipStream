use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{sleep, Instant};

pub(crate) struct Limiter {
    bucket: Mutex<Bucket>,
    changed: watch::Sender<()>,
}

struct Bucket {
    rate: Option<u64>,
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    fn refill(&mut self) {
        let now = Instant::now();
        if let Some(rate) = self.rate {
            let capacity = (rate / 8).max(16_384) as f64;
            self.tokens = (self.tokens
                + now.duration_since(self.updated).as_secs_f64() * rate as f64)
                .min(capacity);
        }
        self.updated = now;
    }
}

impl Limiter {
    pub(crate) fn new(rate: Option<u64>) -> Self {
        Self {
            bucket: Mutex::new(Bucket {
                rate: rate.filter(|rate| *rate > 0),
                tokens: 0.0,
                updated: Instant::now(),
            }),
            changed: watch::channel(()).0,
        }
    }

    pub(crate) fn rate(&self) -> Option<u64> {
        self.bucket.lock().unwrap().rate
    }

    pub(crate) fn set(&self, rate: Option<u64>) {
        let mut bucket = self.bucket.lock().unwrap();
        *bucket = Bucket {
            rate: rate.filter(|rate| *rate > 0),
            tokens: 0.0,
            updated: Instant::now(),
        };
        self.changed.send_replace(());
    }

    /// Debit both limits together so one bucket cannot accumulate a burst while
    /// the other holds a reservation. Callers request at most 16 KiB at a time.
    pub(crate) async fn acquire(&self, transfer: &Self, bytes: usize) {
        let mut global_change = self.changed.subscribe();
        let mut transfer_change = transfer.changed.subscribe();
        let bytes = bytes as f64;
        loop {
            let delay = {
                let mut global = self.bucket.lock().unwrap();
                let mut local = transfer.bucket.lock().unwrap();
                global.refill();
                local.refill();
                let wait = |bucket: &Bucket| {
                    bucket
                        .rate
                        .map_or(0.0, |rate| (bytes - bucket.tokens).max(0.0) / rate as f64)
                };
                let delay = wait(&global).max(wait(&local));
                if delay == 0.0 {
                    for bucket in [&mut global, &mut local] {
                        if bucket.rate.is_some() {
                            bucket.tokens -= bytes;
                        }
                    }
                    return;
                }
                delay
            };
            tokio::select! {
                _ = sleep(Duration::from_secs_f64(delay)) => {}
                _ = global_change.changed() => {}
                _ = transfer_change.changed() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn combined_limits_charge_at_the_slower_rate_with_a_bounded_burst() {
        const KIB: u64 = 1024;
        let global = Limiter::new(Some(128 * KIB));
        let transfer = Limiter::new(Some(256 * KIB));
        let start = Instant::now();
        for _ in 0..16 {
            global.acquire(&transfer, 16 * KIB as usize).await;
        }
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_secs(2));
        assert!(elapsed < Duration::from_millis(2050), "{elapsed:?}");

        transfer.set(Some(64 * KIB));
        let start = Instant::now();
        global.acquire(&transfer, 16 * KIB as usize).await;
        assert_eq!(start.elapsed(), Duration::from_millis(250));

        sleep(Duration::from_secs(10)).await;
        let start = Instant::now();
        global.acquire(&transfer, 16 * KIB as usize).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
        global.acquire(&transfer, 16 * KIB as usize).await;
        assert_eq!(start.elapsed(), Duration::from_millis(250));
    }
}
