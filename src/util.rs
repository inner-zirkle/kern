//! Small shared utilities: content hashing, hex, id shortening, total-order
//! float comparison, the ranking tiebreak, percentiles, clock reads, and the
//! dependency-free UUID mint.

use sha2::{Digest, Sha256};

/// SHA-256 of the UTF-8 bytes as 64 lowercase hex chars — the stable content
/// identity used for entity ids and dedup keys.
pub fn content_hash(s: &str) -> String {
	let hash = Sha256::digest(s.as_bytes());
	hex::encode(hash)
}

pub mod hex {
	const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

	/// Encode bytes as lowercase hex, two chars per byte.
	pub fn encode(bytes: impl AsRef<[u8]>) -> String {
		let bytes = bytes.as_ref();
		let mut s = String::with_capacity(bytes.len() * 2);
		for &b in bytes {
			s.push(HEX_CHARS[(b >> 4) as usize] as char);
			s.push(HEX_CHARS[(b & 0x0f) as usize] as char);
		}
		s
	}

	/// Decode a hex string of any even length. Odd length or any non-hexdigit
	/// byte yields `None`. An `ed25519:` prefix is tolerated (key strings).
	pub fn decode(s: &str) -> Option<Vec<u8>> {
		let s = s.strip_prefix("ed25519:").unwrap_or(s);
		if !s.len().is_multiple_of(2) {
			return None;
		}
		let mut out = Vec::with_capacity(s.len() / 2);
		for chunk in s.as_bytes().chunks(2) {
			let hi = hex_nibble(chunk[0])?;
			let lo = hex_nibble(chunk[1])?;
			out.push((hi << 4) | lo);
		}
		Some(out)
	}

	fn hex_nibble(b: u8) -> Option<u8> {
		match b {
			b'0'..=b'9' => Some(b - b'0'),
			b'a'..=b'f' => Some(b - b'a' + 10),
			b'A'..=b'F' => Some(b - b'A' + 10),
			_ => None,
		}
	}
}

/// First 12 chars of an id for log/display use; char-boundary safe, shorter
/// ids pass through unchanged.
pub fn short_id(id: &str) -> &str {
	match id.char_indices().nth(12) {
		Some((byte_pos, _)) => &id[..byte_pos],
		None => id,
	}
}

/// Cap `s` at `max` chars, appending `...` only when something was cut;
/// char-boundary safe.
pub fn truncate(s: &str, max: usize) -> String {
	match s.char_indices().nth(max) {
		Some((byte_pos, _)) => format!("{}...", &s[..byte_pos]),
		None => s.to_string(),
	}
}

/// Total-order shim over `PartialOrd`: incomparable pairs (NaN) compare Equal
/// instead of panicking, so float sorts stay safe on degenerate scores.
pub fn cmp_partial<T: PartialOrd>(a: &T, b: &T) -> std::cmp::Ordering {
	a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Score desc, id asc — the single ranking tiebreak; use at every ranking site
/// or top-k regresses to nondeterministic order.
pub fn cmp_rank<S: PartialOrd>(
	a_score: S,
	a_id: &str,
	b_score: S,
	b_id: &str,
) -> std::cmp::Ordering {
	cmp_partial(&b_score, &a_score).then_with(|| a_id.cmp(b_id))
}

/// Nearest-rank percentile. Input must be ascending-sorted; `p` is a fraction
/// in `[0, 1]` (clamped). `None` only on an empty slice.
pub fn percentile_sorted<T: Copy>(sorted: &[T], p: f64) -> Option<T> {
	if sorted.is_empty() {
		return None;
	}
	if p <= 0.0 {
		return Some(sorted[0]);
	}
	if p >= 1.0 {
		return Some(sorted[sorted.len() - 1]);
	}
	let rank = (p * sorted.len() as f64).ceil() as usize;
	Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// Nanoseconds since the Unix epoch; 0 on a clock-before-epoch rather than
/// panicking.
pub fn now_nanos() -> u128 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.unwrap_or_default()
		.as_nanos()
}

/// Milliseconds since the Unix epoch; 0 on a clock-before-epoch.
pub fn now_ms() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_millis() as u64)
		.unwrap_or(0)
}

