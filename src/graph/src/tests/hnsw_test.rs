//! Tests extracted from hnsw.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use math::cosine_distance as bf_cosine;
	use rand::{RngExt, SeedableRng};
	use std::collections::HashSet;
	use util::cmp_partial as bf_cmp;

	impl HnswIndex {
		fn arena_slots(&self) -> usize {
			self.nodes.len()
		}

		fn level_of(&self, id: &str) -> usize {
			let slot = self.slot_of[id];
			self.nodes[slot as usize]
				.as_ref()
				.expect("live node")
				.layers
				.len()
				- 1
		}
	}

	fn rand_vec(rng: &mut rand::rngs::StdRng, dim: usize) -> Vec<f32> {
		(0..dim).map(|_| rng.random::<f32>() * 2.0 - 1.0).collect()
	}

	fn brute_force_topk(vecs: &[(String, Vec<f32>)], q: &[f32], k: usize) -> HashSet<String> {
		let mut scored: Vec<(String, f64)> = vecs
			.iter()
			.map(|(id, v)| (id.clone(), bf_cosine(v, q)))
			.collect();
		scored.sort_by(|a, b| bf_cmp(&a.1, &b.1));
		scored.into_iter().take(k).map(|(id, _)| id).collect()
	}

	fn random_corpus(seed: u64, n: usize, dim: usize) -> Vec<(String, Vec<f32>)> {
		let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
		(0..n)
			.map(|i| (format!("v{i}"), rand_vec(&mut rng, dim)))
			.collect()
	}

	#[test]
	fn node_level_depends_only_on_id_not_insert_order() {
		let corpus = random_corpus(41, 300, 16);
		let mut fwd = HnswIndex::new(16, 128);
		let mut rev = HnswIndex::new(16, 128);
		for (id, v) in &corpus {
			fwd.insert(id.clone(), v.clone().into());
		}
		for (id, v) in corpus.iter().rev() {
			rev.insert(id.clone(), v.clone().into());
		}
		for (id, _) in &corpus {
			assert_eq!(
				fwd.level_of(id),
				rev.level_of(id),
				"level of {id} depends on insert position, not id"
			);
		}
	}

	#[test]
	fn identical_insert_sequence_builds_identical_graph() {
		let corpus = random_corpus(42, 300, 16);
		let build = || {
			let mut idx = HnswIndex::new(16, 128);
			for (id, v) in &corpus {
				idx.insert(id.clone(), v.clone().into());
			}
			idx.structure_digest()
		};
		assert_eq!(build(), build(), "same insert sequence, different graph");
	}

	#[test]
	fn empty_index_returns_nothing() {
		let idx = HnswIndex::new(8, 64);
		assert!(idx.is_empty());
		assert!(idx.search(&[1.0, 0.0], 5, 16).is_empty());
	}

	#[test]
	fn inserts_and_finds_exact_nearest() {
		let mut idx = HnswIndex::new(8, 64);
		idx.insert("x".into(), vec![1.0, 0.0, 0.0].into());
		idx.insert("y".into(), vec![0.0, 1.0, 0.0].into());
		idx.insert("z".into(), vec![0.0, 0.0, 1.0].into());
		let hits = idx.search(&[0.9, 0.1, 0.0], 1, 16);
		assert_eq!(hits[0].id, "x", "nearest by cosine is x");
	}

	#[test]
	fn delete_removes_node_from_results() {
		let mut idx = HnswIndex::new(8, 64);
		idx.insert("x".into(), vec![1.0, 0.0].into());
		idx.insert("y".into(), vec![0.0, 1.0].into());
		idx.delete("x");
		assert!(idx.search(&[1.0, 0.0], 5, 16).iter().all(|h| h.id != "x"));
	}

	#[test]
	fn delete_then_insert_reuses_slot_and_search_stays_correct() {
		let dim = 24;
		let corpus = random_corpus(3, 200, dim);
		let mut idx = HnswIndex::new(16, 128);
		for (id, v) in &corpus {
			idx.insert(id.clone(), v.clone().into());
		}
		let slots_before = idx.arena_slots();

		let mut live: Vec<(String, Vec<f32>)> = corpus.clone();
		let mut rng = rand::rngs::StdRng::seed_from_u64(1234);
		for i in 0..40 {
			let victim = live.remove(rng.random_range(0..live.len()));
			idx.delete(&victim.0);
			let nv = rand_vec(&mut rng, dim);
			let nid = format!("new{i}");
			idx.insert(nid.clone(), nv.clone().into());
			live.push((nid, nv));
		}

		assert_eq!(
			idx.arena_slots(),
			slots_before,
			"deleted slots were recycled, arena did not grow"
		);
		assert_eq!(idx.len(), live.len(), "live count tracks the churn");

		let mut qrng = rand::rngs::StdRng::seed_from_u64(77);
		let k = 8;
		let mut total = 0.0;
		for _ in 0..25 {
			let q = rand_vec(&mut qrng, dim);
			let truth = brute_force_topk(&live, &q, k);
			let got: HashSet<String> = idx.search(&q, k, 128).into_iter().map(|h| h.id).collect();
			assert!(
				got.iter().all(|id| live.iter().any(|(lid, _)| lid == id)),
				"a recycled/deleted id leaked into results"
			);
			total += truth.intersection(&got).count() as f64 / k as f64;
		}
		let recall = total / 25.0;
		assert!(recall >= 0.85, "recall after churn too low: {recall:.3}");
	}

	#[test]
	fn recall_matches_brute_force() {
		let dim = 32;
		let corpus = random_corpus(7, 300, dim);
		let mut idx = HnswIndex::new(16, 128);
		for (id, v) in &corpus {
			idx.insert(id.clone(), v.clone().into());
		}
		let k = 10;
		let queries = 25;
		let mut qrng = rand::rngs::StdRng::seed_from_u64(99);
		let mut total = 0.0;
		for _ in 0..queries {
			let q = rand_vec(&mut qrng, dim);
			let truth = brute_force_topk(&corpus, &q, k);
			let got: HashSet<String> = idx.search(&q, k, 128).into_iter().map(|h| h.id).collect();
			total += truth.intersection(&got).count() as f64 / k as f64;
		}
		let recall = total / queries as f64;
		assert!(recall >= 0.85, "HNSW recall@{k} too low: {recall:.3}");
	}

	#[test]
	fn search_order_matches_brute_force_on_separated_corpus() {
		let dim = 48;
		let corpus = random_corpus(2024, 400, dim);
		let mut idx = HnswIndex::new(24, 200);
		for (id, v) in &corpus {
			idx.insert(id.clone(), v.clone().into());
		}
		let k = 5;
		let mut qrng = rand::rngs::StdRng::seed_from_u64(2025);
		let mut matched = 0;
		let queries = 30;
		for _ in 0..queries {
			let q = rand_vec(&mut qrng, dim);
			let mut scored: Vec<(String, f64)> = corpus
				.iter()
				.map(|(id, v)| (id.clone(), bf_cosine(v, &q)))
				.collect();
			scored.sort_by(|a, b| bf_cmp(&a.1, &b.1));
			let truth: Vec<String> = scored.into_iter().take(k).map(|(id, _)| id).collect();
			let got: Vec<String> = idx.search(&q, k, 256).into_iter().map(|h| h.id).collect();
			if got == truth {
				matched += 1;
			}
		}
		assert!(
			matched >= queries - 2,
			"exact-order match on separated corpus: {matched}/{queries}"
		);
	}

	#[test]
	fn search_filtered_matches_brute_force_over_subset() {
		let dim = 16;
		let corpus = random_corpus(21, 240, dim);
		let mut idx = HnswIndex::new(16, 128);
		for (id, v) in &corpus {
			idx.insert(id.clone(), v.clone().into());
		}
		let keep = |id: &str| {
			id.trim_start_matches('v')
				.parse::<usize>()
				.map(|n| n % 2 == 0)
				.unwrap_or(false)
		};
		let subset: Vec<(String, Vec<f32>)> =
			corpus.iter().filter(|(id, _)| keep(id)).cloned().collect();

		let k = 8;
		let queries = 25;
		let mut qrng = rand::rngs::StdRng::seed_from_u64(55);
		let mut total = 0.0;
		for _ in 0..queries {
			let q = rand_vec(&mut qrng, dim);
			let truth = brute_force_topk(&subset, &q, k);
			let hits = idx.search_filtered(&q, k, 128, &keep);
			assert_eq!(
				hits.len(),
				k,
				"filtered search returned fewer than k matches"
			);
			let got: HashSet<String> = hits.into_iter().map(|h| h.id).collect();
			assert!(
				got.iter().all(|id| keep(id)),
				"filtered search returned a non-matching id"
			);
			total += truth.intersection(&got).count() as f64 / k as f64;
		}
		let recall = total / queries as f64;
		assert!(recall >= 0.85, "filtered recall@{k} too low: {recall:.3}");
	}

	#[test]
	fn search_filtered_reject_all_is_empty() {
		let mut idx = HnswIndex::new(8, 64);
		idx.insert("a".into(), vec![1.0, 0.0].into());
		idx.insert("b".into(), vec![0.0, 1.0].into());
		assert!(idx
			.search_filtered(&[1.0, 0.0], 5, 32, &|_| false)
			.is_empty());
	}

	#[test]
	fn search_filtered_finds_single_rare_match() {
		let dim = 16;
		let corpus = random_corpus(8, 200, dim);
		let mut idx = HnswIndex::new(16, 128);
		for (id, v) in &corpus {
			idx.insert(id.clone(), v.clone().into());
		}
		let target = "v137";
		let qv = corpus
			.iter()
			.find(|(id, _)| id == target)
			.map(|(_, v)| v.clone())
			.unwrap();
		let hits = idx.search_filtered(&qv, 5, 128, &|id| id == target);
		assert_eq!(hits.len(), 1, "the one matching node is found");
		assert_eq!(hits[0].id, target);
	}

	#[test]
	fn int8_recall_tracks_f64() {
		let dim = 32;
		let corpus = random_corpus(13, 300, dim);
		let mut f64_idx = HnswIndex::new(16, 128);
		let mut i8_idx = HnswIndex::with_mode(16, 128, QuantizationMode::Int8);
		for (id, v) in &corpus {
			f64_idx.insert(id.clone(), v.clone().into());
			i8_idx.insert(id.clone(), v.clone().into());
		}
		let k = 10;
		let queries = 25;
		let mut qrng = rand::rngs::StdRng::seed_from_u64(123);
		let mut total = 0.0;
		for _ in 0..queries {
			let q = rand_vec(&mut qrng, dim);
			let f: HashSet<String> = f64_idx
				.search(&q, k, 128)
				.into_iter()
				.map(|h| h.id)
				.collect();
			let i: HashSet<String> = i8_idx
				.search(&q, k, 128)
				.into_iter()
				.map(|h| h.id)
				.collect();
			total += f.intersection(&i).count() as f64 / k as f64;
		}
		let agreement = total / queries as f64;
		assert!(
			agreement >= 0.75,
			"int8 vs f64 top-{k} agreement too low: {agreement:.3}"
		);
	}

	#[test]
	fn binary_recall_tracks_f64() {
		let dim = 32;
		let corpus = random_corpus(13, 300, dim);
		let mut f64_idx = HnswIndex::new(16, 128);
		let mut bin_idx = HnswIndex::with_mode(16, 128, QuantizationMode::Binary);
		for (id, v) in &corpus {
			f64_idx.insert(id.clone(), v.clone().into());
			bin_idx.insert(id.clone(), v.clone().into());
		}
		let k = 10;
		let queries = 25;
		let mut qrng = rand::rngs::StdRng::seed_from_u64(123);
		let mut total = 0.0;
		for _ in 0..queries {
			let q = rand_vec(&mut qrng, dim);
			let f: HashSet<String> = f64_idx
				.search(&q, k, 128)
				.into_iter()
				.map(|h| h.id)
				.collect();
			let b: HashSet<String> = bin_idx
				.search(&q, k, 128)
				.into_iter()
				.map(|h| h.id)
				.collect();
			total += f.intersection(&b).count() as f64 / k as f64;
		}
		let agreement = total / queries as f64;
		// The 0.30 floor locks the measured no-rescore behaviour; rescore must lift
		// it before Binary becomes user-selectable (numbers in the splinter note).
		assert!(
			agreement >= 0.30,
			"binary vs f64 top-{k} agreement below floor: {agreement:.3}"
		);
	}
	#[test]
	fn a_deleted_slot_is_not_reusable_until_its_inbound_edges_are_scrubbed() {
		// The whole safety argument for deferring the scrub: a slot may sit dead
		// with edges still pointing at it, but it must not be handed to a new id
		// while they do — that is how a stale edge starts aliasing a live node.
		let mut ix = HnswIndex::new(8, 100);
		for i in 0..12 {
			ix.insert(
				format!("e{i}"),
				rand_vec(&mut rand::SeedableRng::seed_from_u64(i), 8).into(),
			);
		}
		let before = ix.len();

		ix.delete("e5");
		assert_eq!(ix.len(), before - 1, "a deleted node is immediately gone");
		assert!(
			ix.free.is_empty(),
			"the slot must NOT be free while inbound edges may still name it"
		);
		assert_eq!(ix.pending_scrub.len(), 1, "it is queued for the next pass");

		// The next insert drains the queue before it can take the slot.
		ix.insert(
			"fresh".into(),
			rand_vec(&mut rand::SeedableRng::seed_from_u64(99), 8).into(),
		);
		assert!(
			ix.pending_scrub.is_empty(),
			"allocating a slot must scrub first"
		);
		let dead = ix.nodes.iter().flatten().any(|n| {
			n.layers
				.iter()
				.any(|l| l.iter().any(|&s| ix.id_of.get(s as usize).is_none()))
		});
		assert!(!dead, "no edge points outside the arena");
	}

	#[test]
	fn one_scrub_pass_clears_every_slot_deleted_since_the_last_one() {
		// The cost this closes: scrubbing per delete made a GC sweep pay
		// O(victims x nodes x edges). A sweep now pays one pass total.
		let mut ix = HnswIndex::new(8, 100);
		for i in 0..12 {
			ix.insert(
				format!("e{i}"),
				rand_vec(&mut rand::SeedableRng::seed_from_u64(i), 8).into(),
			);
		}
		for i in [2u64, 4, 6, 8] {
			ix.delete(&format!("e{i}"));
		}
		assert_eq!(ix.pending_scrub.len(), 4, "all four wait for one pass");

		ix.insert(
			"fresh".into(),
			rand_vec(&mut rand::SeedableRng::seed_from_u64(77), 8).into(),
		);

		assert!(ix.pending_scrub.is_empty(), "one pass drained all four");
		let live: std::collections::HashSet<u32> = (0..ix.nodes.len() as u32)
			.filter(|&s| ix.nodes[s as usize].is_some())
			.collect();
		for n in ix.nodes.iter().flatten() {
			for l in &n.layers {
				for s in l {
					assert!(live.contains(s), "edge to slot {s} survived the scrub");
				}
			}
		}
	}

	#[test]
	fn structure_digest_pins_canon_layout_for_a_single_node() {
		// `structure_digest` feeds `content_hash`; the canon string is
		// `ep={id};max={layer}\n{id}|\n` for one level-0 node. A delimiter or
		// layout change in the canon moves the digest and with it every stored
		// import guard that checks it (ROADMAP item 77). The level of "a" under
		// `new(16, 128)` is 0 (FNV level_for, pinned by this test's expected
		// string), so the canon is exactly the single-line form below.
		let mut idx = HnswIndex::new(16, 128);
		idx.insert("a".to_string(), vec![0.0_f32].into());
		let canon = "ep=a;max=0\na|\n";
		assert_eq!(idx.structure_digest(), content_hash(canon));
	}
}
