//! Prompt cache break detection.
//!
//! Tracks cache_read_input_tokens and cache_creation_input_tokens across
//! LLM calls to detect when the prompt cache breaks. Logs warnings when
//! a cache break is detected so operators can diagnose cost spikes.
//!
//! Following Claude Code's promptCacheBreakDetection.ts pattern.

use std::sync::Mutex;

/// Tracks prompt cache hit/miss patterns across LLM calls.
pub struct CacheTracker {
    inner: Mutex<CacheTrackerInner>,
}

struct CacheTrackerInner {
    /// Previous turn's cache_read tokens (baseline for comparison).
    prev_cache_read: Option<u64>,
    /// Previous turn's total input tokens.
    prev_input_tokens: Option<u64>,
    /// Number of consecutive cache hits.
    consecutive_hits: u32,
    /// Number of cache breaks detected this session.
    total_breaks: u32,
    /// Number of total LLM calls tracked.
    total_calls: u32,
}

/// Result of analyzing a cache event.
#[derive(Debug, Clone)]
pub struct CacheAnalysis {
    pub is_cache_break: bool,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub hit_rate_percent: f64,
    pub consecutive_hits: u32,
    pub total_breaks: u32,
}

impl CacheTracker {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(CacheTrackerInner {
                prev_cache_read: None,
                prev_input_tokens: None,
                consecutive_hits: 0,
                total_breaks: 0,
                total_calls: 0,
            }),
        }
    }

    /// Record token usage from an LLM call and detect cache breaks.
    ///
    /// A cache break is detected when:
    /// - cache_read_tokens drops by >5% compared to previous turn, OR
    /// - cache_read_tokens drops by >2000 absolute tokens
    /// AND we had a previous baseline to compare against.
    pub fn record(
        &self,
        input_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    ) -> CacheAnalysis {
        let mut inner = self.inner.lock().unwrap();
        inner.total_calls += 1;

        let is_cache_break = if let Some(prev_read) = inner.prev_cache_read {
            let drop = prev_read.saturating_sub(cache_read_tokens);
            let pct_drop = if prev_read > 0 {
                (drop as f64 / prev_read as f64) * 100.0
            } else {
                0.0
            };
            // Cache break: >5% drop OR >2000 token absolute drop
            drop > 2000 || pct_drop > 5.0
        } else {
            false // first call, no baseline
        };

        if is_cache_break {
            inner.total_breaks += 1;
            inner.consecutive_hits = 0;
            tracing::warn!(
                cache_read = cache_read_tokens,
                cache_creation = cache_creation_tokens,
                prev_cache_read = inner.prev_cache_read,
                total_breaks = inner.total_breaks,
                "Prompt cache break detected — cache read tokens dropped significantly"
            );
        } else if inner.prev_cache_read.is_some() {
            inner.consecutive_hits += 1;
        }

        let hit_rate = if inner.total_calls > 1 {
            let hits = inner.total_calls - 1 - inner.total_breaks;
            (hits as f64 / (inner.total_calls - 1) as f64) * 100.0
        } else {
            100.0
        };

        // Update baseline
        inner.prev_cache_read = Some(cache_read_tokens);
        inner.prev_input_tokens = Some(input_tokens);

        CacheAnalysis {
            is_cache_break,
            cache_read_tokens,
            cache_creation_tokens,
            hit_rate_percent: hit_rate,
            consecutive_hits: inner.consecutive_hits,
            total_breaks: inner.total_breaks,
        }
    }

    /// Get the current cache hit rate percentage.
    pub fn hit_rate(&self) -> f64 {
        let inner = self.inner.lock().unwrap();
        if inner.total_calls > 1 {
            let hits = inner.total_calls - 1 - inner.total_breaks;
            (hits as f64 / (inner.total_calls - 1) as f64) * 100.0
        } else {
            100.0
        }
    }

    /// Reset tracking state (call after compaction or /clear).
    pub fn reset(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.prev_cache_read = None;
        inner.prev_input_tokens = None;
        inner.consecutive_hits = 0;
        // Keep total_breaks and total_calls for session-level stats
    }
}

impl Default for CacheTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_call_no_break() {
        let tracker = CacheTracker::new();
        let result = tracker.record(1000, 800, 200);
        assert!(!result.is_cache_break);
    }

    #[test]
    fn stable_cache_no_break() {
        let tracker = CacheTracker::new();
        tracker.record(1000, 800, 200);
        let result = tracker.record(1000, 790, 210);
        assert!(!result.is_cache_break);
        assert_eq!(result.consecutive_hits, 1);
    }

    #[test]
    fn large_drop_triggers_break() {
        let tracker = CacheTracker::new();
        tracker.record(10000, 8000, 2000);
        let result = tracker.record(10000, 100, 9900);
        assert!(result.is_cache_break);
        assert_eq!(result.total_breaks, 1);
    }

    #[test]
    fn absolute_drop_triggers_break() {
        let tracker = CacheTracker::new();
        tracker.record(5000, 4000, 1000);
        let result = tracker.record(5000, 1500, 3500);
        assert!(result.is_cache_break);
    }

    #[test]
    fn reset_clears_baseline() {
        let tracker = CacheTracker::new();
        tracker.record(1000, 800, 200);
        tracker.reset();
        let result = tracker.record(1000, 100, 900);
        assert!(!result.is_cache_break); // no baseline after reset
    }

    #[test]
    fn hit_rate_calculation() {
        let tracker = CacheTracker::new();
        tracker.record(1000, 800, 200); // baseline
        tracker.record(1000, 790, 210); // hit
        tracker.record(1000, 100, 900); // break
        tracker.record(1000, 780, 220); // hit
        // 2 hits, 1 break out of 3 comparisons = 66.67%
        let rate = tracker.hit_rate();
        assert!((rate - 66.67).abs() < 1.0);
    }
}
