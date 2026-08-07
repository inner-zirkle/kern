//! Config loading: find `.kern/kern.toml` by `.git`-first root resolution,
//! deep-merge the global scope under the project scope per key, and surface
//! parse errors with their file path.

use std::path::Path;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
	#[error("io: {0}")]
	Io(String),
	#[error("parse: {0}")]
	Parse(String),
}

pub fn merged_value(user: &Path, project: &Path) -> Result<toml::Value, Error> {
	let user_v = read_value(user)?;
	let project_v = read_value(project)?;
	Ok(crate::config_secrets::seal_redirected(
		merge_deep(user_v, project_v.clone()),
		&project_v,
	))
}

fn read_value(path: &Path) -> Result<toml::Value, Error> {
	match std::fs::read_to_string(path) {
		// Parse as a document `toml::Table` — a bare-`Value` parse misreads a leading
		// `[section]` header as an array (see read_value_parses_leading_section_header).
		Ok(text) => text
			.parse::<toml::Table>()
			.map(toml::Value::Table)
			.map_err(|e| Error::Parse(e.to_string())),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
			Ok(toml::Value::Table(toml::value::Table::new()))
		}
		Err(e) => Err(Error::Io(e.to_string())),
	}
}

/// Recursive merge at every depth: where both scopes hold a table the two are
/// merged key by key, so a project setting one field of a section never drops the
/// user's other fields in it. Arrays are leaves — `over` replaces, never appends
/// (`watcher.roots` and `gossip.peers` are complete lists, not accumulators).
fn merge_deep(base: toml::Value, over: toml::Value) -> toml::Value {
	match (base, over) {
		(toml::Value::Table(mut a), toml::Value::Table(b)) => {
			for (k, v) in b {
				let merged = match a.remove(&k) {
					Some(existing) => merge_deep(existing, v),
					None => v,
				};
				a.insert(k, merged);
			}
			toml::Value::Table(a)
		}
		(_, over) => over,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn read_value_parses_leading_section_header() {
		let dir = std::env::temp_dir().join(format!("cfgio_rv_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let p = dir.join("c.toml");
		std::fs::write(&p, "[section]\nenabled = true\n").unwrap();
		let v = read_value(&p).expect("read_value should parse a document");
		let enabled = v
			.get("section")
			.and_then(|s| s.get("enabled"))
			.and_then(|b| b.as_bool());
		assert_eq!(enabled, Some(true));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn merged_value_merges_project_section_over_missing_user() {
		let dir = std::env::temp_dir().join(format!("cfgio_ll_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let user = dir.join("user.toml");
		let project = dir.join("project.toml");
		std::fs::write(&project, "[section]\nenabled = true\n").unwrap();
		let merged = merged_value(&user, &project).expect("merged_value");
		let enabled = merged
			.get("section")
			.and_then(|s| s.get("enabled"))
			.and_then(|b| b.as_bool());
		assert_eq!(enabled, Some(true));
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn merged_value_project_field_wins_and_keeps_the_user_fields_it_omits() {
		let dir = std::env::temp_dir().join(format!("cfgio_ovr_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let user = dir.join("user.toml");
		let project = dir.join("project.toml");
		std::fs::write(&user, "[embed]\nurl = \"user-url\"\nkey = \"secret\"\n").unwrap();
		std::fs::write(&project, "[embed]\nmodel = \"proj-model\"\n").unwrap();

		let merged = merged_value(&user, &project).expect("merged_value");
		let embed = merged
			.get("embed")
			.and_then(|v| v.as_table())
			.expect("embed table");
		assert_eq!(
			embed.get("model").and_then(|v| v.as_str()),
			Some("proj-model"),
			"the project leaf wins"
		);
		assert_eq!(
			embed.get("url").and_then(|v| v.as_str()),
			Some("user-url"),
			"a field the project omits is inherited, not lost"
		);
		assert_eq!(
			embed.get("key").and_then(|v| v.as_str()),
			Some("secret"),
			"the key rides along while the project leaves the endpoint alone"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn merged_value_seals_the_key_when_the_project_redirects_the_endpoint() {
		let dir = std::env::temp_dir().join(format!("cfgio_seal_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let user = dir.join("user.toml");
		let project = dir.join("project.toml");
		std::fs::write(&user, "[embed]\nurl = \"user-url\"\nkey = \"secret\"\n").unwrap();
		std::fs::write(&project, "[embed]\nurl = \"http://attacker.example/v1\"\n").unwrap();

		let merged = merged_value(&user, &project).expect("merged_value");
		let embed = merged
			.get("embed")
			.and_then(|v| v.as_table())
			.expect("embed table");
		assert_eq!(
			embed.get("url").and_then(|v| v.as_str()),
			Some("http://attacker.example/v1"),
			"the redirect itself still applies"
		);
		assert_eq!(
			embed.get("key").and_then(|v| v.as_str()),
			None,
			"a cloned repo redirecting the endpoint must not harvest the user's credential"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}

	#[test]
	fn merge_deep_merges_nested_tables_at_depth() {
		let base: toml::Value = "[a.b]\nx = 1\ny = 2\n"
			.parse::<toml::Table>()
			.unwrap()
			.into();
		let over: toml::Value = "[a.b]\ny = 9\n[a.c]\nz = 3\n"
			.parse::<toml::Table>()
			.unwrap()
			.into();

		let merged = merge_deep(base, over);
		let b = merged.get("a").and_then(|a| a.get("b")).expect("a.b");
		assert_eq!(
			b.get("x").and_then(|v| v.as_integer()),
			Some(1),
			"depth-2 sibling survives"
		);
		assert_eq!(
			b.get("y").and_then(|v| v.as_integer()),
			Some(9),
			"depth-2 leaf overridden"
		);
		assert_eq!(
			merged
				.get("a")
				.and_then(|a| a.get("c"))
				.and_then(|c| c.get("z"))
				.and_then(|v| v.as_integer()),
			Some(3),
			"a table only `over` has is added"
		);
	}

	#[test]
	fn merge_deep_leaf_and_array_in_over_replace_the_base() {
		let base: toml::Value = "[w]\nenabled = true\nroots = [\"a\", \"b\"]\n"
			.parse::<toml::Table>()
			.unwrap()
			.into();
		let over: toml::Value = "[w]\nroots = [\"c\"]\n"
			.parse::<toml::Table>()
			.unwrap()
			.into();

		let merged = merge_deep(base, over);
		let w = merged.get("w").expect("w");
		assert_eq!(
			w.get("enabled").and_then(|v| v.as_bool()),
			Some(true),
			"the scalar `over` omits is kept"
		);
		let roots: Vec<&str> = w
			.get("roots")
			.and_then(|v| v.as_array())
			.expect("roots")
			.iter()
			.filter_map(|v| v.as_str())
			.collect();
		assert_eq!(
			roots,
			vec!["c"],
			"an array is a leaf: replaced, not concatenated"
		);
	}

	#[test]
	fn merge_deep_scalar_in_over_beats_a_table_in_base() {
		// The conflict must sit BELOW the top level: a top-level clash is settled by
		// the plain insert the pre-deep-merge code already did, so it proves nothing.
		let base: toml::Value = "[a.b]\nx = 1\n".parse::<toml::Table>().unwrap().into();
		let over: toml::Value = "[a]\nb = 7\n".parse::<toml::Table>().unwrap().into();

		let merged = merge_deep(base, over);
		assert_eq!(
			merged
				.get("a")
				.and_then(|a| a.get("b"))
				.and_then(|v| v.as_integer()),
			Some(7),
			"mismatched kinds one level down: `over` wins outright"
		);
	}

	#[test]
	fn merged_value_keeps_sections_present_in_only_one_scope() {
		let dir = std::env::temp_dir().join(format!("cfgio_keep_{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let user = dir.join("user.toml");
		let project = dir.join("project.toml");
		std::fs::write(&user, "[reason]\nmodel = \"qwen\"\n").unwrap();
		std::fs::write(&project, "[embed]\nurl = \"p\"\n").unwrap();

		let merged = merged_value(&user, &project).expect("merged_value");
		assert_eq!(
			merged
				.get("reason")
				.and_then(|s| s.get("model"))
				.and_then(|v| v.as_str()),
			Some("qwen"),
			"user-only [reason] survives",
		);
		assert!(
			merged.get("embed").is_some(),
			"project-only [embed] is present too"
		);
		let _ = std::fs::remove_dir_all(&dir);
	}
}
