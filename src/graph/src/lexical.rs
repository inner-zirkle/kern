//! The BM25 lexical index over entity documents (an entity's statements plus
//! its rephrase wordings), kept incrementally in sync with the graph — the
//! exact-term recall leg that embedding similarity alone would miss.

use super::graph::GraphGnn;
use base::base_types::{Entity, Kern, ReasonKind};
use std::collections::HashMap;
use std::sync::RwLock;

// An entity's lexical document: its own statements, then every alternate wording
// a dedup merged onto it and parked on a `Rephrase` reason.
//
// ONE document per entity id, not one per wording. The index is keyed by entity
// id and `inner_insert` replaces, so a second `insert` under the same id would
// drop the primary wording; a second insert under the REASON's id would put the
// same entity in the BM25 result twice. Appending keeps the entity single, at
// the cost of a longer `doc_len` — BM25 length normalization dilutes the primary
// wording's own terms a little in exchange for the alternate being reachable.
pub fn entity_document(kern: &Kern, entity: &Entity) -> String {
	let mut doc = entity.statements.join(" ");
	for rid in kern.by_from.get(&entity.id).into_iter().flatten() {
		match kern.reasons.get(rid) {
			Some(r) if r.kind == ReasonKind::Rephrase && !r.text.is_empty() => {
				if !doc.is_empty() {
					doc.push(' ');
				}
				doc.push_str(&r.text);
			}
			_ => {}
		}
	}
	doc
}

// Re-derives one entity's lexical document from the graph. Every site that mints
// or drops a `Rephrase` calls this, so an alternate wording never outlives the
// reason that carries it.
pub fn reindex_entity(g: &GraphGnn, kern_id: &str, entity_id: &str) {
	let (Some(lex), Some(kern)) = (g.lexical(), g.loaded(kern_id)) else {
		return;
	};
	if let Some(e) = kern.entities.get(entity_id) {
		lex.insert(entity_id, &entity_document(kern, e));
	}
}

#[derive(Debug, Clone)]
pub struct LexicalHit {
	pub entity_id: String,
	pub score: f32,
}

#[derive(Default)]
struct Posting {
	tf: u32,
}

struct Inner {
	k1: f32,
	b: f32,
	postings: HashMap<String, HashMap<String, Posting>>,
	doc_len: HashMap<String, u32>,
	total_len: u64,
}

pub struct LexicalIndex {
	inner: RwLock<Inner>,
}

impl LexicalIndex {
	pub fn new_in_ram(k1: f32, b: f32) -> Self {
		Self {
			inner: RwLock::new(Inner {
				k1,
				b,
				postings: HashMap::new(),
				doc_len: HashMap::new(),
				total_len: 0,
			}),
		}
	}

	// Read at QUERY time — no re-indexing. Invalid inputs are clamped/ignored.
	pub fn set_bm25_params(&self, k1: f32, b: f32) {
		let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
		if k1.is_finite() {
			inner.k1 = k1.max(0.0);
		}
		if b.is_finite() {
			inner.b = b.clamp(0.0, 1.0);
		}
	}

	pub fn insert(&self, entity_id: &str, text: &str) {
		let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
		inner_insert(&mut inner, entity_id, text);
	}

	pub fn remove(&self, entity_id: &str) {
		let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
		inner_remove(&mut inner, entity_id);
	}

	pub fn search(&self, query: &str, k: usize) -> Vec<LexicalHit> {
		self.search_filtered(query, k, &|_| true)
	}

