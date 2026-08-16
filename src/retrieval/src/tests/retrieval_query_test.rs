//! Tests extracted from retrieval_query.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use base::base_types::{mk_entity, EntityKind, Kern, Reason, ReasonKind};
	use graph::reason::add_reason;

	// ROADMAP item 94. A dedup keeps the incoming wording on a `Rephrase` reason
	// and nothing else, so the exact phrasing a user might search for sat in the
	// store and in neither index. The corpus is sized past `seed_k * 2` on purpose:
	// with a handful of entities the dense seed returns everything and the gap is
	// invisible, which is why the probe has to make the survivor un-seedable by
	// vector before it can prove anything about the lexical one.
	fn deduped_corpus() -> GraphGnn {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		{
			let k = g.kerns.get_mut(&root).expect("root kern");
			let mut s = mk_entity(
				"survivor",
				"ada keeps her bicycle in the shed",
				1.0,
				EntityKind::Claim,
			);
			// Related to the query but not near it: 20 fillers sit closer, so the
			// survivor is never a dense seed. This is the shape item 94 is about —
			// the entity only an exact rare term can reach.
			s.vector = vec![1.0, 0.45].into();
			s.gnn_vector = vec![1.0, 0.45].into();
			k.entities.insert("survivor".into(), s);
			for i in 0..20 {
				let id = format!("decoy{i}");
				let mut d = mk_entity(
					&id,
					&format!("unrelated filler statement number {i}"),
					1.0,
					EntityKind::Claim,
				);
				let t = 0.001 * i as f32;
				d.vector = vec![t, 1.0].into();
				d.gnn_vector = vec![t, 1.0].into();
				k.entities.insert(id, d);
			}
		}
		g.index_entity("survivor", &root);
		for i in 0..20 {
			g.index_entity(&format!("decoy{i}"), &root);
		}
		g.rebuild_index();
		g.lexical()
			.expect("in-ram lexical index")
			.rebuild_from_graph(&g);

		graph::accept::merge_duplicate(
			&mut g,
			"survivor",
			"ada stores her velocipede in the outbuilding",
			1.0,
			EntityKind::Claim,
			None,
		)
		.expect("the near-duplicate merges onto the survivor");
		g
	}

	fn retrieved_ids(g: &GraphGnn, query_text: &str) -> Vec<String> {
		let cfg = config::RetrievalConfig {
			// The fixture has no edges, so PageRank's dangling mass spreads evenly over
			// the whole corpus and seeds the survivor for ANY query — it would hide the
			// one seed source this test is about.
			pagerank_enabled: false,
			..Default::default()
		};
		let w = Weights {
			content: 0.70,
			reason: 0.15,
			edge: 0.15,
		};
		// A short query does not embed onto the document it is about; the vector
		// here points at the filler field, so the survivor can only arrive lexically.
		retrieve(g, &cfg, &[0.0, 1.0], query_text, Mode::Hybrid, None, w)
			.results
			.into_iter()
			.map(|r| r.entity.id)
			.collect()
	}

	#[test]
	fn a_query_in_the_merged_away_wording_finds_the_survivor() {
		let g = deduped_corpus();
		let kid = g.kern_of_entity("survivor").unwrap().to_string();
		assert!(
			g.loaded(&kid)
				.unwrap()
				.reasons
				.values()
				.any(|r| r.kind == ReasonKind::Rephrase && r.text.contains("velocipede")),
			"precondition: the merged-away wording is stored on the survivor"
		);
		assert!(
			!retrieved_ids(&g, "zzznolexicalmatch").contains(&"survivor".to_string()),
			"precondition: 20 fillers sit nearer this query vector, so the survivor is \
			 no dense seed — anything that finds it now arrived through the lexical index"
		);

		let ids = retrieved_ids(&g, "velocipede outbuilding");
		assert!(
			ids.contains(&"survivor".to_string()),
			"a query phrased in the merged document's own words must reach the \
			 survivor that swallowed it: {ids:?}"
		);
	}

	#[test]
	fn an_entity_matching_both_wordings_is_delivered_once() {
		let g = deduped_corpus();
		let lex = g.lexical().unwrap();

		// The alternate wording is a posting on the SURVIVOR's document, not a
		// document of its own — so it answers under the survivor's id.
		let alt = lex.search("velocipede outbuilding", 10);
		assert_eq!(
			alt.iter().map(|h| h.entity_id.as_str()).collect::<Vec<_>>(),
			vec!["survivor"],
			"the alternate wording answers as the survivor, exactly once"
		);

		// The case a second document per wording would double.
		let both = lex.search("bicycle shed velocipede outbuilding", 10);
		assert_eq!(
			both
				.iter()
				.map(|h| h.entity_id.as_str())
				.collect::<Vec<_>>(),
			vec!["survivor"],
			"and a query hitting BOTH wordings still returns one row, not two"
		);

		let ids = retrieved_ids(&g, "bicycle shed velocipede outbuilding");
		assert_eq!(
			ids.iter().filter(|id| *id == "survivor").count(),
			1,
			"delivery carries it once, not once per matching wording: {ids:?}"
		);
	}

	#[test]
	fn lexical_top_boost_pins_a_verbatim_match_to_the_top_past_higher_cosine_decoys() {
		// The query vector points at the filler field, so the 20 decoys outrank the
		// survivor by content score alone. With `lexical_top_boost` on, the
		// survivor's verbatim BM25 overlap must lift it to #1 of the delivered list
		// — the post-MMR re-sort is what makes the bonus visible past diversity.
		let g = deduped_corpus();
		let cfg = config::RetrievalConfig {
			pagerank_enabled: false,
			lexical_top_boost: 1.0,
			..Default::default()
		};
		let w = Weights {
			content: 0.70,
			reason: 0.15,
			edge: 0.15,
		};
		let ids = retrieve(&g, &cfg, &[0.0, 1.0], "bicycle shed", Mode::Hybrid, None, w)
			.results
			.into_iter()
			.map(|r| r.entity.id)
			.collect::<Vec<_>>();
		assert!(
			!ids.is_empty(),
			"precondition: the query delivered something: {ids:?}"
		);
		assert_eq!(
			ids.first(),
			Some(&"survivor".to_string()),
			"the verbatim-lexical match wins the top over higher-cosine decoys: {ids:?}"
		);

		// And the same query without the boost leaves the survivor buried — the
		// decoys' content score wins. This is the counterfactual that proves the
		// boost is doing the work, not the seed.
		let cfg_off = config::RetrievalConfig {
			pagerank_enabled: false,
			lexical_top_boost: 0.0,
			..Default::default()
		};
		let ids_off = retrieve(
			&g,
			&cfg_off,
			&[0.0, 1.0],
			"bicycle shed",
			Mode::Hybrid,
			None,
			w,
		)
		.results
		.into_iter()
		.map(|r| r.entity.id)
		.collect::<Vec<_>>();
		assert_ne!(
			ids_off.first(),
			Some(&"survivor".to_string()),
			"without the boost the cosine-dominant decoys keep the top: {ids_off:?}"
		);
	}

	#[test]
	fn format_chains_renders_entities_and_reason_labels() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("k", "");
		k.entities.insert(
			"e1".into(),
			mk_entity("e1", "alpha", 0.0, EntityKind::Claim),
		);
		k.entities
			.insert("e2".into(), mk_entity("e2", "beta", 0.0, EntityKind::Claim));
		add_reason(
			&mut k,
			Reason {
				from: "e1".into(),
				to: "e2".into(),
				id: "r1".into(),
				text: "supports".into(),
				kind: ReasonKind::Similarity,
				..Default::default()
			},
		);
		g.kerns.insert("k".into(), k);

		let chains = [PathChain {
			nodes: vec!["e1".into(), "r1".into(), "e2".into()],
			score: 1.0,
		}];
		let out = format_chains(&g, &chains);
		assert!(out.contains("Chain 1:"));
		assert!(out.contains("[Entity] alpha"));
		assert!(out.contains("[Entity] beta"));
		assert!(
			out.contains("--supports-->"),
			"reason text used as the edge label: {out}"
		);
	}

	#[test]
	fn query_locked_is_read_only_and_defers_the_access_stamp() {
		use graph::accept;
		use parking_lot::RwLock;

		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		let mut e = mk_entity("hot", "the sky is blue", 0.0, EntityKind::Claim);
		e.vector = vec![1.0, 0.0, 0.0, 0.0].into();
		accept::accept(&mut g, &root, e, "");
		let graph = RwLock::new(g);

		let cfg = RetrievalConfig::default();
		let result = query_locked(
			&graph,
			&cfg,
			&HeatConfig::default(),
			&[1.0, 0.0, 0.0, 0.0],
			"sky",
			crate::retrieval::seed::Mode::Content,
			None,
		);
		assert!(!result.entities.is_empty(), "the entity is retrieved");
		assert!(
			result.entities.iter().any(|s| s.entity.id == "hot"),
			"the caller gets the retrieved id so it can enqueue the deferred stamp"
		);

		let g = graph.read();
		let (live, _) = find_entity(&g, "hot").expect("entity still live");
		assert!(
			live.accessed_at.is_none(),
			"query_locked does NOT stamp the live graph — the write-back is deferred"
		);
		assert_eq!(
			live.access_count.value(),
			0,
			"no inline write lock: the live access counter is untouched by the read path"
		);
	}

	#[test]
	fn retrieve_drops_an_expired_claim_from_the_default_path() {
		// Pins the CALL SITE, not the predicate: the unit tests on `drop_expired`
		// pass unchanged if the call in `retrieve` is deleted, which is exactly how
		// `valid_until` came to be honoured by a function nothing invoked.
		use std::time::{Duration, SystemTime};
		let now = SystemTime::now();
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		{
			let k = g.kerns.get_mut(&root).expect("root kern");
			for (id, ttl) in [
				("live", Some(now + Duration::from_secs(3600))),
				("expired", Some(now - Duration::from_secs(3600))),
			] {
				let mut e = mk_entity(
					id,
					"ada keeps her bicycle in the shed",
					1.0,
					EntityKind::Claim,
				);
				e.vector = vec![1.0, 0.0].into();
				e.gnn_vector = vec![1.0, 0.0].into();
				e.valid_until = ttl;
				k.entities.insert(id.into(), e);
			}
		}
		for id in ["live", "expired"] {
			g.index_entity(id, &root);
		}
		g.rebuild_index();

		let cfg = config::RetrievalConfig::default();
		let w = Weights {
			content: 0.70,
			reason: 0.15,
			edge: 0.15,
		};
		let out = retrieve(&g, &cfg, &[1.0, 0.0], "ada bicycle", Mode::Hybrid, None, w);

		let ids: Vec<&str> = out.results.iter().map(|r| r.entity.id.as_str()).collect();
		assert!(
			ids.contains(&"live"),
			"precondition: the live claim is retrieved"
		);
		assert!(
			!ids.contains(&"expired"),
			"an expired claim must not reach delivery: {ids:?}"
		);

		// Same corpus, same call site, one instant named: expiry is for the
		// implicit "now", so a point-in-time query must still see the history.
		let opts = crate::retrieval::score::QueryOptions {
			as_of: Some(now - Duration::from_secs(7200)),
			..Default::default()
		};
		let out = retrieve(
			&g,
			&cfg,
			&[1.0, 0.0],
			"ada bicycle",
			Mode::Hybrid,
			Some(&opts),
			w,
		);
		let ids: Vec<&str> = out.results.iter().map(|r| r.entity.id.as_str()).collect();
		assert!(
			ids.contains(&"expired"),
			"a query that names its own instant judges validity THERE — dropping the \
			 since-expired claim would make history unqueryable: {ids:?}"
		);
	}

	// A chain is a SECOND delivery channel: `format_chains` renders the text of
	// every entity on the path, and nothing about it is a result. Filtering only
	// `results` left the filter stopping the row and the chain printing it
	// anyway — the filter would read as applied while filtering nothing.
	#[test]
	fn a_filtered_entity_does_not_leak_through_a_path_chain() {
		let mut g = GraphGnn::new();
		let root = g.root.id.clone();
		{
			let k = g.kerns.get_mut(&root).expect("root kern");
			let mut open = mk_entity(
				"open",
				"ada keeps her bicycle in the shed",
				1.0,
				EntityKind::Claim,
			);
			open.vector = vec![1.0, 0.0].into();
			open.gnn_vector = vec![1.0, 0.0].into();
			k.entities.insert("open".into(), open);

			// Orthogonal to the query, so it is never a SEED — the only way it can
			// enter the walk is by the edge, which is exactly the path that builds a
			// chain and the path the filter predicate has to cover.
			let mut secret = mk_entity(
				"secret",
				"the vault code is 4815162342",
				1.0,
				EntityKind::Document,
			);
			secret.vector = vec![0.0, 1.0].into();
			secret.gnn_vector = vec![0.0, 1.0].into();
			k.entities.insert("secret".into(), secret);

			add_reason(
				k,
				Reason {
					from: "open".into(),
					to: "secret".into(),
					id: "r1".into(),
					text: "relates to".into(),
					kind: ReasonKind::Similarity,
					score: 0.9,
					..Default::default()
				},
			);
		}
		for id in ["open", "secret"] {
			g.index_entity(id, &root);
		}
		g.rebuild_index();

		let cfg = config::RetrievalConfig::default();
		let w = Weights {
			content: 0.70,
			reason: 0.15,
			edge: 0.15,
		};

		// Precondition: unfiltered, the walk reaches the scoped thought and prints it.
		let open_read = retrieve(
			&g,
			&cfg,
			&[1.0, 0.0],
			"ada bicycle shed",
			Mode::Hybrid,
			None,
			w,
		);
		assert!(
			open_read.chain_text.contains("vault code"),
			"precondition: the walk does reach it and the chain does render its text: {:?}",
			open_read.chain_text
		);

		let claims_only = crate::retrieval::score::QueryOptions {
			kind: Some(EntityKind::Claim),
			..Default::default()
		};
		let out = retrieve(
			&g,
			&cfg,
			&[1.0, 0.0],
			"ada bicycle shed",
			Mode::Hybrid,
			Some(&claims_only),
			w,
		);
		let ids: Vec<&str> = out.results.iter().map(|r| r.entity.id.as_str()).collect();
		assert!(
			!ids.contains(&"secret"),
			"the filtered thought is dropped from the results: {ids:?}"
		);
		assert!(
			!out.chain_text.contains("vault code"),
			"and from the chains, which render text and answer to no result filter: {:?}",
			out.chain_text
		);
	}
}
mod fuse_tests {
	use super::*;

