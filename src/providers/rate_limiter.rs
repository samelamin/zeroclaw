//! Provider-level rate limiter.
//!
//! Token-bucket rate limiting for LLM API calls with adaptive backoff
//! when providers return 429 (Too Many Requests). Prevents ZeroClaw
//! from overwhelming providers under load.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Token-bucket rate limiter with adaptive backoff.
pub struct ProviderRateLimiter {
    inner: Mutex<RateLimiterInner>,
}

struct RateLimiterInner {
    /// Maximum requests allowed in the time window.
    max_requests: u32,
    /// Time window for the rate limit.
    window: Duration,
    /// Timestamps of recent requests within the current window.
    request_times: Vec<Instant>,
    /// Backoff duration after a 429 response.
    backoff_until: Option<Instant>,
    /// Current backoff multiplier (doubles on each consecutive 429).
    backoff_multiplier: u32,
    /// Base backoff duration.
    base_backoff: Duration,
}

impl ProviderRateLimiter {
    /// Create a new rate limiter.
    ///
    /// # Arguments
    /// * `max_requests` - Maximum requests per window (default: 50)
    /// * `window` - Time window (default: 60s)
    pub fn new(max_requests: u32, window: Duration) -> Self {
        Self {
            inner: Mutex::new(RateLimiterInner {
                max_requests,
                window,
                request_times: Vec::new(),
                backoff_until: None,
                backoff_multiplier: 1,
                base_backoff: Duration::from_secs(5),
            }),
        }
    }

    /// Check if a request is allowed right now.
    /// Returns `Ok(())` if allowed, or `Err(wait_duration)` if rate limited.
    pub fn check(&self) -> Result<(), Duration> {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();

        // Check backoff first
        if let Some(until) = inner.backoff_until {
            if now < until {
                return Err(until - now);
            }
            // Backoff expired, clear it
            inner.backoff_until = None;
        }

        // Prune old timestamps outside the window
        let cutoff = now - inner.window;
        inner.request_times.retain(|t| *t > cutoff);

        // Check if under limit
        if inner.request_times.len() >= inner.max_requests as usize {
            let oldest = inner.request_times[0];
            let wait = inner.window - (now - oldest);
            return Err(wait);
        }

        // Record this request
        inner.request_times.push(now);
        Ok(())
    }

    /// Wait until a request is allowed, then record it.
    pub async fn acquire(&self) {
        loop {
            match self.check() {
                Ok(()) => return,
                Err(wait) => {
                    tracing::debug!(wait_ms = wait.as_millis(), "Rate limiter: waiting");
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// Record a 429 response — triggers adaptive backoff.
    pub fn record_rate_limited(&self) {
        let mut inner = self.inner.lock().unwrap();
        let backoff = inner.base_backoff * inner.backoff_multiplier;
        inner.backoff_until = Some(Instant::now() + backoff);
        inner.backoff_multiplier = (inner.backoff_multiplier * 2).min(32); // cap at 32x
        tracing::warn!(
            backoff_secs = backoff.as_secs(),
            multiplier = inner.backoff_multiplier,
            "Provider returned 429 — backing off"
        );
    }

    /// Record a successful response — resets backoff multiplier.
    pub fn record_success(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.backoff_multiplier = 1;
    }

    /// Get current state for diagnostics.
    pub fn stats(&self) -> RateLimiterStats {
        let inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let cutoff = now - inner.window;
        let active_requests = inner.request_times.iter().filter(|t| **t > cutoff).count() as u32;
        RateLimiterStats {
            active_requests,
            max_requests: inner.max_requests,
            is_backing_off: inner.backoff_until.map_or(false, |u| now < u),
            backoff_multiplier: inner.backoff_multiplier,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RateLimiterStats {
    pub active_requests: u32,
    pub max_requests: u32,
    pub is_backing_off: bool,
    pub backoff_multiplier: u32,
}

impl Default for ProviderRateLimiter {
    fn default() -> Self {
        Self::new(50, Duration::from_secs(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_requests_under_limit() {
        let limiter = ProviderRateLimiter::new(5, Duration::from_secs(60));
        for _ in 0..5 {
            assert!(limiter.check().is_ok());
        }
    }

    #[test]
    fn blocks_over_limit() {
        let limiter = ProviderRateLimiter::new(3, Duration::from_secs(60));
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_ok());
        assert!(limiter.check().is_err());
    }

    #[test]
    fn backoff_on_429() {
        let limiter = ProviderRateLimiter::new(100, Duration::from_secs(60));
        limiter.check().unwrap();
        limiter.record_rate_limited();
        // Should be in backoff
        assert!(limiter.check().is_err());
        assert!(limiter.stats().is_backing_off);
    }

    #[test]
    fn backoff_doubles() {
        let limiter = ProviderRateLimiter::new(100, Duration::from_secs(60));
        limiter.record_rate_limited();
        assert_eq!(limiter.stats().backoff_multiplier, 2);
        limiter.record_rate_limited();
        assert_eq!(limiter.stats().backoff_multiplier, 4);
    }

    #[test]
    fn success_resets_backoff_multiplier() {
        let limiter = ProviderRateLimiter::new(100, Duration::from_secs(60));
        limiter.record_rate_limited();
        limiter.record_rate_limited();
        assert_eq!(limiter.stats().backoff_multiplier, 4);
        limiter.record_success();
        assert_eq!(limiter.stats().backoff_multiplier, 1);
    }

    #[test]
    fn backoff_caps_at_32x() {
        let limiter = ProviderRateLimiter::new(100, Duration::from_secs(60));
        for _ in 0..10 {
            limiter.record_rate_limited();
        }
        assert_eq!(limiter.stats().backoff_multiplier, 32);
    }
}
