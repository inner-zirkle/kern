pub fn split(text: &str, hint: &str, llm: Option<&dyn Fn(&str) -> String>) -> Vec<String> {
	if let Some(llm_fn) = llm {
		let result = llm_split(text, hint, llm_fn);
		if !result.is_empty() {
			return result;
		}
	}
	paragraph_split(text)
}

pub(crate) fn llm_split(text: &str, hint: &str, llm: &dyn Fn(&str) -> String) -> Vec<String> {
	let context = if hint.is_empty() {
		String::new()
	} else {
		format!(" This text describes {hint}.")
	};
	let prompt = format!(
		"Extract the key factual statements from the following text.{context} \
		 One statement per line. No numbering. No commentary.\n\n{text}"
	);
	let response = llm(&prompt);
	if response.is_empty() {
		return Vec::new();
	}
	trim_nonempty(response.lines())
}

pub(crate) fn paragraph_split(text: &str) -> Vec<String> {
	let chunks = trim_nonempty(text.split("\n\n"));
	if !chunks.is_empty() {
		return chunks;
	}
	let trimmed = text.trim();
	if trimmed.is_empty() {
		Vec::new()
	} else {
		vec![trimmed.to_string()]
	}
}

fn trim_nonempty<'a>(parts: impl Iterator<Item = &'a str>) -> Vec<String> {
	parts
		.map(|p| p.trim().to_string())
		.filter(|p| !p.is_empty())
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn paragraph_split_single_and_multi() {
		assert_eq!(
			paragraph_split("one paragraph"),
			vec!["one paragraph".to_string()]
		);
		assert_eq!(
			paragraph_split("first\n\nsecond"),
			vec!["first".to_string(), "second".to_string()]
		);
		assert_eq!(
			paragraph_split("  a  \n\n\n\n  b  "),
			vec!["a".to_string(), "b".to_string()]
		);
	}

	#[test]
	fn whitespace_or_empty_input_yields_no_chunks() {
		assert!(paragraph_split("").is_empty(), "empty -> no chunks");
		assert!(
			paragraph_split("   \n\n \t ").is_empty(),
			"whitespace-only -> no bogus chunk"
		);
	}

	#[test]
	fn split_uses_llm_statements_when_present() {
		let llm = |_p: &str| "stmt one\nstmt two".to_string();
		assert_eq!(
			split("raw", "", Some(&llm)),
			vec!["stmt one".to_string(), "stmt two".to_string()]
		);
	}

	#[test]
	fn split_falls_back_to_paragraphs_when_llm_returns_empty() {
		let llm = |_p: &str| String::new();
		assert_eq!(
			split("para a\n\npara b", "", Some(&llm)),
			vec!["para a".to_string(), "para b".to_string()],
			"empty LLM response -> paragraph fallback"
		);
	}

	#[test]
	fn split_without_llm_uses_paragraph_split() {
		assert_eq!(
			split("x\n\ny", "", None),
			vec!["x".to_string(), "y".to_string()]
		);
	}

	#[test]
	fn llm_split_folds_hint_into_the_prompt() {
		let seen = std::cell::RefCell::new(String::new());
		let llm = |p: &str| {
			*seen.borrow_mut() = p.to_string();
			"s".to_string()
		};
		let _ = llm_split("body", "rust code", &llm);
		assert!(
			seen.borrow().contains("describes rust code"),
			"hint is named in the prompt"
		);
		let _ = llm_split("body", "", &llm);
		assert!(!seen.borrow().contains("describes"));
	}
}