	fn hit(id: &str) -> EntityHit {
		EntityHit {
			entity_id: id.into(),
			score: 0.0,
		}
	}

	#[test]
	fn empty_weights_recovers_unweighted_rrf() {
		let a = [hit("x"), hit("y")];
		let b = [hit("y"), hit("z")];
		let lists: Vec<&[EntityHit]> = vec![&a, &b];
		let out = rrf(&lists, &[], 60.0, 10);
		assert_eq!(out[0].entity_id, "y", "y in both lists sorts first");
	}

	#[test]
	fn global_list_downweight_sinks_popular_irrelevant_entity() {
		let dense = [hit("rel")];
		let global = [hit("pop")];
		let lists: Vec<&[EntityHit]> = vec![&dense, &global];

		let unweighted = rrf(&lists, &[1.0, 1.0], 60.0, 10);
		assert_eq!(unweighted[0].entity_id, "pop", "equal weights: id tiebreak");

		let weighted = rrf(&lists, &[1.0, 0.5], 60.0, 10);
		assert_eq!(weighted[0].entity_id, "rel", "down-weighted global sinks");
		assert!(
			weighted[0].score > weighted[1].score,
			"rel strictly above pop"
		);
	}

	#[test]
	fn missing_weight_defaults_to_one() {
		let a = [hit("x")];
		let b = [hit("x")];
		let lists: Vec<&[EntityHit]> = vec![&a, &b];
		let out = rrf(&lists, &[1.0], 60.0, 10);
		let both = rrf(&lists, &[1.0, 1.0], 60.0, 10);
		assert_eq!(out[0].score, both[0].score, "missing weight == 1.0");
	}

