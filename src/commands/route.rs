use transport::kern_rpc::{AuthReq, CallToolReq, KernRpcClient};
use transport::typed::{AdapterError, Endpoint, JsonEnvelopeCodec};

pub(crate) enum Routed {
	Done(serde_json::Value),
	Refused(String),
	NoDaemon,
}

pub(crate) async fn route(name: &str, args: serde_json::Value) -> Routed {
	route_to(&Endpoint::kern(), &crate::rpc::caller(), name, args).await
}

// One attempt and no spawn: a one-shot write must never conjure the daemon it
// was looking for, and an absent socket is the ordinary no-daemon case rather
// than a failure to report.
pub(crate) async fn route_to(
	endpoint: &Endpoint,
	auth: &AuthReq,
	name: &str,
	args: serde_json::Value,
) -> Routed {
	let connected = KernRpcClient::<JsonEnvelopeCodec>::connect_endpoint_with_retry(
		endpoint,
		auth,
		1,
		std::time::Duration::ZERO,
	)
	.await;
	let client = match connected {
		Ok(c) => c,
		// A refusal is not an absence. The daemon answered and turned this caller
		// away, so falling through to NoDaemon would send the CLI off to write
		// locally behind a daemon that owns the graph — the exact split item 9
		// closed, reopened by an auth failure.
		Err(AdapterError::Unauthenticated(e)) => {
			return Routed::Refused(format!("daemon refused this caller: {e}"))
		}
		// Not an absence either, and not the daemon's verdict — this caller
		// refused the endpoint. Something is bound at the socket path that this
		// user does not own, which means no daemon of ours is reachable there;
		// reporting NoDaemon would send the CLI off to write locally without
		// ever mentioning that somebody else holds the name.
		Err(AdapterError::UntrustedEndpoint(e)) => {
			return Routed::Refused(format!("refusing endpoint: {e}"))
		}
		Err(_) => return Routed::NoDaemon,
	};
	let req = CallToolReq {
		name: name.to_string(),
		args,
	};
	match client.call_tool(req).await {
		Ok(res) => unwrap_envelope(&res.envelope),
		// A daemon that answered the connect owns the graph even if the call went
		// wrong. Writing locally behind its back is exactly the split this closes,
		// so a failed call is reported, never retried against the store.
		Err(e) => Routed::Refused(format!("daemon rpc: {e}")),
	}
}

fn unwrap_envelope(envelope: &serde_json::Value) -> Routed {
	let text = envelope
		.pointer("/content/0/text")
		.and_then(|v| v.as_str())
		.unwrap_or("");
	if envelope.get("isError").and_then(|v| v.as_bool()) == Some(true) {
		let msg = if text.is_empty() {
			"kern tool error".to_string()
		} else {
			text.to_string()
		};
		return Routed::Refused(msg);
	}
	match serde_json::from_str(text) {
		Ok(v) => Routed::Done(v),
		Err(e) => Routed::Refused(format!("decode daemon result: {e}")),
	}
}

pub(crate) fn u64_field(v: &serde_json::Value, key: &str) -> u64 {
	v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

pub(crate) fn f64_field(v: &serde_json::Value, key: &str) -> f64 {
	v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

pub(crate) fn str_field<'a>(v: &'a serde_json::Value, key: &str) -> &'a str {
	v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

pub(crate) fn array_field<'a>(v: &'a serde_json::Value, key: &str) -> &'a [serde_json::Value] {
	v.get(key)
		.and_then(|x| x.as_array())
		.map(Vec::as_slice)
		.unwrap_or(&[])
}

#[cfg(all(test, unix))]
mod tests {
	use super::*;
	use crate::test_support::{scratch_endpoint, serving, test_caller};

