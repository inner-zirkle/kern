//! MCP prompt definitions — the reusable prompt templates a host lists and
//! fills.

use serde_json::value::RawValue;

use crate::{err_resp, ok, Response, ERR_INVALID_REQ, ERR_NOT_FOUND};

// Guarded by `research_prompt_names_are_real_tools` against renames in `tools.rs`.
const QUERY_TOOL: &str = "query";
const INGEST_TOOL: &str = "ingest";

pub fn prompt_definitions() -> Vec<serde_json::Value> {
	vec![serde_json::json!({
		"name": "research",
		"description": "Use the kern knowledge graph to research a topic",
		"arguments": [{
			"name": "topic",
			"description": "The topic to research",
			"required": true,
		}],
	})]
}

pub(crate) fn handle_prompt_get(
	id: Option<Box<RawValue>>,
	params: Option<Box<RawValue>>,
) -> Response {
	#[derive(serde::Deserialize)]
	struct Params {
		name: String,
		#[serde(default)]
		arguments: std::collections::HashMap<String, String>,
	}

	let params: Params = match params
		.as_deref()
		.map(|r| serde_json::from_str(r.get()))
		.transpose()
	{
		Ok(Some(p)) => p,
		_ => return err_resp(id, ERR_INVALID_REQ, "invalid params"),
	};

	match params.name.as_str() {
		"research" => {
			let topic = params.arguments.get("topic").cloned().unwrap_or_default();
			if topic.is_empty() {
				return err_resp(id, ERR_INVALID_REQ, "topic argument required");
			}
			ok(
				id,
				serde_json::json!({
					"messages": [{
						"role": "user",
						"content": {
							"type": "text",
							"text": format!(
								"Use the kern knowledge graph to recall what is known about: {topic}\n\n\
								1. Use {q}(\"{topic}\") to see what's already known\n\
								2. Read the passages and chains yourself — kern does no synthesis\n\
								3. If knowledge is lacking, use {ing} to add relevant text",
								q = QUERY_TOOL,
								ing = INGEST_TOOL,
							),
						},
					}],
				}),
			)
		}
		_ => err_resp(
			id,
			ERR_NOT_FOUND,
			&format!("unknown prompt: {}", params.name),
		),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn raw(v: serde_json::Value) -> Box<RawValue> {
		serde_json::value::to_raw_value(&v).unwrap()
	}

	fn message_text(resp: &Response) -> String {
		resp.result.as_ref().expect("result present")["messages"][0]["content"]["text"]
			.as_str()
			.expect("text content")
			.to_string()
	}

	#[test]
	fn happy_path_embeds_topic_in_message() {
		let params = raw(serde_json::json!({
			"name": "research",
			"arguments": { "topic": "borrow checker" },
		}));
		let resp = handle_prompt_get(None, Some(params));
		assert!(resp.error.is_none(), "no error on valid request");
		assert!(message_text(&resp).contains("borrow checker"));
	}

	#[test]
	fn missing_params_is_invalid_request() {
		let resp = handle_prompt_get(None, None);
		let err = resp.error.as_ref().expect("error present");
		assert_eq!(err.code, ERR_INVALID_REQ);
	}

	#[test]
	fn unknown_prompt_name_is_not_found() {
		let params = raw(serde_json::json!({ "name": "no_such_prompt" }));
		let resp = handle_prompt_get(None, Some(params));
		let err = resp.error.as_ref().expect("error present");
		assert_eq!(err.code, ERR_NOT_FOUND);
	}

	#[test]
	fn empty_topic_is_rejected() {
		let params = raw(serde_json::json!({
			"name": "research",
			"arguments": { "topic": "" },
		}));
		let resp = handle_prompt_get(None, Some(params));
		let err = resp.error.as_ref().expect("error present");
		assert_eq!(err.code, ERR_INVALID_REQ);
	}

	#[test]
	fn happy_path_steers_to_query_and_ingest_tools() {
		let params = raw(serde_json::json!({
			"name": "research",
			"arguments": { "topic": "graphs" },
		}));
		let text = message_text(&handle_prompt_get(None, Some(params)));
		assert!(
			text.contains(QUERY_TOOL),
			"prompt should reference the query tool"
		);
		assert!(
			text.contains(INGEST_TOOL),
			"prompt should reference the ingest tool"
		);
	}

	#[test]
	fn research_prompt_names_are_real_tools() {
		let names: Vec<String> = crate::tools::tool_definitions()
			.iter()
			.filter_map(|d| d.get("name").and_then(|n| n.as_str()).map(String::from))
			.collect();
		for tool in [QUERY_TOOL, INGEST_TOOL] {
			assert!(
				names.contains(&tool.to_string()),
				"research prompt references tool `{tool}` absent from tool_definitions()"
			);
		}
	}
}