	#[test]
	fn equal_score_tie_broken_by_id_ascending_under_top_k() {
		let la = [hit("b")];
		let lb = [hit("a")];
		let lists: Vec<&[EntityHit]> = vec![&la, &lb];
		let out = rrf(&lists, &[1.0, 1.0], 60.0, 1);
		assert_eq!(out.len(), 1, "top_k=1 keeps a single hit");
		assert_eq!(
			out[0].entity_id, "a",
			"tie resolved to id-ascending winner under truncation"
		);
	}

	#[test]
	fn top_k_truncates_and_zero_is_empty_without_panicking() {
		let a = [hit("x"), hit("y"), hit("z")];
		let lists: Vec<&[EntityHit]> = vec![&a];

		assert!(rrf(&lists, &[], 60.0, 0).is_empty(), "top_k=0 is empty");
		assert_eq!(rrf(&lists, &[], 60.0, 2).len(), 2, "truncates to top_k");
		assert_eq!(
			rrf(&lists, &[], 60.0, 99).len(),
			3,
			"top_k over count returns all"
		);
	}
}
mod merge_tests {
	use super::*;
	use base::base_types::Kern;

	use base::base_types::Entity;
	use test_support::entity as ent;
	fn hit(id: &str, score: f64) -> EntityHit {
		EntityHit {
			entity_id: id.into(),
			score,
		}
	}
	fn scored(entity: &Entity, score: f64) -> ScoredRef<'_> {
		ScoredRef { entity, score }
	}
	fn find<'a, 'g>(rs: &'a [ScoredRef<'g>], id: &str) -> Option<&'a ScoredRef<'g>> {
		rs.iter().find(|s| s.entity.id == id)
	}

	#[test]
	fn entity_seen_in_both_sources_outranks_one_seen_once() {
		let g = GraphGnn::new();
		let (ea, eb) = (ent("a"), ent("b"));
		let beam = vec![scored(&ea, 0.5), scored(&eb, 0.5)];
		let seeds = [hit("a", 0.5)];
		let out = merge_results(&g, &seeds, beam);

		let a = find(&out, "a").expect("a present");
		let b = find(&out, "b").expect("b present");
		assert!(
			a.score > b.score,
			"corroborated a ({}) > lone b ({})",
			a.score,
			b.score
		);
		assert!((a.score - (0.5 + std::f64::consts::LN_2)).abs() < 1e-9);
		assert!((b.score - 0.5).abs() < 1e-9);
		assert_eq!(out[0].entity.id, "a", "higher score sorts first");
	}

	#[test]
	fn seed_absent_from_graph_and_beam_is_silently_skipped() {
		let g = GraphGnn::new();
		let eb = ent("b");
		let beam = vec![scored(&eb, 0.5)];
		let seeds = [hit("ghost", 0.9)];
		let out = merge_results(&g, &seeds, beam);

		assert!(find(&out, "ghost").is_none(), "unresolvable seed dropped");
		assert_eq!(out.len(), 1, "only the beam entity survives");
		assert_eq!(out[0].entity.id, "b");
	}

	#[test]
	fn seed_only_entity_is_pulled_from_the_graph() {
		let mut g = GraphGnn::new();
		let mut k = Kern::new("kx", "");
		k.entities.insert("c".into(), ent("c"));
		g.kerns.insert("kx".into(), k);

		let out = merge_results(&g, &[hit("c", 0.7)], Vec::new());
		let c = find(&out, "c").expect("seed resolved from graph");
		assert!((c.score - 0.7).abs() < 1e-9, "single observation unchanged");
	}
}
mod gravity_tests {
	use super::*;
	use crate::retrieval::expand::ScoredEntity;
	use base::base_types::{mk_entity, EntityKind};
	use graph::accept::add_graviton_with_mass;