	fn kern_with_edge() -> crate::mcp::Server {
		use crate::base::reason::add_reason;
		use crate::base::types::Kern;
		let srv = crate::test_support::mcp_server();
		let mut k = Kern::new("kx", "");
		k.entities
			.insert("a".into(), crate::test_support::entity("a"));
		k.entities
			.insert("b".into(), crate::test_support::entity("b"));
		let mut healthy = crate::test_support::edge("a", "b");
		healthy.score = 1.0;
		add_reason(&mut k, healthy);
		add_reason(&mut k, crate::test_support::edge("a", "c"));
		srv.graph.write().kerns.insert("kx".into(), k);
		srv
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn a_missing_socket_is_no_daemon_not_an_error() {
		let ep = scratch_endpoint("absent");
		let out = route_to(
			&ep,
			&test_caller(),
			"forget",
			serde_json::json!({"id": "a"}),
		)
		.await;
		assert!(
			matches!(out, Routed::NoDaemon),
			"nothing serving -> the caller owns the write"
		);
	}

	// The gate over a real socket. Every other refusal test in the tree runs on
	// an in-process pipe, which proves the branch but not the transport: this one
	// crosses `kern.sock` — real accept loop, real per-connection task, real
	// framing — and asserts the two things that matter on the far side. The tool
	// did not run (the daemon's graph still holds the entity), and the caller was
	// told it was *refused* rather than that nothing was there, because a
	// `NoDaemon` here is exactly how a CLI ends up writing behind a live daemon.
	//
	// The wrong token is `TEST_TOKEN` with its last byte changed, same length, so
	// only the byte compare can refuse it.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_wrong_token_over_the_real_socket_is_refused_and_runs_nothing() {
		let ep = scratch_endpoint("wrong-token");
		let srv = kern_with_edge();
		let graph = srv.graph.clone();
		serving(srv, &ep).await;

		let wrong = transport::kern_rpc::AuthReq::new("scratch-tokex");
		assert_eq!(
			wrong.token.len(),
			crate::test_support::TEST_TOKEN.len(),
			"a wrong token of another length never reaches the byte compare"
		);

		let out = route_to(&ep, &wrong, "forget", serde_json::json!({"id": "a"})).await;
		match out {
			Routed::Refused(msg) => assert!(
				msg.contains("unauthenticated"),
				"a refusal must name itself as one: {msg}"
			),
			Routed::NoDaemon => {
				panic!("a refusal reported as an absence sends the CLI off to write locally")
			}
			Routed::Done(v) => panic!("an unauthenticated caller ran the tool: {v}"),
		}

		let g = graph.read();
		assert!(
			g.kerns.get("kx").expect("kern").entities.contains_key("a"),
			"the daemon's graph must be untouched by a caller it refused"
		);
	}

	// The whole point of item 9's serving half: the mutation must land in the
	// daemon's live graph, not in a second copy the CLI opened behind it.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_forget_mutates_the_serving_daemons_graph() {
		let ep = scratch_endpoint("forget");
		let srv = kern_with_edge();
		let graph = srv.graph.clone();
		serving(srv, &ep).await;

		let out = route_to(
			&ep,
			&test_caller(),
			"forget",
			serde_json::json!({"id": "a"}),
		)
		.await;
		let Routed::Done(v) = out else {
			panic!("a serving daemon must answer the forget");
		};
		assert_eq!(
			u64_field(&v, "removed_edges"),
			2,
			"both incident edges cascaded"
		);

