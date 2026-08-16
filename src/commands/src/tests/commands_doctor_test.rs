//! Tests extracted from commands_doctor.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::{Entity, EntityKind, Reason};
	use graph::graph::GraphGnn;

	fn seed(g: &mut GraphGnn, text: &str) -> String {
		let root = g.root.id.clone();
		let mut t = Entity {
			id: util::content_hash(text),
			kind: EntityKind::Claim,
			..Default::default()
		};
		t.set_text(text.to_string());
		let id = t.id.clone();
		g.kerns
			.get_mut(&root)
			.expect("root kern")
			.entities
			.insert(id.clone(), t);
		id
	}

	#[test]
	fn doctor_finds_dangling_reasons_and_repair_executes_only_the_manifest() {
		let cfg = config::Config::default();
		let mut g = GraphGnn::new();
		let a = seed(&mut g, "a real thought");
		let root = g.root.id.clone();
		{
			let kern = g.kerns.get_mut(&root).unwrap();
			graph::reason::add_reason(
				kern,
				Reason {
					id: "r-dangling".into(),
					from: a.clone(),
					to: "no-such-entity".into(),
					..Default::default()
				},
			);
		}

		let manifest = diagnose(&cfg, &g);
		let dangling = manifest
			.findings
			.iter()
			.find(|f| f.code == "dangling_reasons")
			.expect("dangling edge must be found");
		assert_eq!(dangling.repairs.len(), 1);

		// Round-trip through JSON — the manifest IS the authorization format.
		let raw = serde_json::to_string(&manifest).unwrap();
		let parsed: Manifest = serde_json::from_str(&raw).unwrap();
		let (dropped, _) = apply_repairs(&mut g, &parsed);
		assert_eq!(dropped, 1);
		assert!(!g
			.kerns
			.get(&root)
			.unwrap()
			.reasons
			.contains_key("r-dangling"));

		// Fail-closed: replaying the same manifest re-verifies and finds the
		// reason gone — nothing to drop, no error.
		let (again, _) = apply_repairs(&mut g, &parsed);
		assert_eq!(again, 0);
	}

	#[test]
	fn repair_with_no_authorized_actions_repairs_nothing() {
		let cfg = config::Config::default();
		let mut g = GraphGnn::new();
		seed(&mut g, "healthy thought");
		// A manifest whose findings carry no repairs (e.g. only advice).
		let manifest = Manifest {
			format: "kern-doctor".into(),
			version: 1,
			findings: vec![],
		};
		assert_eq!(apply_repairs(&mut g, &manifest), (0, 0));
	}
}
