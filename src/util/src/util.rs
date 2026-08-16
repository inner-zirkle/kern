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
#[path = "tests/util_test.rs"]
mod util_tests;

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

#[allow(clippy::result_unit_err)]
pub fn parse_rfc3339(s: &str) -> Result<std::time::SystemTime, ()> {
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
pub fn civil_from_days(z: i64) -> (i32, u32, u32) {
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
pub fn date_string(now: std::time::SystemTime) -> String {
	match now.duration_since(std::time::UNIX_EPOCH) {
		Ok(d) => {
			let days = (d.as_secs() / 86400) as i64;
			let (y, m, d) = civil_from_days(days);
			format!("{y:04}-{m:02}-{d:02}")
		}
		Err(_) => "1970-01-01".to_string(),
	}
}

/// `t` as `YYYY-MM-DD HH:MM` UTC — the resolution `kern log` prints. Minutes,
/// not seconds: the log answers "when", and sub-minute precision is noise a
/// reader has to skip past on every line.
pub fn datetime_string(t: std::time::SystemTime) -> String {
	match t.duration_since(std::time::UNIX_EPOCH) {
		Ok(d) => {
			let secs = d.as_secs();
			let days = (secs / 86400) as i64;
			let (y, m, dd) = civil_from_days(days);
			let rem = secs % 86400;
			format!(
				"{y:04}-{m:02}-{dd:02} {:02}:{:02}",
				rem / 3600,
				(rem % 3600) / 60
			)
		}
		Err(_) => "1970-01-01 00:00".to_string(),
	}
}
