//! Ingest policy knobs: dedup threshold, clamp lengths, retention-to-TTL
//! mapping, and the review policy that decides which sources land `pending`
//! (held for curation) versus `active`.

use base::base_constants::INGEST_DEDUP_THRESHOLD;
use base::base_types::{EntityKind, ReviewState, Source};

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

/// Source scheme → the curation state a claim arriving on it is placed in. An
/// absent key is `Active`, so an empty policy is today's behaviour exactly.
pub type ReviewPolicy = BTreeMap<String, ReviewState>;

/// The one resolution, so no producer can key on something other than the
/// scheme `IngestConfig::validate` checks against.
pub fn review_for(policy: &ReviewPolicy, source: &Source) -> ReviewState {
	policy.get(source.scheme()).copied().unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct Config {
	pub dedup_threshold: f64,
	/// Per-kind overrides indexed by `EntityKind as u8` (Fact=0 .. Conclusion=4).
	/// `None` falls back to `dedup_threshold`. Default `[None; 5]` is
	/// bit-identical to a single global threshold — an operator can ask Facts
	/// to dedup tighter than Claims without tightening both (ROADMAP item 48
	/// beside). Preset-owned, not auto-tuned.
	pub dedup_threshold_by_kind: [Option<f64>; EntityKind::Conclusion as usize + 1],
	pub valid_from: Option<std::time::SystemTime>,
	pub valid_until: Option<std::time::SystemTime>,
	// The POLICY, not a resolved state: the intake drain hands one `Config` to a
	// whole pass of records whose sources differ, so the scheme is only known
	// per job. `job()` resolves it — the single gate every producer passes.
	pub review_policy: ReviewPolicy,
	// The write-time hygiene gate (noise/secret refusal). Travels with the job
	// for the same reason `review_policy` does: the worker is the one commit
	// path every producer funnels through, so the policy rides the job rather
	// than being re-derived per producer. Default `Off` — the gate's arrival is
	// not a behaviour change.
	pub hygiene: hygiene::GateConfig,
}

impl Default for Config {
	fn default() -> Self {
		Self {
			dedup_threshold: INGEST_DEDUP_THRESHOLD,
			dedup_threshold_by_kind: [None; EntityKind::Conclusion as usize + 1],
			valid_from: None,
			valid_until: None,
			review_policy: ReviewPolicy::new(),
			hygiene: hygiene::GateConfig::default(),
		}
	}
}

// Retention is a duration at the caller boundary and an absolute instant on the
// entity. The single conversion lives here so the CLI flag and the MCP field
// cannot drift apart; 0 means "no TTL", matching every other unset knob.
pub fn valid_until_from_retention(retention_secs: u64) -> Result<Option<SystemTime>, String> {
	if retention_secs == 0 {
		return Ok(None);
	}
	SystemTime::now()
		.checked_add(Duration::from_secs(retention_secs))
		.map(Some)
		.ok_or_else(|| format!("retention_secs {retention_secs} overflows the clock"))
}

impl Config {
	/// Per-kind dedup threshold: a `Some` override on the kind's slot wins, else
	/// the global `dedup_threshold`. Indexed by `EntityKind as u8` so it is O(1)
	/// and needs no `Hash` derive on `EntityKind`.
	pub fn dedup_threshold_for(&self, kind: EntityKind) -> f64 {
		self.dedup_threshold_by_kind[kind as usize].unwrap_or(self.dedup_threshold)
	}

	/// The same conversion, resolved *now*, for the entrances whose retention is
	/// a standing policy rather than a per-call argument. Long-lived callers (the
	/// intake poll loop, the file watcher) must build one per pass: resolving it
	/// once at startup would stamp a file seen on day 30 with a deadline measured
	/// from boot. `Config::validate` refuses an unrepresentable retention at
	/// load, so an error here is a caller that skipped it — say so, then no TTL.
	pub fn with_retention(mut self, retention_secs: u64) -> Self {
		self.valid_until = valid_until_from_retention(retention_secs).unwrap_or_else(|e| {
			tracing::error!(target: "kern.ingest", error = %e, "unusable retention_secs; ingesting with no TTL");
			None
		});
		self
	}

	pub fn validate(&self) -> Result<(), String> {
		if !(0.0..=1.0).contains(&self.dedup_threshold) {
			return Err(format!(
				"dedup_threshold must be in [0.0, 1.0], got {}",
				self.dedup_threshold
			));
		}
		for (i, slot) in self.dedup_threshold_by_kind.iter().enumerate() {
			if let Some(t) = slot {
				if !(0.0..=1.0).contains(t) {
					let kind = EntityKind::from_u8(i as u8)
						.map(|k| k.as_str())
						.unwrap_or("unknown");
					return Err(format!(
						"dedup_threshold_by_kind[{kind}] must be in [0.0, 1.0], got {t}"
					));
				}
			}
		}
		Ok(())
	}
}

#[cfg(test)]
#[path = "tests/ingest_config_test.rs"]
mod ingest_config_tests;
