//! MCP resource definitions and reads — the graph views a host can pull as
//! documents.

use serde_json::value::RawValue;

use graph::search::{find_entity, find_reason};
use util::truncate;

use crate::mcp::{err_resp, ok, Response, Server, ERR_INVALID_REQ, ERR_NOT_FOUND};

pub fn resource_definitions() -> Vec<serde_json::Value> {
	vec![
		serde_json::json!({
			"uri": "kern://local/health",
			"name": "Graph health",
			"description": "Entity/edge counts, tick heat, unnamed count, gravitons",
			"mimeType": "application/json",
		}),
		serde_json::json!({
			"uri": "kern://local/thoughts",
			"name": "Top thoughts",
			"description": "Top thoughts by global rank",
			"mimeType": "application/json",
		}),
		serde_json::json!({
			"uri": "kern://local/kerns",
			"name": "All Kerns",
			"description": "All loaded Kerns with gravitons and stats",
			"mimeType": "application/json",
		}),
		serde_json::json!({
			"uri": "kern://local/claim-kinds",
			"name": "Claim kinds",
			"description": "All registered claim kinds (built-ins not included)",
			"mimeType": "application/json",
		}),
	]
}

pub(crate) fn handle_resource_read(
	server: &Server,
	id: Option<Box<RawValue>>,
	params: Option<Box<RawValue>>,
) -> Response {
	#[derive(serde::Deserialize)]
	struct Params {
		uri: String,
	}

	let params: Params = match params
		.as_deref()
		.map(|r| serde_json::from_str(r.get()))
		.transpose()
	{
		Ok(Some(p)) => p,
		_ => return err_resp(id, ERR_INVALID_REQ, "invalid params"),
	};

	match params.uri.as_str() {
		"kern://local/health" => ok(id, resource_content(&params.uri, &resource_health(server))),
		"kern://local/thoughts" => ok(
			id,
			resource_content(&params.uri, &resource_thoughts(server)),
		),
		"kern://local/kerns" => ok(id, resource_content(&params.uri, &resource_kerns(server))),
		"kern://local/claim-kinds" => ok(
			id,
			resource_content(&params.uri, &resource_claim_kinds(server)),
		),
		_ => {
			if let Some(tid) = params.uri.strip_prefix("thought://") {
				return ok(
					id,
					resource_content(&params.uri, &resource_thought(server, tid)),
				);
			}
			if let Some(rid) = params.uri.strip_prefix("reason://") {
				return ok(
					id,
					resource_content(&params.uri, &resource_reason(server, rid)),
				);
			}
			err_resp(
				id,
				ERR_NOT_FOUND,
				&format!("unknown resource: {}", params.uri),
			)
		}
	}
}

fn resource_health(server: &Server) -> String {
	serde_json::to_string(&server.health_stats()).unwrap_or_default()
}

const TOP_THOUGHTS: usize = 50;

fn resource_thoughts(server: &Server) -> String {
	let g = server.graph.read();
	let mut all: Vec<(f64, serde_json::Value)> = Vec::new();
	for kern in g.all() {
		for t in kern.entities.values() {
			all.push((
				t.score,
				serde_json::json!({
					"id": t.id,
					"score": t.score,
					"text": truncate(&t.text(), 200),
					"kern": kern.id,
				}),
			));
		}
	}
	all.sort_by(|a, b| {
		let a_id = a.1["id"].as_str().unwrap_or("");
		let b_id = b.1["id"].as_str().unwrap_or("");
		util::cmp_rank(a.0, a_id, b.0, b_id)
	});
	let top: Vec<serde_json::Value> = all.into_iter().take(TOP_THOUGHTS).map(|(_, v)| v).collect();
	serde_json::to_string(&top).unwrap_or_default()
}

fn resource_kerns(server: &Server) -> String {
	let g = server.graph.read();
	let summaries: Vec<serde_json::Value> = g
		.all()
		.iter()
		.map(|k| {
			serde_json::json!({
				"id": k.id,
				"graviton": k.graviton_text,
				"entities": k.entities.len(),
				"reasons": k.reasons.len(),
				"children": k.children.len(),
			})
		})
		.collect();
	serde_json::to_string(&summaries).unwrap_or_default()
}

fn resource_claim_kinds(server: &Server) -> String {
	let g = server.graph.read();
	serde_json::to_string(&g.root.claim_kinds).unwrap_or_default()
}

fn edge_json(re: &base::base_types::Reason) -> serde_json::Value {
	serde_json::json!({
		"id": re.id,
		"from": re.from,
		"to": re.to,
		"kind": re.kind as i32,
		"text": re.text.clone(),
		"score": re.score,
	})
}

