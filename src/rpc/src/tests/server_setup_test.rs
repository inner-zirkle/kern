//! Tests extracted from mcp_tools_setup.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;

	fn state(gravitons: &[&str], thoughts: u64) -> SetupState {
		SetupState {
			gravitons: gravitons.iter().map(|s| s.to_string()).collect(),
			thoughts,
			claim_kinds: 0,
			intake_dir: ".kern/intake".into(),
		}
	}

	#[test]
	fn fresh_project_gets_the_seeding_step() {
		let text = render_setup(&state(&[], 0));
		assert!(text.contains("## Seed gravitons"));
		assert!(text.contains("[todo] gravitons seeded (none)"));
		assert!(text.contains("[todo] memory has content"));
	}

	#[test]
	fn seeded_project_skips_the_seeding_step() {
		let text = render_setup(&state(&["decisions", "architecture"], 12));
		assert!(!text.contains("## Seed gravitons"));
		assert!(text.contains("[done] gravitons seeded (decisions, architecture)"));
		assert!(text.contains("[done] memory has content (12 thoughts)"));
	}

	#[test]
	fn capture_and_verify_are_always_present() {
		for s in [state(&[], 0), state(&["a"], 5)] {
			let text = render_setup(&s);
			assert!(text.contains("## Wire capture into your host"));
			assert!(text.contains("## Verify"));
			assert!(text.contains(".kern/intake"), "intake dir must be inlined");
			assert!(text.contains("degrade"));
			assert!(text.contains("## Tune"), "preset tiers are offered");
		}
	}
}
