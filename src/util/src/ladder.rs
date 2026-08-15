//! Degradation ladders for optional subsystems. Tracks which fallback step
//! is active, logs transitions, and exposes state for health reporting.
//!
//! Each ladder is a `static` so it is zero-cost when not degraded.

use std::sync::atomic::{AtomicU8, Ordering};

/// Tracks which fallback step is active for a subsystem.
pub struct FallbackLadder {
    name: &'static str,
    steps: &'static [&'static str],
    current: AtomicU8,
}

impl FallbackLadder {
    pub const fn new(name: &'static str, steps: &'static [&'static str]) -> Self {
        Self {
            name,
            steps,
            current: AtomicU8::new(0),
        }
    }

    pub fn current_step(&self) -> u8 {
        self.current.load(Ordering::Relaxed)
    }

    pub fn current_label(&self) -> &'static str {
        let i = self.current.load(Ordering::Relaxed) as usize;
        self.steps.get(i).copied().unwrap_or("unknown")
    }

    pub fn on_primary(&self) -> bool {
        self.current_step() == 0
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Move one step down. Logs the transition.
    pub fn step_down(&self, reason: &str) {
        let prev = self.current.fetch_add(1, Ordering::Relaxed);
        let new = prev + 1;
        tracing::info!(
            target: "kern.ladder",
            subsystem = self.name,
            from_step = prev,
            from_label = self.steps.get(prev as usize).unwrap_or(&"?"),
            to_step = new,
            to_label = self.steps.get(new as usize).unwrap_or(&"?"),
            reason,
            "degradation ladder stepped down"
        );
    }

    /// Try to step back up (periodic health check succeeded).
    pub fn step_up(&self) {
        let prev = self.current.fetch_sub(1, Ordering::Relaxed);
        if prev > 0 {
            let new = prev - 1;
            tracing::info!(
                target: "kern.ladder",
                subsystem = self.name,
                from_step = prev,
                to_step = new,
                "degradation ladder stepped up"
            );
        }
    }

    /// Reset to primary path.
    pub fn reset(&self) {
        let prev = self.current.swap(0, Ordering::Relaxed);
        if prev > 0 {
            tracing::info!(
                target: "kern.ladder",
                subsystem = self.name,
                from_step = prev,
                "degradation ladder reset to primary"
            );
        }
    }
}
