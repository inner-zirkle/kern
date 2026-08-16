//! Tests extracted from base_types.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	#[test]
	fn entity_set_text_replaces_text_and_marks_dirty() {
		let mut e = Entity {
			statements: vec!["old statement".into()],
			chunks: vec![ChunkPart {
				kind: ChunkPartKind::StatementRef,
				text: String::new(),
				index: 0,
			}],
			..Default::default()
		};
		assert_eq!(e.text(), "old statement");
		assert!(!e.dirty);

		e.set_text("brand new text".into());

		assert_eq!(e.text(), "brand new text");
		assert!(e.dirty, "edit must mark the entity dirty for reevaluation");
		assert!(
			e.statements.is_empty(),
			"statement refs are dropped on edit"
		);
		assert!(e.updated_at.is_some());
	}

	#[test]
	fn reason_set_text_replaces_text_and_marks_dirty() {
		let mut r = Reason {
			text: "old edge".into(),
			..Default::default()
		};
		assert!(!r.dirty);
		r.set_text("new edge".into());
		assert_eq!(r.text, "new edge");
		assert!(r.dirty);
	}

	#[test]
	fn conf_mean_and_variance_handle_a_zero_total_prior() {
		let e = Entity {
			conf_alpha: 0.0,
			conf_beta: 0.0,
			..Default::default()
		};
		assert_eq!(e.conf_mean(), 0.5);
		assert_eq!(e.conf_variance(), 0.0);
	}

	#[test]
	fn conf_mean_and_variance_for_a_beta_prior() {
		// Beta(2,1): mean = a/(a+b) = 2/3; var = ab / ((a+b)^2 (a+b+1)) = 2/36.
		let e = Entity {
			conf_alpha: 2.0,
			conf_beta: 1.0,
			..Default::default()
		};
		assert!((e.conf_mean() - 2.0 / 3.0).abs() < 1e-12);
		assert!((e.conf_variance() - 2.0 / 36.0).abs() < 1e-12);
	}

	#[test]
	fn kern_has_graviton_requires_both_text_and_vector() {
		let mut k = Kern::new("k", "");
		assert!(!k.has_graviton(), "fresh kern has no graviton");
		k.graviton_text = "topic".into();
		assert!(
			!k.has_graviton(),
			"text without a vector is not a full graviton"
		);
		k.graviton_vec = vec![0.1, 0.2];
		assert!(k.has_graviton(), "text + vector -> gravitationally bound");
	}

	#[test]
	fn new_named_child_sets_graviton_parent_and_root() {
		let k = Kern::new_named_child("parent", "rootid", "generic", vec![0.5, 0.5]);
		assert_eq!(k.parent, "parent");
		assert_eq!(k.root_id, "rootid");
		assert_eq!(k.graviton_text, "generic");
		assert_eq!(k.graviton_vec, vec![0.5, 0.5]);
		assert!(k.is_named() && k.has_graviton());
		assert!(!k.id.is_empty(), "id is the content hash, never empty");
	}

	#[test]
	fn kern_id_derivation_is_deterministic_and_input_sensitive() {
		assert_eq!(unnamed_kern_id("p", 42), unnamed_kern_id("p", 42));
		assert_eq!(
			named_child_kern_id("p", "code", 9),
			named_child_kern_id("p", "code", 9)
		);
		assert_ne!(unnamed_kern_id("p", 1), unnamed_kern_id("p", 2));
		assert_ne!(unnamed_kern_id("a", 7), unnamed_kern_id("b", 7));
		assert_ne!(
			named_child_kern_id("p", "code", 9),
			named_child_kern_id("p", "docs", 9)
		);
		assert!(!unnamed_kern_id("p", 0).is_empty());
		assert!(!named_child_kern_id("p", "x", 0).is_empty());
	}

	#[test]
	fn entity_kind_serde_roundtrip() {
		for k in [
			EntityKind::Fact,
			EntityKind::Claim,
			EntityKind::Document,
			EntityKind::Question,
			EntityKind::Conclusion,
		] {
			let json = serde_json::to_string(&k).expect("serialize");
			let back: EntityKind = serde_json::from_str(&json).expect("deserialize");
			assert_eq!(k, back, "roundtrip failed for {k:?}");
			assert_eq!(EntityKind::parse(k.as_str()), Some(k));
		}
	}

	#[test]
	fn entity_status_default_is_active() {
		assert_eq!(EntityStatus::default(), EntityStatus::Active);
	}

	#[test]
	fn source_scheme_returns_correct_tag() {
		let cases: &[(Source, &str)] = &[
			(
				Source::File {
					path: "/x".into(),
					section: String::new(),
					title: String::new(),
					author: String::new(),
					url: String::new(),
				},
				"file",
			),
			(
				Source::Ticket {
					system: "gh".into(),
					object_id: "1".into(),
					section: String::new(),
					title: String::new(),
					author: String::new(),
					url: String::new(),
				},
				"ticket",
			),
			(
				Source::Session {
					session_id: "s".into(),
					section: String::new(),
					title: String::new(),
				},
				"session",
			),
			(
				Source::Agent {
					agent: "a".into(),
					object_id: "o".into(),
					title: String::new(),
				},
				"agent",
			),
			(
				Source::Inline {
					hash: "h".into(),
					section: String::new(),
				},
				"inline",
			),
		];
		for (src, want) in cases {
			assert_eq!(src.scheme(), *want);
		}
		assert!(Source::parse_scheme("file").is_some());
		assert!(Source::parse_scheme("bogus").is_none());
	}

	#[test]
	fn source_id_pins_null_delimited_composition() {
		// `source_id` = content_hash(scheme \x00 object \x00 section); the \x00
		// delimiter is load-bearing, not a separator of convenience. Changing it
		// or reordering the fields orphans every source id and breaks the stored
		// import guard that checks `content_hash(&e.text()) == e.id`
		// (ROADMAP item 77).
		let ticket = Source::Ticket {
			system: "gh".into(),
			object_id: "42".into(),
			section: "disc".into(),
			title: String::new(),
			author: String::new(),
			url: String::new(),
		};
		assert_eq!(
			ticket.source_id(),
			Some(util::content_hash("ticket\x0042\x00disc"))
		);
		// An empty object short-circuits to None before the hash; pin that the
		// guard is the emptiness check, not a digest of an empty object.
		assert_eq!(
			Source::Ticket {
				system: "gh".into(),
				object_id: String::new(),
				section: String::new(),
				title: String::new(),
				author: String::new(),
				url: String::new(),
			}
			.source_id(),
			None
		);
	}

	#[test]
	fn unnamed_kern_id_pins_parent_then_nonce_composition() {
		// `unnamed_kern_id` = content_hash(parent_id ++ nonce_nanos) with no
		// delimiter. Inserting one (e.g. `\x00`) passes the determinism test above
		// and orphans every kern in existence (ROADMAP item 77).
		assert_eq!(unnamed_kern_id("p", 42), util::content_hash("p42"));
		assert_eq!(unnamed_kern_id("root", 0), util::content_hash("root0"));
	}

	#[test]
	fn named_child_kern_id_pins_parent_name_nonce_composition() {
		// `named_child_kern_id` = content_hash(parent_id ++ name ++ nonce_nanos),
		// again delimiter-free. Reordering the fields (name before parent) or
		// inserting a separator passes the determinism test above and orphans
		// every named child (ROADMAP item 77).
		assert_eq!(
			named_child_kern_id("p", "code", 9),
			util::content_hash("pcode9")
		);
		assert_eq!(
			named_child_kern_id("root", "generic", 7),
			util::content_hash("rootgeneric7")
		);
	}

	#[test]
	fn observe_support_and_observe_contradict_stamp_updated_at() {
		let mut e = mk_entity("e", "x", 0.0, EntityKind::Fact);
		assert!(e.updated_at.is_none(), "fresh entity has no updated_at");
		e.observe_support(0.5);
		assert!(e.updated_at.is_some(), "observe_support stamps updated_at");
		// A sentinel beats sleeping on the wall clock: SystemTime is not
		// monotonic, so `now() > now()` is a race, not an assertion.
		e.updated_at = Some(SystemTime::UNIX_EPOCH);
		e.observe_contradict(0.5);
		assert!(
			e.updated_at > Some(SystemTime::UNIX_EPOCH),
			"observe_contradict stamps updated_at afresh"
		);
	}
}
