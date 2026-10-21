use std::collections::BTreeMap;

/// Values at or below this are counted as zero; log buckets cannot hold them.
const ZERO_THRESHOLD: f64 = 1e-9;

/// Bounded-memory quantile estimator (DDSketch with logarithmic buckets).
///
/// Runs can last days at 1 Hz, so keeping every value per unit is not an option.
/// Each estimate is within `relative_accuracy` of a value whose rank matches the
/// quantile; `q = 0` and `q = 1` return the exact min and max. Negative inputs are
/// treated as zero.
#[derive(Debug, Clone, PartialEq)]
pub struct QuantileSketch {
    gamma_ln: f64,
    gamma: f64,
    bins: BTreeMap<i32, u64>,
    zeros: u64,
    count: u64,
    min: f64,
    max: f64,
}

impl Default for QuantileSketch {
    /// Uses 1% relative accuracy: about 1300 buckets cover 1 byte to 100 GiB.
    fn default() -> Self {
        Self::new(0.01)
    }
}

impl QuantileSketch {
    /// Creates an empty sketch. `relative_accuracy` is clamped to `[1e-4, 0.5]`.
    pub fn new(relative_accuracy: f64) -> Self {
        let alpha = relative_accuracy.clamp(1e-4, 0.5);
        let gamma = (1.0 + alpha) / (1.0 - alpha);
        Self {
            gamma_ln: gamma.ln(),
            gamma,
            bins: BTreeMap::new(),
            zeros: 0,
            count: 0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        }
    }

    /// Adds one observation. NaN is ignored.
    pub fn add(&mut self, value: f64) {
        if value.is_nan() {
            return;
        }
        let value = value.max(0.0);
        self.count += 1;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        if value <= ZERO_THRESHOLD {
            self.zeros += 1;
        } else {
            // Bucket i holds (gamma^(i-1), gamma^i]; i fits in i32 for any finite f64.
            let index = (value.ln() / self.gamma_ln).ceil() as i32;
            *self.bins.entry(index).or_default() += 1;
        }
    }

    /// Number of observations.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Largest observation, exact.
    pub fn max(&self) -> Option<f64> {
        (self.count > 0).then_some(self.max)
    }

    /// Estimates quantile `q` in `[0, 1]` using the lower rank `floor(q * (n - 1))`.
    /// Returns `None` when empty.
    pub fn quantile(&self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        let rank = (q.clamp(0.0, 1.0) * (self.count - 1) as f64).floor() as u64;
        if rank < self.zeros {
            return Some(self.min.max(0.0));
        }
        // The extremes are tracked exactly, so p0 and p100 need no estimate.
        if rank == 0 {
            return Some(self.min);
        }
        if rank == self.count - 1 {
            return Some(self.max);
        }
        let mut seen = self.zeros;
        for (&index, &n) in &self.bins {
            seen += n;
            if seen > rank {
                // Midpoint of the bucket in relative terms, clamped to observed values.
                let estimate = 2.0 * self.gamma.powi(index) / (self.gamma + 1.0);
                return Some(estimate.clamp(self.min, self.max));
            }
        }
        Some(self.max)
    }
}
