//! Deep merge lets a project scope inherit the fields it omits. That is the
//! point — a user-level `key` should keep working when a project sets only
//! `model`. It also means a project scope that *redirects* an endpoint inherits
//! the credential minted for a different one: a cloned repo committing
//! `[embed] url = "http://attacker.example/v1"` would otherwise harvest the
//! user's `sk-live-…` on the first embed call, and `reason_key`
//! fall back to `embed.key`, so redirecting any one endpoint reaches it.
//!
//! So a scope that overrides `url` does not inherit that section's `key`. It
//! must supply its own or go without.

const URL: &str = "url";
const KEY: &str = "key";

pub fn seal_redirected(mut merged: toml::Value, project: &toml::Value) -> toml::Value {
	let Some(proj) = project.as_table() else {
		return merged;
	};
	let redirected: Vec<String> = proj
		.iter()
		.filter(|(_, v)| v.get(URL).is_some() && v.get(KEY).is_none())
		.map(|(k, _)| k.clone())
		.collect();
	if let Some(out) = merged.as_table_mut() {
		for section in redirected {
			if let Some(toml::Value::Table(t)) = out.get_mut(&section) {
				t.remove(KEY);
			}
		}
	}
	merged
}

#[cfg(test)]
mod tests {
	use super::*;

	fn table(s: &str) -> toml::Value {
		toml::Value::Table(s.parse::<toml::Table>().expect("test toml parses"))
	}

	fn key_of(v: &toml::Value, section: &str) -> Option<String> {
		v.get(section)?.get(KEY)?.as_str().map(|s| s.to_string())
	}

	#[test]
	fn a_project_that_redirects_the_url_does_not_inherit_the_users_key() {
		let merged = table("[embed]\nurl = \"http://attacker.example/v1\"\nkey = \"sk-live\"\n");
		let project = table("[embed]\nurl = \"http://attacker.example/v1\"\n");
		let sealed = seal_redirected(merged, &project);
		assert_eq!(
			key_of(&sealed, "embed"),
			None,
			"redirecting the endpoint must not carry the credential minted for another one"
		);
		assert!(
			sealed.get("embed").and_then(|e| e.get(URL)).is_some(),
			"only the key is sealed; the redirect itself still applies"
		);
	}

	#[test]
	fn a_project_that_leaves_the_url_alone_keeps_inheriting_the_key() {
		let merged =
			table("[embed]\nurl = \"https://api.openai.com/v1\"\nkey = \"sk-live\"\nmodel = \"m\"\n");
		let project = table("[embed]\nmodel = \"m\"\n");
		let sealed = seal_redirected(merged, &project);
		assert_eq!(
			key_of(&sealed, "embed").as_deref(),
			Some("sk-live"),
			"the whole point of layering: a user-level key survives a project-level model"
		);
	}

	#[test]
	fn a_project_supplying_its_own_key_with_its_own_url_keeps_it() {
		let merged = table("[embed]\nurl = \"http://local/v1\"\nkey = \"project-key\"\n");
		let project = table("[embed]\nurl = \"http://local/v1\"\nkey = \"project-key\"\n");
		let sealed = seal_redirected(merged, &project);
		assert_eq!(key_of(&sealed, "embed").as_deref(), Some("project-key"));
	}

	#[test]
	fn sealing_is_per_section_not_global() {
		let merged = table(
			"[embed]\nurl = \"http://attacker/v1\"\nkey = \"sk-live\"\n\
			 [reason]\nurl = \"https://api.openai.com/v1\"\nkey = \"sk-live\"\n",
		);
		let project = table("[embed]\nurl = \"http://attacker/v1\"\n");
		let sealed = seal_redirected(merged, &project);
		assert_eq!(
			key_of(&sealed, "embed"),
			None,
			"the redirected section is sealed"
		);
		assert_eq!(
			key_of(&sealed, "reason").as_deref(),
			Some("sk-live"),
			"a section the project never touched is untouched"
		);
	}
}
