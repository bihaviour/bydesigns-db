//! A compact, dependency-free latency histogram with high dynamic range.
//!
//! The benchmark plan (spec 09) mandates percentile reporting (p50/p99/p999) via
//! an HDR-style histogram and forbids mean-only latency, because "the mean hides
//! the tail that decides this architecture." This is a small log-bucketed
//! histogram in that spirit: values land in exponential buckets sized by a fixed
//! ratio (default 2%), so any recorded value is represented within that relative
//! error using a bounded number of counters (~940 for 1µs..120s) regardless of
//! sample count. That bounded footprint is the whole point — it lets a long run
//! keep an exact-enough tail without retaining every sample.

/// Default bucket ratio: each bucket spans +2%, bounding the relative error of
/// any reported percentile to ~2% — adequate for separating p50 from a p999 tail
/// that is orders of magnitude larger, which is what spec 09 cares about.
const DEFAULT_RATIO: f64 = 1.02;

/// A log-bucketed histogram over `u64` values (latencies in microseconds).
#[derive(Clone)]
pub struct Histogram {
    log_base: f64,
    buckets: Vec<u64>,
    count: u64,
    min: u64,
    max: u64,
    sum: u128,
}

impl Histogram {
    /// A histogram with the default ~2% bucket resolution.
    pub fn new() -> Histogram {
        Histogram::with_ratio(DEFAULT_RATIO)
    }

    /// A histogram whose buckets each span `ratio` (e.g. 1.02 = 2% resolution).
    pub fn with_ratio(ratio: f64) -> Histogram {
        assert!(ratio > 1.0, "bucket ratio must be > 1.0");
        Histogram {
            log_base: ratio.ln(),
            // Pre-size for 1µs..~120s at the default ratio; grows if exceeded.
            buckets: vec![0; 1024],
            count: 0,
            min: u64::MAX,
            max: 0,
            sum: 0,
        }
    }

    #[inline]
    fn index(&self, v: u64) -> usize {
        if v <= 1 {
            0
        } else {
            ((v as f64).ln() / self.log_base) as usize
        }
    }

    /// The representative value at the center of bucket `idx` (geometric mid).
    fn representative(&self, idx: usize) -> u64 {
        (self.log_base * (idx as f64 + 0.5)).exp().round() as u64
    }

    /// Record one sample (clamped to >= 1 so a 0µs reading still counts).
    pub fn record(&mut self, value: u64) {
        let v = value.max(1);
        let idx = self.index(v);
        if idx >= self.buckets.len() {
            self.buckets.resize(idx + 1, 0);
        }
        self.buckets[idx] += 1;
        self.count += 1;
        self.sum += v as u128;
        self.min = self.min.min(v);
        self.max = self.max.max(v);
    }

    /// Fold another histogram into this one (same ratio assumed).
    pub fn merge(&mut self, other: &Histogram) {
        if other.count == 0 {
            return;
        }
        if other.buckets.len() > self.buckets.len() {
            self.buckets.resize(other.buckets.len(), 0);
        }
        for (i, c) in other.buckets.iter().enumerate() {
            self.buckets[i] += c;
        }
        self.count += other.count;
        self.sum += other.sum;
        self.min = self.min.min(other.min);
        self.max = self.max.max(other.max);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn min(&self) -> u64 {
        if self.count == 0 {
            0
        } else {
            self.min
        }
    }

    pub fn max(&self) -> u64 {
        self.max
    }

    pub fn mean(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum as f64 / self.count as f64
        }
    }

    /// The value at quantile `q` in `[0, 1]`, clamped to the observed range.
    pub fn value_at_quantile(&self, q: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let q = q.clamp(0.0, 1.0);
        // Rank of the target sample (1-based).
        let target = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut cumulative = 0u64;
        for (idx, &c) in self.buckets.iter().enumerate() {
            cumulative += c;
            if cumulative >= target {
                return self.representative(idx).clamp(self.min, self.max);
            }
        }
        self.max
    }
}

impl Default for Histogram {
    fn default() -> Histogram {
        Histogram::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_is_zero() {
        let h = Histogram::new();
        assert_eq!(h.count(), 0);
        assert_eq!(h.value_at_quantile(0.5), 0);
        assert_eq!(h.mean(), 0.0);
    }

    #[test]
    fn quantiles_land_within_bucket_resolution() {
        // A uniform 1..=10_000 distribution: the q-th percentile should be ~q*N,
        // within the histogram's ~2% relative bucket error.
        let mut h = Histogram::new();
        for v in 1..=10_000u64 {
            h.record(v);
        }
        assert_eq!(h.count(), 10_000);
        for (q, expected) in [(0.50, 5_000.0), (0.90, 9_000.0), (0.99, 9_900.0)] {
            let got = h.value_at_quantile(q) as f64;
            let err = (got - expected).abs() / expected;
            assert!(
                err <= 0.03,
                "p{}: got {got}, expected ~{expected} ({:.1}% off)",
                (q * 100.0) as u32,
                err * 100.0,
            );
        }
        assert!(h.value_at_quantile(1.0) >= 9_900);
        assert!(h.min() >= 1 && h.max() <= 10_000);
    }

    #[test]
    fn merge_combines_distributions() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        for v in 1..=1_000u64 {
            a.record(v);
        }
        for v in 1_001..=2_000u64 {
            b.record(v);
        }
        a.merge(&b);
        assert_eq!(a.count(), 2_000);
        assert_eq!(a.min(), 1);
        assert!(a.max() >= 1_980);
        let median = a.value_at_quantile(0.5) as f64;
        assert!((median - 1_000.0).abs() / 1_000.0 <= 0.03);
    }
}