fn resource_thought(server: &Server, id: &str) -> String {
	let g = server.graph.read();
	match find_entity(&g, id) {
		Some((thought, kern_id)) => {
			let mut edges = Vec::new();
			if let Some(kern) = g.kerns.get(&kern_id) {
				let rids = graph::reason::collect_reason_ids(kern, &thought.id);
				for rid in &rids {
					if let Some(re) = kern.reasons.get(rid) {
						edges.push(edge_json(re));
					}
				}
			}
			serde_json::to_string(&serde_json::json!({
				"id": thought.id,
				"kind": thought.kind as u8,
				"text": thought.text(),
				"score": thought.score,
				"access_count": thought.access_count.value_i32(),
				"kern": kern_id,
				"edges": edges,
			}))
			.unwrap_or_default()
		}
		None => format!(r#"{{"error":"thought not found: {id}"}}"#),
	}
}

fn resource_reason(server: &Server, id: &str) -> String {
	let g = server.graph.read();
	match find_reason(&g, id) {
		Some((reason, _)) => serde_json::to_string(&serde_json::json!({
			"id": reason.id,
			"from": reason.from,
			"to": reason.to,
			"kind": reason.kind as i32,
			"text": reason.text.clone(),
			"score": reason.score,
			"traversal_count": reason.traversal_count.value_i32(),
		}))
		.unwrap_or_default(),
		None => format!(r#"{{"error":"reason not found: {id}"}}"#),
	}
}

fn resource_content(uri: &str, text: &str) -> serde_json::Value {
	serde_json::json!({
		"contents": [{
			"uri": uri,
			"mimeType": "application/json",
			"text": text,
		}],
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	use crate::mcp::Server;
	use base::base_types::{Entity, Kern, Reason};
	use graph::reason::add_reason;

	fn make_server() -> Server {
		crate::test_support::mcp_server()
	}

	fn seed(server: &Server) {
		let mut g = server.graph.write();
		let mut k = Kern::new("kx", "");
		k.entities.insert(
			"e1".into(),
			Entity {
				id: "e1".into(),
				..Default::default()
			},
		);
		add_reason(
			&mut k,
			Reason {
				from: "e1".into(),
				to: "e2".into(),
				id: "r1".into(),
				..Default::default()
			},
		);
		g.kerns.insert("kx".into(), k);
	}

	// A dangling endpoint is ordinary — `to` is optional in `add_reason` — and the
	// edge renders whole, its own text included.
	#[tokio::test]
	async fn resource_thought_renders_an_edge_with_a_dangling_endpoint() {
		let srv = make_server();
		seed(&srv);
		{
			let mut g = srv.graph.write();
			let k = g.kerns.get_mut("kx").expect("seed() ran first");
			add_reason(
				k,
				Reason {
					from: "e1".into(),
					to: "ghost".into(),
					id: "r4".into(),
					text: "e1 and the ghost share a mechanism".into(),
					..Default::default()
				},
			);
		}
		let v: serde_json::Value =
			serde_json::from_str(&resource_thought(&srv, "e1")).expect("valid json");
		let r4 = v["edges"]
			.as_array()
			.expect("a list")
			.iter()
			.find(|e| e["id"] == "r4")
			.expect("the edge renders");
		assert_eq!(r4["text"], "e1 and the ghost share a mechanism");
		assert_eq!(r4["to"], "ghost");
	}

	#[tokio::test]
	async fn resource_thought_renders_entity_with_its_edges() {
		let srv = make_server();
		seed(&srv);
		let v: serde_json::Value =
			serde_json::from_str(&resource_thought(&srv, "e1")).expect("valid json");
		assert_eq!(v["id"], "e1");
		assert_eq!(v["kern"], "kx");
		assert_eq!(
			v["edges"].as_array().map(|a| a.len()),
			Some(1),
			"the one incident edge"
		);
		assert_eq!(v["edges"][0]["id"], "r1");
	}

	#[tokio::test]
	async fn resource_thought_missing_returns_error_json() {
		let srv = make_server();
		let out = resource_thought(&srv, "nope");
		let v: serde_json::Value = serde_json::from_str(&out).expect("error is still valid json");
		assert!(v["error"].as_str().unwrap_or("").contains("not found"));
	}

	#[tokio::test]
	async fn resource_reason_renders_reason_endpoints() {
		let srv = make_server();
		seed(&srv);
		let v: serde_json::Value =
			serde_json::from_str(&resource_reason(&srv, "r1")).expect("valid json");
		assert_eq!(v["id"], "r1");
		assert_eq!(v["from"], "e1");
		assert_eq!(v["to"], "e2");
	}

	#[tokio::test]
	async fn resource_reason_missing_returns_error_json() {
		let srv = make_server();
		let out = resource_reason(&srv, "nope");
		let v: serde_json::Value = serde_json::from_str(&out).expect("error is still valid json");
		assert!(v["error"].as_str().unwrap_or("").contains("not found"));
	}
}