	fn scored(id: &str, vector: Vec<f32>, score: f64) -> ScoredEntity {
		let mut entity = mk_entity(id, "t", 0.5, EntityKind::Claim);
		entity.vector = vector.into();
		ScoredEntity { entity, score }
	}

	fn graph_with_graviton(mass: f64) -> GraphGnn {
		let mut g = GraphGnn::new();
		add_graviton_with_mass(&mut g, "work", vec![1.0, 0.0, 0.0], 1.0);
		let id = root_graviton_ids(&g).pop().unwrap();
		g.get_mut(&id).unwrap().mass = mass;
		g
	}

	#[test]
	fn graviton_near_entity_outranks_graviton_far_at_equal_base_score() {
		let g = graph_with_graviton(1.0);
		let cfg = RetrievalConfig::default();
		let mut results = vec![
			scored("far", vec![0.0, 1.0, 0.0], 1.0),
			scored("near", vec![1.0, 0.0, 0.0], 1.0),
			scored("novec", Vec::new(), 1.0),
		];
		apply_gravity(&g, &cfg, &mut results);
		let get = |id: &str| results.iter().find(|r| r.entity.id == id).unwrap().score;
		assert!(
			get("near") > get("far"),
			"near {} must outrank far {}",
			get("near"),
			get("far")
		);
		assert_eq!(get("far"), 1.0, "orthogonal cosine -> no boost");
		assert_eq!(get("novec"), 1.0, "empty entity vector is skipped");
	}

