//! Tests extracted from commands_export.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::{Entity, EntityKind};
	use graph::graph::GraphGnn;

	fn seed(g: &mut GraphGnn, text: &str, valid_from: Option<SystemTime>) -> String {
		let root = g.root.id.clone();
		let mut t = Entity {
			id: util::content_hash(text),
			kind: EntityKind::Claim,
			..Default::default()
		};
		t.set_text(text.to_string());
		t.valid_from = valid_from;
		let id = t.id.clone();
		g.kerns
			.get_mut(&root)
			.expect("root kern")
			.entities
			.insert(id.clone(), t);
		id
	}

	#[test]
	fn export_import_round_trip_survives_json_and_restamps_bitemporal_clocks() {
		let from = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
		let mut src = GraphGnn::new();
		let a = seed(
			&mut src,
			"the auth service owns session invalidation",
			Some(from),
		);
		let b = seed(&mut src, "we ship int8 vectors on disk", None);

		let export = build_export(&mut src);
		assert_eq!(export.format, EXPORT_FORMAT);
		assert_eq!(export.version, EXPORT_VERSION);
		assert!(
			export.bitemporal.contains_key(&a) && !export.bitemporal.contains_key(&b),
			"only clock-bearing entities ride the side map"
		);

		// Through the JSON boundary — the whole point of the format. The plain
		// Entity serialization drops `valid_from` (serde(skip)); the side map is
		// what must carry it across.
		let raw = serde_json::to_string(&export).unwrap();
		let parsed: Export = serde_json::from_str(&raw).unwrap();

		let mut dst = GraphGnn::new();
		let joined = apply_import(&mut dst, parsed);
		assert!(
			joined >= 2,
			"both thoughts join an empty graph, got {joined}"
		);
		let (got_a, _) = graph::search::find_entity(&dst, &a).expect("a imported");
		assert_eq!(got_a.text(), "the auth service owns session invalidation");
		assert_eq!(
			got_a.valid_from,
			Some(from),
			"bi-temporal lower bound restored from the side map"
		);
		assert!(graph::search::find_entity(&dst, &b).is_some());

		// Idempotent: importing the same export again joins nothing new.
		let raw2 = serde_json::to_string(&build_export(&mut src)).unwrap();
		let again = apply_import(&mut dst, serde_json::from_str(&raw2).unwrap());
		assert_eq!(again, 0, "re-import is a no-op union, got {again}");
	}
}
