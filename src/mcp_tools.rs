pub fn tool_definitions() -> Vec<serde_json::Value> {
	let mut defs = crate::mcp_tools_query::tool_schemas();
	defs.extend(crate::mcp_tools_events::tool_schemas());
	defs.extend(crate::mcp_tools_mutate::tool_schemas());
	defs.extend(crate::mcp_tools_admin::tool_schemas());
	defs.extend(crate::mcp_tools_intake::tool_schemas());
	defs.extend(crate::mcp_tools_setup::tool_schemas());
	defs.extend(crate::mcp_tools_delegate::tool_schemas());
	defs
}

pub(crate) fn typed_tool_schemas() -> Vec<crate::transport::ToolSchema> {
	tool_definitions()
		.into_iter()
		.filter_map(|v| crate::transport::ToolSchema::from_value(&v))
		.collect()
}

/// True when an MCP tool response carries `isError: true`.
#[cfg(test)]
pub(crate) fn is_error(out: &serde_json::Value) -> bool {
	out
		.get("isError")
		.and_then(|x| x.as_bool())
		.unwrap_or(false)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn definitions_are_well_formed_and_complete() {
		let defs = tool_definitions();
		let names: Vec<&str> = defs
			.iter()
			.map(|d| d["name"].as_str().expect("each tool has a string name"))
			.collect();

		let expected = [
			"query",
			"events",
			"ingest",
			"link",
			"forget",
			"forget_by_source",
			"degrade",
			"move",
			"promote",
			"health",
			"graviton",
			"claim_kind",
			"pulse",
			"gc",
			"intake_drain",
			"setup",
			"sign",
			"contract_grant",
		];
		assert_eq!(names, expected, "tool set must match (order intentional)");

		for d in &defs {
			let name = d["name"].as_str().unwrap();
			assert!(!name.is_empty(), "tool name must not be empty");
			let schema = &d["inputSchema"];
			assert!(
				schema.is_object(),
				"{name}: inputSchema must be present and an object"
			);
			assert_eq!(
				schema["type"], "object",
				"{name}: inputSchema.type must be 'object'"
			);
		}
	}

	// `kern intake drain` routes through this tool, so a missing schema hides it
	// from the daemon's tool list and a missing arm answers "unknown tool" — both
	// send the CLI back to draining locally beside the daemon's own loop. A
	// second arm is the other failure: two dispatch copies drift apart silently.
	#[test]
	fn intake_drain_is_declared_once_and_dispatched_once() {
		let defs = tool_definitions();
		assert_eq!(
			defs.iter().filter(|d| d["name"] == "intake_drain").count(),
			1,
			"intake_drain must appear in tool_schemas() exactly once"
		);
		let dispatch = include_str!("mcp.rs");
		assert_eq!(
			dispatch.matches("\"intake_drain\" =>").count(),
			1,
			"exactly one arm in the single `match name`"
		);
	}

	// Same contract for `kern promote`, and the stakes are higher: a missing arm
	// sends the CLI down the NoDaemon fallback, which releases the claim in a
	// stale local copy the serving daemon then overwrites — the row reads as
	// promoted and stays held.
	#[test]
	fn promote_is_declared_once_and_dispatched_once() {
		let defs = tool_definitions();
		assert_eq!(
			defs.iter().filter(|d| d["name"] == "promote").count(),
			1,
			"promote must appear in tool_schemas() exactly once"
		);
		let dispatch = include_str!("mcp.rs");
		assert_eq!(
			dispatch.matches("\"promote\" =>").count(),
			1,
			"exactly one arm in the single `match name`"
		);
	}

	#[test]
	fn query_schema_states_text_or_id_without_a_root_combinator() {
		let defs = tool_definitions();
		let query = defs
			.iter()
			.find(|d| d["name"] == "query")
			.expect("query tool present");
		let schema = &query["inputSchema"];
		// The Anthropic tool API rejects anyOf/oneOf/allOf at the root of
		// input_schema, so the "either text or id" rule is prose + a runtime
		// guard in tool_query, never a schema combinator.
		for key in ["anyOf", "oneOf", "allOf"] {
			assert!(
				schema[key].is_null(),
				"query schema must not carry a top-level `{key}`"
			);
		}
		let desc = query["description"].as_str().unwrap_or_default();
		assert!(
			desc.contains("`text`") && desc.contains("`id`") && desc.contains("`ids`"),
			"description must name text, id and ids as the alternatives"
		);
	}

	#[test]
	fn no_tool_schema_carries_a_root_combinator() {
		for def in tool_definitions() {
			let name = def["name"].as_str().unwrap_or("?").to_string();
			for key in ["anyOf", "oneOf", "allOf"] {
				assert!(
					def["inputSchema"][key].is_null(),
					"tool `{name}` declares a top-level `{key}` — the Anthropic \
					 tool API rejects it"
				);
			}
		}
	}

	#[test]
	fn ingest_schema_advertises_the_optional_retention() {
		let defs = tool_definitions();
		let ingest = defs
			.iter()
			.find(|d| d["name"] == "ingest")
			.expect("ingest tool present");
		let props = &ingest["inputSchema"]["properties"];
		assert_eq!(
			props["retention_secs"]["type"], "integer",
			"retention_secs must be advertised so a client can set a TTL"
		);
		let required: Vec<&str> = ingest["inputSchema"]["required"]
			.as_array()
			.map(|a| a.iter().filter_map(|v| v.as_str()).collect())
			.unwrap_or_default();
		assert!(
			!required.contains(&"retention_secs"),
			"retention is opt-in — the default path sets no valid_until"
		);
	}

	#[test]
	fn mutation_tools_declare_their_required_fields() {
		let defs = tool_definitions();
		// `required` must mirror each handler's runtime rejects, so clients fail fast.
		let want: &[(&str, &[&str])] = &[
			("ingest", &["text"]),
			("link", &["from", "to"]),
			("forget", &["id"]),
			("forget_by_source", &["scheme", "object_id"]),
			("degrade", &["query_id"]),
			("move", &["id", "to_kern"]),
			("claim_kind", &["action", "name"]),
		];
		for (name, fields) in want {
			let tool = defs
				.iter()
				.find(|d| d["name"] == *name)
				.expect("tool present");
			let required: Vec<&str> = tool["inputSchema"]["required"]
				.as_array()
				.map(|a| a.iter().filter_map(|v| v.as_str()).collect())
				.unwrap_or_default();
			for f in *fields {
				assert!(
					required.contains(f),
					"{name} schema must require `{f}`, got {required:?}"
				);
			}
		}
	}
}
