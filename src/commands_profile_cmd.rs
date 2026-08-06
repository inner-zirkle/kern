use std::sync::Arc;
use std::time::Instant;

use crate::search::search_all_unlocked;
use crate::profile::{render_timeline, Profile};
use crate::retrieval::seed::Mode;

use crate::commands::{load_graph, Client, Endpoint};

const TIMELINE_WIDTH: usize = 40;

const DISTILL_SAMPLE: &str = "User: The deploy failed because the config pointed at the staging \
	bucket. Assistant: Fixed — the bucket name is now anchored to the environment, so production \
	reads prod-artifacts and staging keeps its own.";

fn ms(t: Instant) -> f64 {
	t.elapsed().as_secs_f64() * 1000.0
}

fn flat(name: &str, total_ms: f64) -> Profile {
	Profile {
		name: name.to_string(),
		checkpoints: Vec::new(),
		total_ms,
	}
}

fn renamed(mut p: Profile, name: &str) -> Profile {
	p.name = name.to_string();
	p
}

// Read-only: nothing is persisted, so it is safe to run next to a daemon.
pub(crate) async fn cmd_profile(cfg: &crate::config::Config, text: &str, no_llm: bool) {
	let mut profiles: Vec<Profile> = Vec::new();

	let t = Instant::now();
	let g = load_graph(cfg);
	profiles.push(flat("load graph", ms(t)));
	let kerns = g.kerns.len();
	let mut entities = 0usize;
	for k in g.all() {
		entities += k.entities.len();
	}

	let reason_url = cfg.reason_url().to_string();
	let llm_client = Client::new(
		Endpoint::new(&reason_url, &cfg.reason.model, cfg.reason_key()),
		Endpoint::new(&cfg.embed.url, &cfg.embed.model, &cfg.embed.key),
	)
	.with_timeout_secs(cfg.reason.timeout_secs);

	let t = Instant::now();
	let qvec = match llm_client.embed(text).await {
		Ok(v) => v,
		Err(e) => {
			eprintln!("embed: {e} (embed endpoint up at {}?)", cfg.embed.url);
			return;
		}
	};
	profiles.push(flat("embed (cold)", ms(t)));

	let t = Instant::now();
	let _ = llm_client.embed(text).await;
	profiles.push(flat("embed (warm)", ms(t)));

	let t = Instant::now();
	let hits = search_all_unlocked(&g, &qvec, 10);
	profiles.push(flat(&format!("vector search ({} hits)", hits.len()), ms(t)));

	for (mode, label) in [
		(Mode::Content, "query content (no llm)"),
		(Mode::Reason, "query reason (no llm)"),
		(Mode::Hybrid, "query hybrid (no llm)"),
	] {
		let (_, p) = crate::retrieval::query::query_profiled(
			&g,
			&cfg.retrieval,
			&cfg.heat,
			&qvec,
			text,
			mode,
			None,
		);
		profiles.push(renamed(p, label));
	}

	if no_llm || reason_url.is_empty() {
		if !no_llm {
			eprintln!("no reason endpoint configured; skipping llm stages");
		}
	} else {
		let llm_fn: crate::retrieval::LlmFunc = Arc::new(llm_client.complete_func());

		let t = Instant::now();
		let claims =
			crate::ingest::distill::distill(DISTILL_SAMPLE, &[], &*llm_fn, std::time::SystemTime::now());
		let n = claims.map(|c| c.len()).unwrap_or(0);
		profiles.push(flat(&format!("distill ({n} claims)"), ms(t)));
	}

	println!("kern profile — {kerns} kerns, {entities} entities, query: {text:?}");
	println!();
	print!("{}", render_timeline(&profiles, TIMELINE_WIDTH));
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::{json, Value};

	#[tokio::test]
	async fn cmd_profile_no_llm_path_does_not_panic() {
		let app = axum::Router::new().route(
			"/api/embed",
			axum::routing::post(|_body: axum::Json<Value>| async move {
				axum::Json(json!({ "embeddings": [[0.1, 0.2, 0.3]] }))
			}),
		);
		let (embed_url, _server) = crate::test_support::spawn_http(app).await;

		let dir = std::env::temp_dir().join(format!("kern_profile_smoke_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();

		let mut cfg = crate::config::Config {
			data_dir: dir.to_string_lossy().into_owned(),
			..Default::default()
		};
		cfg.embed.url = embed_url;

		cmd_profile(&cfg, "smoke test query", true).await;

		let _ = std::fs::remove_dir_all(&dir);
	}
}
