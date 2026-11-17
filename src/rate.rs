//! Rolling-window politeness limiter for SOURCE fetches (county GIS, SOS
//! endpoints...). The coordinator hands out a per-contract budget; the
//! client also enforces its own `LH_RATE_LIMIT` cap when set — the effective
//! budget is the min of both. Coordinator API calls are NOT rate limited by
//! this (they hit our own Worker).
//!
//! Model: token-free rolling windows. `acquire()` blocks until a request
//! slot is free (per requests/min), `record_bytes()` accounts transferred
//! bytes and paces subsequent requests against bytes/min. Byte pacing
//! happens after the fact (a response already in flight cannot be un-sent),
//! which is the standard tradeoff for politeness limiters.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    requests_per_min: u64,
    bytes_per_min: u64,
    state: Mutex<Windows>,
}

#[derive(Default)]
struct Windows {
    request_times: VecDeque<Instant>,
    byte_events: VecDeque<(Instant, u64)>,
}

const WINDOW: Duration = Duration::from_secs(60);

impl RateLimiter {
    pub fn new(requests_per_min: u64, bytes_per_min: u64) -> RateLimiter {
        RateLimiter {
            requests_per_min: requests_per_min.max(1),
            bytes_per_min: bytes_per_min.max(1),
            state: Mutex::new(Windows::default()),
        }
    }

    /// Effective budget = min of contract budget and client cap.
    pub fn combined(contract: (u64, u64), client_cap: Option<(u64, u64)>) -> RateLimiter {
        match client_cap {
            Some((cap_rpm, cap_bpm)) => {
                RateLimiter::new(contract.0.min(cap_rpm), contract.1.min(cap_bpm))
            }
            None => RateLimiter::new(contract.0, contract.1),
        }
    }

    pub fn requests_per_min(&self) -> u64 {
        self.requests_per_min
    }

    pub fn bytes_per_min(&self) -> u64 {
        self.bytes_per_min
    }

    /// Block until a request slot is free, then take it.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut state = self.state.lock().expect("rate limiter lock");
                prune(&mut state.request_times, Instant::now());
                if (state.request_times.len() as u64) < self.requests_per_min {
                    state.request_times.push_back(Instant::now());
                    None
                } else {
                    // Oldest request in the window + 60s is when a slot frees.
                    let oldest = state
                        .request_times
                        .front()
                        .copied()
                        .unwrap_or_else(Instant::now);
                    Some(oldest + WINDOW - Instant::now())
                }
            };
            match wait {
                None => return,
                Some(d) if d <= Duration::ZERO => {
                    // Window boundary passed between prune and push; retry.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Some(d) => tokio::time::sleep(d.min(Duration::from_secs(5))).await,
            }
        }
    }

    /// Account transferred bytes; sleeps inline if the bytes/min budget is
    /// exhausted so subsequent requests pace themselves.
    pub async fn record_bytes(&self, bytes: u64) {
        loop {
            let wait = {
                let mut state = self.state.lock().expect("rate limiter lock");
                let now = Instant::now();
                state
                    .byte_events
                    .retain(|(t, _)| now.duration_since(*t) < WINDOW);
                let used: u64 = state.byte_events.iter().map(|(_, b)| b).sum();
                if used + bytes <= self.bytes_per_min || used == 0 {
                    // `used == 0` lets a single oversized response through
                    // (never deadlocks on a budget smaller than one page).
                    state.byte_events.push_back((now, bytes));
                    None
                } else {
                    let budget = self.bytes_per_min;
                    // Time until enough old bytes drain out of the window.
                    let mut acc = used + bytes - budget;
                    let mut target = now;
                    for (t, b) in &state.byte_events {
                        if acc == 0 {
                            break;
                        }
                        acc = acc.saturating_sub(*b);
                        target = *t;
                    }
                    Some((target + WINDOW - now).max(Duration::ZERO))
                }
            };
            match wait {
                None => return,
                Some(d) if d <= Duration::ZERO => return,
                Some(d) => tokio::time::sleep(d.min(Duration::from_secs(5))).await,
            }
        }
    }
}

fn prune(times: &mut VecDeque<Instant>, now: Instant) {
    times.retain(|t| now.duration_since(*t) < WINDOW);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn acquire_is_cheap_under_budget() {
        let rl = RateLimiter::new(1000, 100_000_000);
        let start = Instant::now();
        for _ in 0..10 {
            rl.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn record_bytes_within_budget_does_not_sleep() {
        let rl = RateLimiter::new(1000, 10_000_000);
        let start = Instant::now();
        rl.record_bytes(1000).await;
        assert!(start.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn combined_takes_minimum() {
        let rl = RateLimiter::combined((120, 5_000_000), Some((60, 8_000_000)));
        assert_eq!(rl.requests_per_min(), 60);
        assert_eq!(rl.bytes_per_min(), 5_000_000);
        let rl2 = RateLimiter::combined((120, 5_000_000), None);
        assert_eq!(rl2.requests_per_min(), 120);
    }
}
