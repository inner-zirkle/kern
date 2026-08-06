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
mod tests {
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