	#[test]
	fn mass_two_pulls_harder_than_mass_one() {
		let cfg = RetrievalConfig::default();
		let boost = |mass: f64| {
			let g = graph_with_graviton(mass);
			let mut results = vec![scored("e", vec![1.0, 0.0, 0.0], 1.0)];
			apply_gravity(&g, &cfg, &mut results);
			results[0].score - 1.0
		};
		let (b1, b2) = (boost(1.0), boost(2.0));
		assert!(b1 > 0.0, "mass 1 boosts at all: {b1}");
		assert!(
			(b2 - 2.0 * b1).abs() < 1e-9,
			"mass scales the pull linearly: {b2} vs 2*{b1}"
		);
	}

	#[test]
	fn gravity_weight_zero_changes_nothing() {
		let g = graph_with_graviton(1.0);
		let cfg = RetrievalConfig {
			gravity_weight: 0.0,
			..Default::default()
		};
		let mut results = vec![scored("near", vec![1.0, 0.0, 0.0], 1.0)];
		apply_gravity(&g, &cfg, &mut results);
		assert_eq!(results[0].score, 1.0);
	}

	#[test]
	fn overlapping_gravitons_take_the_max_not_the_sum() {
		let mut g = graph_with_graviton(1.0);
		add_graviton_with_mass(&mut g, "also-work", vec![1.0, 0.0, 0.0], 1.0);
		let cfg = RetrievalConfig::default();
		let mut results = vec![scored("e", vec![1.0, 0.0, 0.0], 1.0)];
		apply_gravity(&g, &cfg, &mut results);
		let boost = results[0].score - 1.0;
		assert!(
			(boost - cfg.gravity_weight).abs() < 1e-6,
			"two identical unit gravitons boost once, got {boost}"
		);
	}
}