	// keep applied BEFORE top-k truncation, so a sparse filter still returns a full k.
	pub fn search_filtered(
		&self,
		query: &str,
		k: usize,
		keep: &dyn Fn(&str) -> bool,
	) -> Vec<LexicalHit> {
		let tokens = tokenize(query);
		if tokens.is_empty() || k == 0 {
			return Vec::new();
		}
		let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
		let n_docs = inner.doc_len.len() as f32;
		if n_docs <= 0.0 {
			return Vec::new();
		}
		let avgdl = (inner.total_len as f32 / n_docs).max(1.0);
		let k1 = inner.k1;
		let b = inner.b;

		let mut scores: HashMap<String, f32> = HashMap::new();
		for tok in &tokens {
			let postings = match inner.postings.get(tok) {
				Some(p) => p,
				None => continue,
			};
			let df = postings.len() as f32;
			let idf = ((n_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
			for (doc_id, post) in postings {
				let dl = *inner.doc_len.get(doc_id).unwrap_or(&0) as f32;
				let tf = post.tf as f32;
				let denom = tf + k1 * (1.0 - b + b * dl / avgdl);
				let s = idf * (tf * (k1 + 1.0)) / denom;
				*scores.entry(doc_id.clone()).or_insert(0.0) += s;
			}
		}
		let mut hits: Vec<LexicalHit> = scores
			.into_iter()
			.map(|(id, s)| LexicalHit {
				entity_id: id,
				score: s,
			})
			.collect();
		// Score desc, id-asc tiebreak so the `truncate(k)` boundary is reproducible
		// (HashMap source; same convention as fuse::rrf).
		hits.retain(|h| keep(&h.entity_id));
		hits.sort_by(|a, b| util::cmp_rank(a.score, &a.entity_id, b.score, &b.entity_id));
		hits.truncate(k);
		hits
	}

	pub fn rebuild_from_graph(&self, g: &GraphGnn) {
		// One write guard for the whole rebuild so concurrent readers never observe
		// a half-cleared index.
		let mut inner = self.inner.write().unwrap_or_else(|e| e.into_inner());
		inner.postings.clear();
		inner.doc_len.clear();
		inner.total_len = 0;
		for kern in g.all() {
			for t in kern.entities.values() {
				let joined = entity_document(kern, t);
				if !joined.is_empty() {
					inner_insert(&mut inner, &t.id, &joined);
				}
			}
		}
	}

	#[cfg(test)]
	fn doc_count(&self) -> usize {
		self
			.inner
			.read()
			.unwrap_or_else(|e| e.into_inner())
			.doc_len
			.len()
	}
}

// Caller holds the write guard — do NOT lock here. Removes any prior version first (idempotent).
fn inner_insert(inner: &mut Inner, entity_id: &str, text: &str) {
	let tokens = tokenize(text);
	inner_remove(inner, entity_id);
	let dl = tokens.len() as u32;
	if dl == 0 {
		return;
	}
	let mut tfs: HashMap<String, u32> = HashMap::new();
	for tok in tokens {
		*tfs.entry(tok).or_insert(0) += 1;
	}
	for (tok, tf) in tfs {
		inner
			.postings
			.entry(tok)
			.or_default()
			.insert(entity_id.to_string(), Posting { tf });
	}
	inner.doc_len.insert(entity_id.to_string(), dl);
	inner.total_len += dl as u64;
}

fn inner_remove(inner: &mut Inner, entity_id: &str) {
	if let Some(dl) = inner.doc_len.remove(entity_id) {
		inner.total_len = inner.total_len.saturating_sub(dl as u64);
	} else {
		return;
	}
	let mut empty: Vec<String> = Vec::new();
	for (tok, postings) in inner.postings.iter_mut() {
		postings.remove(entity_id);
		if postings.is_empty() {
			empty.push(tok.clone());
		}
	}
	for tok in empty {
		inner.postings.remove(&tok);
	}
}

fn tokenize(text: &str) -> Vec<String> {
	let mut out = Vec::new();
	let mut cur = String::new();
	for ch in text.chars() {
		if ch.is_alphanumeric() {
			for lc in ch.to_lowercase() {
				cur.push(lc);
			}
		} else if !cur.is_empty() {
			out.push(stem(&cur));
			cur.clear();
		}
	}
	if !cur.is_empty() {
		out.push(stem(&cur));
	}
	out
}

// FIRST matching suffix, only if the stem stays > 2 chars; first-match can over-strip.
fn stem(t: &str) -> String {
	let s = t;
	for suf in &["ing", "edly", "ed", "ly", "ies", "es", "s"] {
		if s.len() > suf.len() + 2 && s.ends_with(suf) {
			return s[..s.len() - suf.len()].to_string();
		}
	}
	s.to_string()
}

#[cfg(test)]
#[path = "tests/lexical_test.rs"]
mod lexical_tests;
