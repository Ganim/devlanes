//! The adaptive window: additive increase while the machine has headroom,
//! halving under pressure. Pure state over explicit times, so pressure traces
//! can be replayed without a runtime.

use std::time::Duration;

use devlanes_core::gate::{MemoryPressure, PressureSample};

/// Tuning of [`Controller`]. CPU values are percentages of the whole machine.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AdaptiveConfig {
    /// Window at the start of each busy period.
    pub initial: usize,
    /// Largest window.
    pub max: usize,
    /// Grow while system CPU stays below this.
    pub target_cpu: f64,
    /// CPU above this for [`AdaptiveConfig::high_for`] halves the window.
    pub high_cpu: f64,
    /// Headroom needed this long for each +1.
    pub grow_every: Duration,
    /// How long CPU must stay above `high_cpu` before a halving; also the
    /// shortest time between two halvings.
    pub high_for: Duration,
    /// How often the gate reads pressure while starts are queued or in flight.
    pub sample_every: Duration,
}

impl AdaptiveConfig {
    /// The documented pacing rules: +1 every 2 s below `target_cpu`, halve after
    /// 3 s above 90% or at memory warn, readings every second.
    pub fn new(initial: usize, max: usize, target_cpu: f64) -> Self {
        let max = max.max(1);
        Self {
            initial: initial.clamp(1, max),
            max,
            target_cpu,
            high_cpu: 90.0,
            grow_every: Duration::from_secs(2),
            high_for: Duration::from_secs(3),
            sample_every: Duration::from_secs(1),
        }
    }
}

/// Additive-increase, multiplicative-decrease window over pressure readings.
#[derive(Debug, Clone)]
pub struct Controller {
    config: AdaptiveConfig,
    window: usize,
    memory: MemoryPressure,
    /// Start of the current stretch of readings with headroom.
    good_since: Option<Duration>,
    /// Start of the current stretch of readings above `high_cpu`.
    high_since: Option<Duration>,
    last_cut: Option<Duration>,
}

impl Controller {
    /// A controller at its initial window.
    pub fn new(config: AdaptiveConfig) -> Self {
        Self {
            window: config.initial,
            config,
            memory: MemoryPressure::Normal,
            good_since: None,
            high_since: None,
            last_cut: None,
        }
    }

    /// The configuration in use.
    pub fn config(&self) -> &AdaptiveConfig {
        &self.config
    }

    /// Starts allowed at once.
    pub fn window(&self) -> usize {
        self.window
    }

    /// True while memory pressure is critical: no new starts at all.
    pub fn paused(&self) -> bool {
        self.memory == MemoryPressure::Critical
    }

    /// Back to the initial window. The gate calls it when nothing is queued or
    /// in flight, so every busy period starts at `initial` again.
    pub fn reset(&mut self) {
        *self = Self::new(self.config);
    }

    /// Applies one reading taken at `now`. `demand` is true while starts are
    /// queued: the window only grows when it is the limit.
    pub fn observe(&mut self, now: Duration, sample: &PressureSample, demand: bool) {
        let config = self.config;
        self.memory = sample.memory.unwrap_or_default();
        let cpu = sample.cpu_percent;
        if cpu.is_some_and(|cpu| cpu > config.high_cpu) {
            self.high_since.get_or_insert(now);
        } else {
            self.high_since = None;
        }
        let cpu_overloaded = self
            .high_since
            .is_some_and(|since| now.saturating_sub(since) >= config.high_for);
        let cooled = self
            .last_cut
            .is_none_or(|cut| now.saturating_sub(cut) >= config.high_for);
        if (cpu_overloaded || self.memory >= MemoryPressure::Warn) && cooled {
            self.window = (self.window / 2).max(1);
            self.last_cut = Some(now);
            self.good_since = None;
            // Another halving needs another full stretch above the threshold.
            if cpu_overloaded {
                self.high_since = Some(now);
            }
            return;
        }
        let headroom =
            self.memory == MemoryPressure::Normal && cpu.is_some_and(|cpu| cpu < config.target_cpu);
        if !(headroom && demand) {
            self.good_since = None;
            return;
        }
        let since = *self.good_since.get_or_insert(now);
        if now.saturating_sub(since) >= config.grow_every && self.window < config.max {
            self.window += 1;
            self.good_since = Some(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(cpu: f64, memory: MemoryPressure) -> PressureSample {
        PressureSample {
            cpu_percent: Some(cpu),
            memory: Some(memory),
            cpu_stall_percent: None,
        }
    }

    #[test]
    fn no_growth_without_demand() {
        let mut controller = Controller::new(AdaptiveConfig::new(2, 8, 75.0));
        for second in 0..10 {
            controller.observe(
                Duration::from_secs(second),
                &sample(10.0, MemoryPressure::Normal),
                false,
            );
        }
        assert_eq!(controller.window(), 2);
    }

    #[test]
    fn missing_cpu_holds_the_window() {
        let mut controller = Controller::new(AdaptiveConfig::new(2, 8, 75.0));
        for second in 0..10 {
            controller.observe(
                Duration::from_secs(second),
                &PressureSample::default(),
                true,
            );
        }
        assert_eq!(controller.window(), 2);
        assert!(!controller.paused());
    }

    #[test]
    fn initial_is_clamped_to_max() {
        assert_eq!(AdaptiveConfig::new(9, 3, 75.0).initial, 3);
        assert_eq!(AdaptiveConfig::new(0, 0, 75.0).max, 1);
    }
}
