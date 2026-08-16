//! Tests extracted from graph_ops.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	#[test]
	fn link_vector_prefers_the_reason_embedding() {
		let v = link_vector(
			Some(vec![1.0, 2.0, 3.0]),
			&[0.0, 0.0, 0.0],
			&[9.0, 9.0, 9.0],
		);
		assert_eq!(
			v,
			vec![1.0, 2.0, 3.0],
			"an embedded reason wins over the midpoint"
		);
	}

	fn seed_thought(g: &mut GraphGnn, text: &str, kind: base::base_types::EntityKind) -> String {
		let root = g.root.id.clone();
		let mut t = base::base_types::Entity {
			id: util::content_hash(text),
			kind,
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
	fn audit_ranks_noise_and_secrets_and_archive_holds_them_pending() {
		let mut g = GraphGnn::new();
		let noise = seed_thought(
			&mut g,
			"npm warn deprecated foo@1.0.0",
			base::base_types::EntityKind::Claim,
		);
		let secret = seed_thought(
			&mut g,
			"staging deploy key AKIAIOSFODNN7EXAMPLE",
			base::base_types::EntityKind::Claim,
		);
		let value = seed_thought(
			&mut g,
			"we prefer LMDB over sqlite because the graph is the hot path",
			base::base_types::EntityKind::Claim,
		);

		let report = audit_noise(&g, 0.3, 10);
		assert_eq!(report.scanned, 3);
		let ids: Vec<&str> = report.candidates.iter().map(|c| c.id.as_str()).collect();
		assert!(ids.contains(&noise.as_str()));
		assert!(ids.contains(&secret.as_str()));
		assert!(
			!ids.contains(&value.as_str()),
			"value keyword clamps below 0.3"
		);
		assert_eq!(
			report.candidates[0].id, secret,
			"secret (0.9) ranks above terminal noise (0.85)"
		);
		assert_eq!(
			report.candidates[0].action,
			hygiene::SuggestedAction::Flag,
			"secrets are flagged for a human, never suggested for deletion"
		);

		let out = apply_audit(&mut g, 0.3, AuditAction::Archive);
		assert_eq!(out.archived, 2);
		let (held, _) = find_entity(&g, &noise).unwrap();
		assert_eq!(
			held.review,
			ReviewState::Pending,
			"archive = the curation hold"
		);
		assert!(
			promote_entity(&mut g, &noise).unwrap(),
			"the hold releases through the existing promote path"
		);
		// Idempotent: re-applying holds nothing new (secret row is still held).
		let again = apply_audit(&mut g, 0.3, AuditAction::Archive);
		assert_eq!(again.archived, 1, "only the just-promoted row is re-held");
	}

	#[test]
	fn audit_delete_honors_the_fact_guard_and_never_deletes_secrets() {
		let mut g = GraphGnn::new();
		let noise = seed_thought(
			&mut g,
			"Successfully installed requests-2.31.0",
			base::base_types::EntityKind::Claim,
		);
		let secret = seed_thought(
			&mut g,
			"conn postgres://svc:hunter2pass@db.internal/prod",
			base::base_types::EntityKind::Claim,
		);
		let noisy_fact = seed_thought(
			&mut g,
			"Requirement already satisfied: pip in ./venv",
			base::base_types::EntityKind::Fact,
		);

		let out = apply_audit(&mut g, 0.0, AuditAction::Delete);
		assert_eq!(out.deleted, 1);
		assert_eq!(out.secrets_kept, 1, "delete refuses the secret-bearing row");
		assert_eq!(out.kept_facts, 1, "the Fact guard holds in a bulk sweep");
		assert!(find_entity(&g, &noise).is_none());
		assert!(find_entity(&g, &secret).is_some());
		assert!(find_entity(&g, &noisy_fact).is_some());
	}

	#[test]
	fn audit_apply_floor_caps_min_score_from_below() {
		// Archive floor is 0.5: a 0.3 min_score must not archive a 0.4 row.
		assert_eq!(AuditAction::Archive.floor(), 0.5);
		assert_eq!(AuditAction::Delete.floor(), 0.8);
		let mut g = GraphGnn::new();
		// trivial keyword scores 0.7: archivable, below the delete floor.
		seed_thought(&mut g, "acknowledged", base::base_types::EntityKind::Claim);
		let del = apply_audit(&mut g, 0.0, AuditAction::Delete);
		assert_eq!(del.deleted, 0, "0.7 stays below the 0.8 delete floor");
		let arch = apply_audit(&mut g, 0.0, AuditAction::Archive);
		assert_eq!(arch.archived, 1);
	}

	#[test]
	fn link_vector_falls_back_to_endpoint_midpoint() {
		let v = link_vector(None, &[0.0, 2.0], &[4.0, 6.0]);
		assert_eq!(
			v,
			vec![2.0, 4.0],
			"no embedding -> midpoint of the two endpoints"
		);
		assert_eq!(
			v,
			vec![2.0, 4.0],
			"no embedding -> midpoint of the two endpoints"
		);
	}
}