/// Seconds since the Unix epoch; 0 on a clock-before-epoch.
pub fn now_secs() -> u64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs())
		.unwrap_or(0)
}

/// The LLM prompt asking for the one-sentence reason text on an edge between
/// two thoughts; both sides are capped at 500 chars to bound prompt size.
pub fn explain_relationship_prompt(a: &str, b: &str) -> String {
	format!(
		"Write one sentence describing the specific connection between these two pieces of knowledge. \
		Name the exact concept, mechanism, cause, or logical dependency that links them. \
		Do NOT use vague words like \"related\", \"similar\", \"connected\", or \"both deal with\".\n\n\
		A: {}\n\nB: {}\n\nConnection:",
		truncate(a, 500),
		truncate(b, 500),
	)
}

/// A random RFC 4122 v4 UUID string, minted from the thread-local RNG —
/// avoids a uuid-crate dependency for the one place ids are minted.
pub fn uuid_v4() -> String {
	use rand::RngExt;
	let mut rng = rand::rng();
	let mut b = [0u8; 16];
	rng.fill(&mut b);
	b[6] = (b[6] & 0x0f) | 0x40;
	b[8] = (b[8] & 0x3f) | 0x80;
	format!(
		"{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
		u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
		u16::from_be_bytes([b[4], b[5]]),
		u16::from_be_bytes([b[6], b[7]]),
		u16::from_be_bytes([b[8], b[9]]),
		u64::from_be_bytes([0, 0, b[10], b[11], b[12], b[13], b[14], b[15]]),
	)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn hex_encode_is_lowercase_two_chars_per_byte() {
		assert_eq!(hex::encode([0x00, 0xff, 0x10, 0xab]), "00ff10ab");
		assert_eq!(hex::encode([]), "");
	}

	#[test]
	fn hex_decode_roundtrips_and_rejects_bad_input() {
		assert_eq!(hex::decode(""), Some(vec![]));
		assert_eq!(hex::decode("00ff10ab"), Some(vec![0x00, 0xff, 0x10, 0xab]));
		assert_eq!(hex::decode("00FF10AB"), Some(vec![0x00, 0xff, 0x10, 0xab]));
		assert_eq!(hex::decode("ed25519:00ff"), Some(vec![0x00, 0xff]));
		assert_eq!(hex::decode("0"), None, "odd length");
		assert_eq!(hex::decode("00ff10ag"), None, "non-hex digit");
		assert_eq!(hex::encode(hex::decode("deadbeef").unwrap()), "deadbeef");
	}

	#[test]
	fn percentile_sorted_is_nearest_rank_with_edges_and_generic_types() {
		let xs: Vec<f64> = (1..=10).map(|i| i as f64).collect();
		assert_eq!(percentile_sorted(&xs, 0.0), Some(1.0), "p<=0 -> first");
		assert_eq!(percentile_sorted(&xs, 1.0), Some(10.0), "p>=1 -> last");
		assert_eq!(
			percentile_sorted(&xs, 0.5),
			Some(5.0),
			"ceil(0.5*10)=5 -> xs[4]"
		);
		assert_eq!(percentile_sorted(&xs, 0.95), Some(10.0));
		assert_eq!(percentile_sorted::<f64>(&[], 0.5), None, "empty -> None");
		let ns: Vec<u128> = vec![10, 20, 30, 40, 50];
		assert_eq!(percentile_sorted(&ns, 0.5), Some(30u128));
		assert_eq!(percentile_sorted(&ns, 0.95), Some(50u128));
	}

	#[test]
	fn cmp_rank_orders_by_score_desc_then_id_asc() {
		use std::cmp::Ordering;
		assert_eq!(cmp_rank(0.9_f64, "z", 0.1, "a"), Ordering::Less);
		assert_eq!(cmp_rank(0.1_f64, "a", 0.9, "z"), Ordering::Greater);
		assert_eq!(cmp_rank(0.5_f64, "a", 0.5, "b"), Ordering::Less);
		assert_eq!(cmp_rank(0.5_f64, "b", 0.5, "a"), Ordering::Greater);
		assert_eq!(cmp_rank(0.5_f64, "a", 0.5, "a"), Ordering::Equal);
		assert_eq!(cmp_rank(f64::NAN, "a", f64::NAN, "b"), Ordering::Less);
		assert_eq!(cmp_rank(2.0_f32, "a", 1.0_f32, "z"), Ordering::Less);
	}

	#[test]
	fn content_hash_is_deterministic_64_char_lowercase_hex() {
		let h = content_hash("kern");
		assert_eq!(h.len(), 64, "sha256 -> 32 bytes -> 64 hex chars");
		assert!(h
			.bytes()
			.all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
		assert_eq!(h, content_hash("kern"), "deterministic");
		assert_ne!(h, content_hash("kern2"), "distinct inputs differ");
	}

	#[test]
	fn short_id_caps_at_12_chars_and_is_boundary_safe() {
		assert_eq!(short_id("0123456789abcdef"), "0123456789ab");
		assert_eq!(short_id("abc"), "abc");
		assert_eq!(short_id("0123456789ab"), "0123456789ab");
		let s = short_id("ααααααααααααββ");
		assert_eq!(s.chars().count(), 12);
	}

	#[test]
	fn truncate_appends_ellipsis_only_when_cut() {
		assert_eq!(truncate("hello", 10), "hello", "under max -> unchanged");
		assert_eq!(
			truncate("hello world", 5),
			"hello...",
			"over max -> cut + ellipsis"
		);
		assert_eq!(truncate("αβγδε", 3), "αβγ...");
	}

	#[test]
	fn cmp_partial_orders_and_treats_nan_as_equal() {
		use std::cmp::Ordering;
		assert_eq!(cmp_partial(&1.0, &2.0), Ordering::Less);
		assert_eq!(cmp_partial(&2.0, &1.0), Ordering::Greater);
		assert_eq!(cmp_partial(&1.0, &1.0), Ordering::Equal);
		assert_eq!(
			cmp_partial(&f64::NAN, &1.0),
			Ordering::Equal,
			"NaN is incomparable -> Equal"
		);
	}

	#[test]
	fn uuid_v4_has_correct_layout_version_and_variant() {
		let u = uuid_v4();
		let groups: Vec<&str> = u.split('-').collect();
		assert_eq!(
			groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
			vec![8, 4, 4, 4, 12],
			"5 dash-separated groups of 8-4-4-4-12"
		);
		assert!(u.bytes().all(|c| c == b'-' || c.is_ascii_hexdigit()));
		assert_eq!(&groups[2][0..1], "4", "RFC4122 version 4");
		assert!(
			matches!(&groups[3][0..1], "8" | "9" | "a" | "b"),
			"RFC4122 variant bits"
		);
		assert_ne!(uuid_v4(), uuid_v4(), "two mints differ (random)");
	}

	#[test]
	fn now_nanos_is_after_epoch() {
		assert!(now_nanos() > 0);
	}
}

/// Inclusive lower bound of a confidence weight.
pub const CONF_MIN: f64 = 0.0;
/// Inclusive upper bound of a confidence weight.
pub const CONF_MAX: f64 = 1.0;

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ValidateError {
	#[error("conf {0} out of range [0.0..=1.0]")]
	ConfOutOfRange(f64),
}

/// Accept a confidence weight iff it is a real number in `[0.0, 1.0]`;
/// NaN is rejected, not clamped — a caller sending NaN has a bug to hear about.
pub fn validate_conf(conf: f64) -> Result<f64, ValidateError> {
	if conf.is_nan() || !(CONF_MIN..=CONF_MAX).contains(&conf) {
		return Err(ValidateError::ConfOutOfRange(conf));
	}
	Ok(conf)
}

#[cfg(test)]
mod validate_tests {
	use super::*;

	#[test]
	fn conf_out_of_range_rejected_high() {
		assert!(matches!(
			validate_conf(1.5),
			Err(ValidateError::ConfOutOfRange(_))
		));
	}

	#[test]
	fn conf_out_of_range_rejected_low() {
		assert!(matches!(
			validate_conf(-0.01),
			Err(ValidateError::ConfOutOfRange(_))
		));
	}

	#[test]
	fn conf_out_of_range_rejected_nan() {
		assert!(matches!(
			validate_conf(f64::NAN),
			Err(ValidateError::ConfOutOfRange(_))
		));
	}

	#[test]
	fn conf_inclusive_bounds_accepted() {
		assert_eq!(validate_conf(0.0), Ok(0.0));
		assert_eq!(validate_conf(1.0), Ok(1.0));
		assert_eq!(validate_conf(0.5), Ok(0.5));
	}
}

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

// A warn on a hot path floods the log until the log is useless. Counters behind
// such a warn stay exact and unconditional; only the printed line is throttled.
pub struct LogThrottle {
	last_secs: AtomicU64,
	interval_secs: u64,
}

impl LogThrottle {
	pub const fn new(interval_secs: u64) -> Self {
		Self {
			last_secs: AtomicU64::new(0),
			interval_secs,
		}
	}

	// The first call always passes, then at most one per interval. Racing callers
	// may both pass — a duplicate line is cheaper than a lock on a hot path.
	pub fn allow(&self) -> bool {
		// 0 means "never fired", so a pre-1970 clock must not read as never.
		let now = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(0)
			.max(1);
		let last = self.last_secs.load(Ordering::Relaxed);
		if last != 0 && now.saturating_sub(last) < self.interval_secs {
			return false;
		}
		self.last_secs.store(now, Ordering::Relaxed);
		true
	}
}

#[cfg(test)]
mod throttle_tests {
	use super::*;

	#[test]
	fn the_first_call_passes_and_the_flood_behind_it_does_not() {
		let t = LogThrottle::new(3600);
		assert!(t.allow(), "the first crossing is always reported");
		for _ in 0..1000 {
			assert!(!t.allow(), "every later call inside the window is silent");
		}
	}

	#[test]
	fn a_zero_interval_never_throttles() {
		let t = LogThrottle::new(0);
		assert!(t.allow());
		assert!(t.allow(), "interval 0 disables throttling");
	}
}

pub(crate) fn parse_rfc3339(s: &str) -> Result<std::time::SystemTime, ()> {
	let s = s.trim();
	// The fixed slices below read bytes 0..19: length must be checked AFTER the
	// trim and those bytes must be ASCII, or the str slicing panics.
	if s.len() < 19 || !s.as_bytes()[..19].is_ascii() {
		return Err(());
	}
	let year: i32 = s[0..4].parse().map_err(|_| ())?;
	let month: u32 = s[5..7].parse().map_err(|_| ())?;
	let day: u32 = s[8..10].parse().map_err(|_| ())?;
	let hour: u32 = s[11..13].parse().map_err(|_| ())?;
	let min: u32 = s[14..16].parse().map_err(|_| ())?;
	let sec: u32 = s[17..19].parse().map_err(|_| ())?;

	fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
		let y = if m <= 2 { y - 1 } else { y } as i64;
		let m = m as i64;
		let d = d as i64;
		let era = if y >= 0 { y } else { y - 399 } / 400;
		let yoe = y - era * 400;
		let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
		let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
		era * 146097 + doe - 719468
	}

	let days = days_from_civil(year, month, day);
	let secs = days * 86400 + hour as i64 * 3600 + min as i64 * 60 + sec as i64;
	if secs < 0 {
		return Err(());
	}
	Ok(std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

/// The inverse of `days_from_civil`: days since the Unix epoch -> (year, month,
/// day). Howard Hinnant's algorithm, the same one `days_from_civil` inverts, so
/// the pair round-trips. Used to render a `SystemTime` as a calendar date for
/// the distill prompt, so the model can resolve relative dates ("last Tuesday")
/// against a known today.
pub(crate) fn civil_from_days(z: i64) -> (i32, u32, u32) {
	let z = z + 719468;
	let era = if z >= 0 { z } else { z - 146096 } / 146097;
	let doe = z - era * 146097; // [0, 146096]
	let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
	let y = yoe + era * 400;
	let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
	let mp = (5 * doy + 2) / 153; // [0, 11]
	let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
	let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
	(if m <= 2 { y + 1 } else { y } as i32, m, d)
}

/// `now` as a calendar date in `YYYY-MM-DD`, for the distill prompt's relative-
/// date resolution. Time-of-day is dropped: a day is the resolution `valid_from`
/// already carries, and a UTC date avoids a local-time zone the prompt has no
/// way to name. Returns a fixed sentinel on a clock-before-epoch (impossible in
/// practice) rather than panicking.
pub(crate) fn date_string(now: std::time::SystemTime) -> String {
	match now.duration_since(std::time::UNIX_EPOCH) {
		Ok(d) => {
			let days = (d.as_secs() / 86400) as i64;
			let (y, m, d) = civil_from_days(days);
			format!("{y:04}-{m:02}-{d:02}")
		}
		Err(_) => "1970-01-01".to_string(),
	}
}

#[cfg(test)]
mod time_tests {
	use super::parse_rfc3339;

	#[test]
	fn valid_timestamps_parse() {
		assert!(parse_rfc3339("2026-06-05T09:00:00Z").is_ok());
		assert!(parse_rfc3339("2026-06-05T09:00:00").is_ok());
		assert!(parse_rfc3339("  2026-06-05T09:00:00Z  ").is_ok());
	}

	#[test]
	fn short_after_trim_is_err_not_panic() {
		assert_eq!(parse_rfc3339("   2026   "), Err(()));
		assert_eq!(parse_rfc3339("                    "), Err(()));
		assert_eq!(parse_rfc3339(""), Err(()));
	}

	#[test]
	fn multibyte_in_slice_region_is_err_not_panic() {
		assert_eq!(parse_rfc3339("20é6-06-05T09:00:00Z"), Err(()));
		assert_eq!(parse_rfc3339("2026-06-05T09:00:0😀"), Err(()));
	}

	#[test]
	fn malformed_digits_are_err() {
		assert_eq!(parse_rfc3339("YYYY-06-05T09:00:00Z"), Err(()));
	}

	#[test]
	fn epoch_and_known_instant_compute_correctly() {
		use std::time::{Duration, UNIX_EPOCH};
		assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Ok(UNIX_EPOCH));
		// 2000-01-01T00:00:00Z = 946684800 unix seconds.
		assert_eq!(
			parse_rfc3339("2000-01-01T00:00:00Z"),
			Ok(UNIX_EPOCH + Duration::from_secs(946684800))
		);
	}

	#[test]
	fn civil_from_days_at_epoch_is_1970_01_01() {
		assert_eq!(super::civil_from_days(0), (1970, 1, 1));
	}

	#[test]
	fn civil_from_days_round_trips_a_known_date() {
		// 2026-07-22 is 20656 days after 1970-01-01.
		assert_eq!(super::civil_from_days(20656), (2026, 7, 22));
	}

	#[test]
	fn date_string_renders_epoch_and_a_known_instant() {
		assert_eq!(super::date_string(std::time::UNIX_EPOCH), "1970-01-01");
		let t = super::parse_rfc3339("2026-07-22T00:00:00").unwrap();
		assert_eq!(super::date_string(t), "2026-07-22");
	}
}
