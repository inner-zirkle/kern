//! The guard that makes `FORMAT_VERSION` mean something.
//!
//! A version byte only identifies a layout if every layout change bumps it.
//! That broke once, quietly: f60fbce (2026-08-15) added `Entity.trust_tier`, a
//! persisted field, and never touched `store_core` — so two incompatible
//! `Entity` layouts both call themselves version 10, and `legacy.rs` has to try
//! both and hope. Nothing failed at the time; the cost landed on whoever read
//! one of those stores next.
//!
//! This test is what would have failed. It encodes a fixed sample of every
//! persisted row type and pins a checksum of the bytes. Add, remove, reorder or
//! retype a persisted field and the checksum moves — which is the reminder to
//! bump `FORMAT_VERSION`, freeze the outgoing layout in `legacy.rs`, and update
//! the constant here in the same commit.
//!
//! It deliberately pins BYTES, not field names: bincode is positional, so the
//! bytes are the actual contract. Renaming a field is not a layout change and
//! this test correctly stays quiet for it.

use super::*;
use base::base_types::{mk_entity, EntityKind, Reason, ReasonKind};
use std::time::{Duration, UNIX_EPOCH};

// FNV-1a. A hash, not a cryptographic one — the job is "did these bytes move",
// and an inline 6-liner beats a dependency for that.
fn checksum(bytes: &[u8]) -> u64 {
	let mut h: u64 = 0xcbf2_9ce4_8422_2325;
	for b in bytes {
		h ^= u64::from(*b);
		h = h.wrapping_mul(0x0000_0100_0000_01b3);
	}
	h
}

// Every map holds at most ONE entry: `HashMap` iteration order is randomised per
// process, so a fixture with two would hash differently run to run. One entry is
// enough to cover the value type, which is what changes.
fn sample_stored_kern() -> StoredKern {
	let mut e = mk_entity("e1", "layout guard sample", 0.5, EntityKind::Claim);
	e.root_id = "root".into();
	e.external_id = "ext".into();
	e.created_at = Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000));
	e.valid_until_lamport = 7;
	e.producer_id = "p".into();
	e.unlinked_count = 2;

	let mut k = Kern::new("k1", "root");
	k.graviton_text = "guarded".into();
	k.graviton_vec = vec![0.5, 0.25];
	k.mass = 2.0;
	k.entities.insert("e1".into(), e);
	k.reasons.insert(
		"r1".into(),
		Reason {
			id: "r1".into(),
			from: "e1".into(),
			to: "e2".into(),
			to_kern_id: "k2".into(),
			kind: ReasonKind::Ratification,
			text: "edge".into(),
			score: 0.75,
			score_lamport: 3,
			score_producer: "sp".into(),
			producer_id: "pid".into(),
			..Default::default()
		},
	);
	StoredKern::from_kern(&k)
}

fn sample_cold_row() -> ColdRow {
	let mut e = mk_entity("c1", "cold layout sample", 0.25, EntityKind::Fact);
	e.producer_id = "p".into();
	ColdRow {
		entity: e,
		temporal: StoredTemporal::default(),
	}
}

// Bytes only — no zstd frame and no version byte, so the checksum tracks the
// struct layout and nothing else (a zstd version bump must not fail this).
fn layout_checksum<T: Serialize>(v: &T) -> u64 {
	let raw = bincode::serde::encode_to_vec(v, bincode_cfg()).expect("fixture encodes");
	checksum(&raw)
}

const STORED_KERN_LAYOUT: u64 = 0x7060_e33c_7751_2204;
const COLD_ROW_LAYOUT: u64 = 0x31e6_75d4_acc0_aa0e;

const WHAT_TO_DO: &str = "\
a persisted layout changed. That is allowed — but it MUST come with:
  1. FORMAT_VERSION bumped in src/store_core/src/lib.rs,
  2. the outgoing layout frozen in src/store_core/src/legacy.rs and added to
     READABLE_VERSIONS, so existing stores can still be read and migrated,
  3. this constant updated to the new value printed below.
Skipping (1) is what made version 10 ambiguous (see this file's header).";

#[test]
fn the_stored_kern_layout_has_not_changed_without_a_version_bump() {
	let got = layout_checksum(&sample_stored_kern());
	assert_eq!(
		got, STORED_KERN_LAYOUT,
		"StoredKern layout checksum is {got:#x}, expected {STORED_KERN_LAYOUT:#x}\n{WHAT_TO_DO}"
	);
}

#[test]
fn the_cold_row_layout_has_not_changed_without_a_version_bump() {
	let got = layout_checksum(&sample_cold_row());
	assert_eq!(
		got, COLD_ROW_LAYOUT,
		"ColdRow layout checksum is {got:#x}, expected {COLD_ROW_LAYOUT:#x}\n{WHAT_TO_DO}"
	);
}

// The guard guards itself: if the fixture ever encoded to nothing, both tests
// above would pass forever on an empty layout.
#[test]
fn the_fixtures_actually_encode_something() {
	let raw =
		bincode::serde::encode_to_vec(sample_stored_kern(), bincode_cfg()).expect("fixture encodes");
	assert!(
		raw.len() > 64,
		"the StoredKern fixture encoded to {} bytes — too small to be covering the type",
		raw.len()
	);
}