		let g = graph.read();
		assert!(
			!g.kerns.get("kx").expect("kern").entities.contains_key("a"),
			"the daemon's own graph lost the entity"
		);
	}

	// Same contract for the per-source forget (ROADMAP item 19): a host that
	// deleted a document hands the whole source over, and the cascade has to land
	// in the graph the daemon is serving rather than in the copy this process
	// would have opened beside it.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_forget_by_source_mutates_the_serving_daemons_graph() {
		use crate::base::types::Source;

		let ep = scratch_endpoint("forget-source");
		let srv = kern_with_edge();
		for (id, section) in [("a", "intro"), ("b", "body")] {
			srv
				.graph
				.write()
				.kerns
				.get_mut("kx")
				.expect("kern")
				.entities
				.get_mut(id)
				.expect("entity")
				.source = Source::File {
				path: "notes.md".into(),
				section: section.into(),
				title: String::new(),
				author: String::new(),
				url: String::new(),
			};
		}
		let graph = srv.graph.clone();
		serving(srv, &ep).await;

		let args = serde_json::json!({"scheme": "file", "object_id": "notes.md"});
		let Routed::Done(v) = route_to(&ep, &test_caller(), "forget_by_source", args).await else {
			panic!("a serving daemon must answer the per-source forget");
		};
		assert_eq!(
			u64_field(&v, "removed_entities"),
			2,
			"both sections of the source went"
		);
		assert_eq!(u64_field(&v, "removed_edges"), 2, "their edges cascaded");

		let g = graph.read();
		let kern = g.kerns.get("kx").expect("kern");
		assert!(
			kern.entities.is_empty(),
			"the daemon's own graph lost the source's entities"
		);
	}

	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_degrade_reports_both_counts_from_the_daemon() {
		let ep = scratch_endpoint("degrade");
		serving(kern_with_edge(), &ep).await;

		let out = route_to(
			&ep,
			&test_caller(),
			"degrade",
			serde_json::json!({"query_id": "a"}),
		)
		.await;
		let Routed::Done(v) = out else {
			panic!("a serving daemon must answer the degrade");
		};
		assert_eq!(
			u64_field(&v, "decayed_edges"),
			2,
			"both incident edges visited"
		);
		// The CLI has always printed a reap count; a routed degrade that cannot
		// read one back would silently print 0 for every reap.
		assert_eq!(
			u64_field(&v, "removed_edges"),
			1,
			"the sub-threshold edge is reported as reaped"
		);
	}

	// The read half of item 9: `kern get` must read the daemon's live graph, and
	// the answer must carry everything the CLI prints.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_get_reads_the_serving_daemons_graph() {
		let ep = scratch_endpoint("get");
		let srv = kern_with_edge();
		let graph = srv.graph.clone();
		serving(srv, &ep).await;
		graph
			.write()
			.kerns
			.get_mut("kx")
			.expect("kern")
			.entities
			.get_mut("a")
			.expect("entity")
			.set_text("only in the daemon".into());

		let out = route_to(&ep, &test_caller(), "query", serde_json::json!({"id": "a"})).await;
		let Routed::Done(v) = out else {
			panic!("a serving daemon must answer the get");
		};
		assert_eq!(str_field(&v, "id"), "a");
		assert_eq!(
			str_field(&v, "text"),
			"only in the daemon",
			"the text came from the daemon's graph, not from disk"
		);
		assert_eq!(str_field(&v, "kern"), "kx");
		assert_eq!(array_field(&v, "edges").len(), 2, "both incident edges");
	}

	// An entity the daemon never flushed: a `get` that reads the store instead of
	// asking the owner reports "not found" for something that exists.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_get_reads_the_daemons_unflushed_state() {
		let ep = scratch_endpoint("get-unflushed");
		let srv = kern_with_edge();
		srv
			.graph
			.write()
			.kerns
			.get_mut("kx")
			.expect("kern")
			.entities
			.insert(
				"only-in-ram".into(),
				crate::test_support::entity("only-in-ram"),
			);
		serving(srv, &ep).await;

		let out = route_to(
			&ep,
			&test_caller(),
			"query",
			serde_json::json!({"id": "only-in-ram"}),
		)
		.await;
		let Routed::Done(v) = out else {
			panic!("a serving daemon must answer the get");
		};
		assert_eq!(
			str_field(&v, "id"),
			"only-in-ram",
			"the entity came from the daemon's live graph"
		);
	}

	// A prefix is what kern itself prints (`short_id`), so the daemon has to
	// resolve one or every copied id fails the moment a daemon is up — the fix
	// for staleness would become a miss.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_routed_get_still_resolves_a_prefix() {
		let ep = scratch_endpoint("get-prefix");
		let srv = kern_with_edge();
		// A full-length id nobody would type, so "9f3c" is a genuine prefix and
		// not an exact hit that would pass with prefix matching removed.
		srv
			.graph
			.write()
			.kerns
			.get_mut("kx")
			.expect("kern")
			.entities
			.insert(
				"9f3c8d21b4e07a65".into(),
				crate::test_support::entity("9f3c8d21b4e07a65"),
			);
		serving(srv, &ep).await;

		let out = route_to(
			&ep,
			&test_caller(),
			"query",
			serde_json::json!({"id": "9f3c"}),
		)
		.await;
		let Routed::Done(v) = out else {
			panic!("a prefix must resolve through the daemon");
		};
		assert_eq!(
			str_field(&v, "id"),
			"9f3c8d21b4e07a65",
			"the prefix resolved to the full id"
		);
	}

	// A tool error is the daemon's answer, not a reason to go around it.
	#[tokio::test(flavor = "multi_thread")]
	async fn a_daemon_tool_error_is_refused_not_downgraded_to_no_daemon() {
		let ep = scratch_endpoint("refuse");
		serving(kern_with_edge(), &ep).await;

		let out = route_to(
			&ep,
			&test_caller(),
			"forget",
			serde_json::json!({"id": "ghost"}),
		)
		.await;
		match out {
			Routed::Refused(msg) => assert!(msg.contains("thought not found"), "{msg}"),
			_ => panic!("an unknown id must come back as the daemon's refusal"),
		}
	}
}
