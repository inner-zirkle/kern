//! Stigmergic access heat: every retrieval deposits [`HeatConfig::deposit_access`]
//! on the entities it touched, and heat decays exponentially with the configured
//! half-life. GC reads the decayed value to pick cold-tier victims, so "still
//! used" is measured, not declared.

use std::time::SystemTime;

#[cfg(test)]
#[path = "tests/heat_test.rs"]
mod heat_tests;

pub fn decayed(heat: f32, since: Option<SystemTime>, now: SystemTime, half_life_secs: u64) -> f32 {
	if heat <= 0.0 {
		return 0.0;
	}
	let Some(since) = since else {
		return heat;
	};
	let dt = match now.duration_since(since) {
		Ok(d) => d.as_secs_f64(),
		Err(_) => return heat,
	};
	let t = (half_life_secs as f64).max(1.0);
	let lambda = std::f64::consts::LN_2 / t;
	(heat as f64 * (-lambda * dt).exp()) as f32
}

pub fn deposit(
	heat: f32,
	since: Option<SystemTime>,
	now: SystemTime,
	half_life_secs: u64,
	deposit: f32,
) -> f32 {
	decayed(heat, since, now, half_life_secs) + deposit
}

// ==== [weibull] ====

/// Per-claim-kind Weibull decay parameters. `shape` < 1.0 = decreasing hazard
/// (the longer it survives, the slower it decays — preferences); `shape` = 1.0
/// is plain exponential; > 1.0 = increasing hazard (ages out fast). The scale
/// η is `eta_factor × half_life/ln2`, so the operator's configured half-life
/// stays the single time knob and `{1.0, 1.0}` is bit-identical to [`decayed`].
/// Adapted from mnemosyne's per-memory-type Weibull table (MIT).
#[derive(Debug, Clone, Copy)]
pub struct KindDecay {
	pub shape: f64,
	pub eta_factor: f64,
}

impl Default for KindDecay {
	fn default() -> Self {
		Self {
			shape: 1.0,
			eta_factor: 1.0,
		}
	}
}

/// The decay curve for a distilled claim's kind label. Ratios are mnemosyne's
/// hour table normalized to its `general` row, remapped onto kern's built-in
/// claim kinds. An empty or unknown label — every non-distilled entity — is
/// the default curve, exactly today's exponential.
pub fn kind_decay(label: &str) -> KindDecay {
	let (shape, eta_factor) = match label {
		"preference" => (0.4, 26.0),
		"decision" => (1.0, 2.0),
		"project" => (0.85, 6.4),
		"fact" => (0.8, 4.3),
		"code-fact" => (0.75, 12.9),
		"reference" => (0.5, 26.0),
		"procedural" => (0.9, 2.9),
		_ => return KindDecay::default(),
	};
	KindDecay { shape, eta_factor }
}

/// The claim-kind label an entity carries, or "" for everything that is not a
/// distilled claim. The one label decode, shared with the query filter's
/// convention (`session://<kind>` in the Session source title).
pub fn claim_kind_label(source: &base::base_types::Source) -> &str {
	source.title().strip_prefix("session://").unwrap_or("")
}

/// Weibull survival decay: `heat × exp(-(Δt/η)^k)`. With the default curve
/// this equals [`decayed`] to the last bit (k=1 collapses the power).
pub fn decayed_weibull(
	heat: f32,
	since: Option<SystemTime>,
	now: SystemTime,
	half_life_secs: u64,
	kd: KindDecay,
) -> f32 {
	if kd.shape == 1.0 && kd.eta_factor == 1.0 {
		return decayed(heat, since, now, half_life_secs);
	}
	if heat <= 0.0 {
		return 0.0;
	}
	let Some(since) = since else {
		return heat;
	};
	let dt = match now.duration_since(since) {
		Ok(d) => d.as_secs_f64(),
		Err(_) => return heat,
	};
	let eta = (half_life_secs as f64).max(1.0) / std::f64::consts::LN_2 * kd.eta_factor;
	(heat as f64 * (-(dt / eta).powf(kd.shape)).exp()) as f32
}

/// [`decayed`] with the entity's own claim-kind curve.
pub fn decayed_for(e: &base::base_types::Entity, now: SystemTime, half_life_secs: u64) -> f32 {
	decayed_weibull(
		e.heat,
		e.heat_updated_at,
		now,
		half_life_secs,
		kind_decay(claim_kind_label(&e.source)),
	)
}

/// [`deposit`] with the entity's own claim-kind curve.
pub fn deposit_for(
	e: &base::base_types::Entity,
	now: SystemTime,
	half_life_secs: u64,
	amount: f32,
) -> f32 {
	decayed_for(e, now, half_life_secs) + amount
}
