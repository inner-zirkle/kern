//! Tests extracted from config.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use std::path::PathBuf;

	#[test]
	fn load_gravitons_relative_data_dir_to_cwd() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		let kern = root.join(".kern");
		std::fs::create_dir_all(&kern).unwrap();
		std::fs::write(kern.join("kern.toml"), "data_dir = \".kern/data\"\n").unwrap();

		let cfg = Config::load(&root).expect("load");

		let got = PathBuf::from(&cfg.data_dir);
		assert!(got.is_absolute(), "data_dir must be absolute, got {got:?}");
		assert_eq!(got, root.join(".kern").join("data"));
	}

	#[test]
	fn load_of_a_foreign_root_pins_data_dir_to_that_root() {
		// Regression: with no config file, serde's default pinned data_dir to the
		// *process* cwd — a cross-root load (hub merge) then read its own store.
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".kern")).unwrap();

		let cfg = Config::load(&root).expect("load");
		assert_eq!(
			PathBuf::from(&cfg.data_dir),
			root.join(".kern").join("data"),
			"configless load must land in the passed root, not the process cwd"
		);
	}

	#[test]
	fn kern_dir_env_pins_the_store_base() {
		// pi's integration sets KERN_DIR=<root>/.pi/kern; a build that ignores it
		// silently reads/writes <cwd>/.kern and every session hits a different
		// store (RECALL_PLAN F5b). env is process-global, so restore after.
		let prev = std::env::var_os("KERN_DIR");
		let dir = tempfile::tempdir().unwrap();
		std::env::set_var("KERN_DIR", dir.path());
		let cfg = Config::default_in(std::path::Path::new("/nowhere"));
		match prev {
			Some(p) => std::env::set_var("KERN_DIR", p),
			None => std::env::remove_var("KERN_DIR"),
		}
		assert_eq!(
			PathBuf::from(&cfg.data_dir),
			dir.path().join("data"),
			"KERN_DIR replaces the .kern store base"
		);
	}

	#[test]
	fn log_dir_stays_inside_the_data_dir_kern_owns() {
		let root = Path::new("/proj");
		assert_eq!(
			Config::default_in(root).log_dir(),
			root.join(".kern").join("data").join("logs")
		);

		let mut moved = Config::default_in(root);
		moved.data_dir = "/var/lib/kern/store".into();
		assert_eq!(
			moved.log_dir(),
			PathBuf::from("/var/lib/kern/store/logs"),
			"a relocated store keeps its logs inside itself — the parent may be $HOME"
		);
	}

	#[test]
	fn resolve_root_walks_up_to_nearest_kern_dir() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".kern")).unwrap();
		let deep = root.join("a").join("b");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), root);
	}

	#[test]
	fn resolve_root_returns_start_when_no_kern_ancestor() {
		let dir = tempfile::tempdir().unwrap();
		let start = dir.path().canonicalize().unwrap();
		// Shield from stray .kern dirs in parent directories (e.g. /tmp/.kern from a running daemon)
		std::fs::create_dir_all(start.join(".kern")).unwrap();
		assert_eq!(Config::resolve_root(&start), start);
	}

	#[test]
	fn resolve_root_gravitons_at_git_root_when_no_kern() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".git")).unwrap();
		let deep = root.join("a").join("b");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), root);
	}

	#[test]
	fn resolve_root_detects_git_as_a_file() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/x\n").unwrap();
		let deep = root.join("a");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), root);
	}

	#[test]
	fn resolve_root_innermost_git_wins() {
		let dir = tempfile::tempdir().unwrap();
		let outer = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(outer.join(".git")).unwrap();
		let inner = outer.join("project");
		std::fs::create_dir_all(inner.join(".git")).unwrap();
		let deep = inner.join("src");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), inner);
	}

	#[test]
	fn resolve_root_prefers_git_root_over_deeper_kern() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".git")).unwrap();
		let sub = root.join("sub");
		std::fs::create_dir_all(sub.join(".kern")).unwrap();
		let deep = sub.join("deep");
		std::fs::create_dir_all(&deep).unwrap();

		assert_eq!(Config::resolve_root(&deep), root);
	}

	#[test]
	fn default_in_pins_data_dir_to_the_given_cwd_deterministically() {
		let cwd = Path::new("some_project_root");
		let cfg = Config::default_in(cwd);
		assert_eq!(
			cfg.data_dir,
			cwd.join(".kern").join("data").to_string_lossy()
		);
		assert_eq!(Config::default_in(cwd).data_dir, cfg.data_dir);
	}

	#[test]
	fn validate_requires_embed_and_surfaces_sub_config_invariants() {
		let cfg = Config::default_in(Path::new("x"));
		assert!(cfg.validate().is_ok(), "shipped defaults validate");

		let mut no_embed = Config::default_in(Path::new("x"));
		no_embed.embed.url = String::new();
		assert!(no_embed.validate().unwrap_err().contains("embed.url"));

		let mut bad_ingest = Config::default_in(Path::new("x"));
		bad_ingest.ingest.dedup_threshold = 2.0;
		let err = bad_ingest.validate().unwrap_err();
		assert!(
			err.contains("ingest"),
			"sub-config error is surfaced + tagged: {err}"
		);

		let mut bad_retr = Config::default_in(Path::new("x"));
		bad_retr.retrieval.seed_k = 0;
		let err = bad_retr.validate().unwrap_err();
		assert!(
			err.contains("retrieval"),
			"retrieval error surfaced + tagged: {err}"
		);
		assert!(err.contains("seed_k"), "the specific issue is named: {err}");
	}

	fn root_with(toml: &str) -> tempfile::TempDir {
		let dir = tempfile::tempdir().unwrap();
		let kern = dir.path().join(".kern");
		std::fs::create_dir_all(&kern).unwrap();
		std::fs::write(kern.join("kern.toml"), toml).unwrap();
		dir
	}

	#[test]
	fn configless_load_defaults_to_relaxed() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().canonicalize().unwrap();
		std::fs::create_dir_all(root.join(".kern")).unwrap();
		let cfg = Config::load(&root).expect("load");
		assert_eq!(cfg.preset, Preset::Relaxed);
		assert_eq!(cfg.retrieval.seed_k, 25);
		assert_eq!(cfg.heat.half_life_secs, 30 * 24 * 60 * 60);
	}

	#[test]
	fn preset_key_applies_its_tier() {
		let dir = root_with("preset = \"tight\"\n");
		let cfg = Config::load(&dir.path().canonicalize().unwrap()).expect("load");
		assert_eq!(cfg.preset, Preset::Tight);
		assert_eq!(cfg.retrieval.seed_k, 10);
		assert_eq!(cfg.heat.half_life_secs, 3 * 24 * 60 * 60);
	}

	#[test]
	fn preset_managed_sections_refuse_to_load() {
		for section in ["heat", "ingest", "retrieval"] {
			let dir = root_with(&format!("[{section}]\nanything = 1\n"));
			let err = Config::load(&dir.path().canonicalize().unwrap()).unwrap_err();
			let msg = err.to_string();
			assert!(
				msg.contains(section) && msg.contains("preset"),
				"[{section}] must be refused with a pointer to presets: {msg}"
			);
		}
	}

	// The same load-bearing point for the review lifecycle: a `review_policy` a
	// `kern.toml` cannot express is a policy nobody has, and the hold half of
	// ROADMAP item 21 is then unreachable no matter what the query surface
	// accepts. Both directions, because the exception must stay an exception.
	#[test]
	fn a_real_kern_toml_can_set_review_policy_and_nothing_else_in_ingest() {
		let dir = root_with("[ingest]\nreview_policy = { inline = \"pending\" }\n");
		let root = dir.path().canonicalize().unwrap();
		let cfg = Config::load_with_user(&root.join("no-such-user.toml"), &root)
			.expect("review_policy is not preset-managed");
		assert_eq!(
			cfg.ingest.review_policy.get("inline"),
			Some(&base::base_types::ReviewState::Pending),
			"the policy a real file set has to reach the struct the ingest gate reads"
		);

		// The tuning key in the same table is still refused, so the preset stays
		// the only writer of what a preset owns.
		let dir =
			root_with("[ingest]\nreview_policy = { inline = \"pending\" }\ndedup_threshold = 0.5\n");
		let root = dir.path().canonicalize().unwrap();
		let err = Config::load_with_user(&root.join("no-such-user.toml"), &root).unwrap_err();
		assert!(
			err.to_string().contains("preset"),
			"a tuning knob smuggled in beside review_policy must still be refused: {err}"
		);
	}

	// The load-bearing half of per-source retention: a policy a `kern.toml`
	// cannot express is a policy nobody has. `[ingest]` accepts nothing but
	// `review_policy`, so the retention key lives in the two sections that
	// describe the sources themselves — and this proves a real file reaches the
	// struct.
	#[test]
	fn a_real_kern_toml_can_set_per_source_retention() {
		let dir = root_with(
			"[intake]\nretention_secs = 2592000\n\n[watcher]\nenabled = true\nretention_secs = 86400\n",
		);
		let root = dir.path().canonicalize().unwrap();
		let cfg = Config::load_with_user(&root.join("no-such-user.toml"), &root)
			.expect("a user-writable section must load");

		assert_eq!(
			cfg.intake.retention_secs, 2_592_000,
			"30 days on the intake"
		);
		assert_eq!(cfg.watcher.retention_secs, 86_400, "a day on the watcher");
		assert!(cfg.validate().is_ok(), "and the loaded config validates");
	}

	#[test]
	fn project_preset_beats_user_preset() {
		let dir = tempfile::tempdir().unwrap();
		let user = dir.path().join("user.toml");
		std::fs::write(&user, "preset = \"tight\"\n").unwrap();
		let root = root_with("preset = \"medium\"\n");
		let cfg = Config::load_with_user(&user, &root.path().canonicalize().unwrap()).expect("load");
		assert_eq!(cfg.preset, Preset::Medium);
		assert_eq!(cfg.retrieval.seed_k, 15);
	}

	#[test]
	fn unknown_preset_name_refuses_to_load() {
		let dir = root_with("preset = \"loose\"\n");
		let err = Config::load(&dir.path().canonicalize().unwrap()).unwrap_err();
		assert!(
			err.to_string().contains("relaxed"),
			"the error names the valid tiers: {err}"
		);
	}

	#[test]
	fn egress_warnings_flags_a_public_embed_url_and_silences_loopback() {
		let mut cfg = Config::default_in(Path::new("x"));
		// loopback embed url — no warning
		cfg.embed.url = "http://127.0.0.1:11434".into();
		assert!(cfg.egress_warnings().is_empty(), "loopback is local");

		// public embed url — one warning, naming embed.url
		cfg.embed.url = "https://api.openai.com".into();
		let w = cfg.egress_warnings();
		assert_eq!(w.len(), 1);
		assert!(w[0].contains("embed.url"), "names the field: {w:?}");
		assert!(w[0].contains("api.openai.com"), "names the host: {w:?}");
	}

	#[test]
	fn egress_warnings_reports_one_per_non_local_url() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "https://api.openai.com".into();
		cfg.reason.url = "http://203.0.113.5".into();
		let w = cfg.egress_warnings();
		assert_eq!(w.len(), 2, "one per non-local url: {w:?}");
		// empty reason.url inherits embed.url silently — must not double-count
		cfg.reason.url = String::new();
		assert_eq!(cfg.egress_warnings().len(), 1);
	}

	#[test]
	fn native_knob_warnings_silent_on_default_loopback() {
		let cfg = Config::default_in(Path::new("x"));
		// default is loopback Ollama, native, default knobs — nothing to warn
		assert!(
			cfg.native_knob_warnings().is_empty(),
			"{:?}",
			cfg.native_knob_warnings()
		);
	}

	#[test]
	fn native_knob_warnings_silent_on_a_v1_endpoint_with_default_knobs() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "http://localhost:8000/v1".into();
		// /v1 endpoint, but knobs still at default — not "trying to tune", silent
		assert!(
			cfg.native_knob_warnings().is_empty(),
			"{:?}",
			cfg.native_knob_warnings()
		);
	}

	#[test]
	fn native_knob_warnings_names_a_tuned_knob_on_a_v1_endpoint() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "http://localhost:8000/v1".into();
		cfg.embed.num_ctx = 8192; // non-default, ignored on /v1
		cfg.embed.keep_alive = "30m".into();
		let w = cfg.native_knob_warnings();
		assert_eq!(w.len(), 2, "one per tuned knob: {w:?}");
		assert!(w[0].contains("embed.num_ctx"), "names the knob: {w:?}");
		assert!(w[0].contains("8192"), "names the value: {w:?}");
		assert!(w[1].contains("embed.keep_alive"), "names the knob: {w:?}");
		assert!(w[1].contains("30m"), "names the value: {w:?}");
		// native (non-/v1) Ollama endpoint with the same tuned knobs — silent,
		// because there the knobs ARE sent
		cfg.embed.url = "http://localhost:11434".into();
		assert!(
			cfg.native_knob_warnings().is_empty(),
			"native endpoint honours the knobs"
		);
	}

	#[test]
	fn embed_config_default_carries_the_native_knob_constants() {
		let c = crate::config::EmbedConfig::default();
		assert_eq!(c.num_ctx, llm::EMBED_NUM_CTX);
		assert_eq!(c.keep_alive, llm::EMBED_KEEP_ALIVE);
	}

	#[test]
	fn wsl_loopback_warnings_silent_off_wsl() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "http://127.0.0.1:11434".into();
		// not WSL -> loopback is correct, silent
		assert!(cfg.wsl_loopback_warnings_for(false).is_empty());
	}

	#[test]
	fn wsl_loopback_warnings_names_a_loopback_endpoint_under_wsl() {
		let mut cfg = Config::default_in(Path::new("x"));
		cfg.embed.url = "http://127.0.0.1:11434".into();
		cfg.reason.url = "http://localhost:11434".into();
		let w = cfg.wsl_loopback_warnings_for(true);
		assert_eq!(w.len(), 2, "one per loopback endpoint: {w:?}");
		assert!(w[0].contains("embed.url"), "names the field: {w:?}");
		assert!(w[0].contains("127.0.0.1"), "names the host: {w:?}");
		assert!(w[0].contains("WSL"), "names the cause: {w:?}");
		// a non-loopback local URL (the WSL2 gateway) is already correct — silent
		cfg.embed.url = "http://172.27.176.1:11434".into();
		assert_eq!(
			cfg.wsl_loopback_warnings_for(true).len(),
			1,
			"gateway IP is not loopback"
		);
	}

	#[test]
	fn is_loopback_url_pins_loopback_only() {
		assert!(llm::is_loopback_url("http://127.0.0.1:11434"));
		assert!(llm::is_loopback_url("http://127.1.2.3:11434"));
		assert!(llm::is_loopback_url("http://localhost:11434"));
		assert!(llm::is_loopback_url("http://[::1]:8080"));
		// RFC1918 is local but NOT loopback — the WSL2 gateway falls here
		assert!(!llm::is_loopback_url("http://172.27.176.1:11434"));
		assert!(!llm::is_loopback_url("http://10.0.0.1:11434"));
		assert!(!llm::is_loopback_url("https://api.openai.com"));
	}
}
mod io_tests {
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
mod preset_tests {
	use super::*;
	#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
	#[serde(default)]
	pub struct HeatConfig {
		pub half_life_secs: u64,
		// Dimensionless heat unit — not a ratio or duration. The only deposit there
		// is: heat measures use, and the tick is not a user (ROADMAP item 32).
		pub deposit_access: f32,
	}

	impl Default for HeatConfig {
		fn default() -> Self {
			Self {
				half_life_secs: 7 * 24 * 60 * 60,
				deposit_access: 1.0,
			}
		}
	}

	use std::path::Path;

	fn applied(p: Preset) -> Config {
		let mut cfg = Config::default_in(Path::new("x"));
		p.apply(&mut cfg);
		cfg
	}

	#[test]
	fn every_preset_yields_a_valid_config() {
		for p in [Preset::Relaxed, Preset::Medium, Preset::Tight] {
			assert!(applied(p).validate().is_ok(), "{p:?} must validate");
		}
	}

	#[test]
	fn medium_matches_the_neutral_struct_defaults() {
		// The sub-config defaults are the medium anchor; this pins them together
		// so neither can drift without failing here.
		let t = Preset::Medium.tuning();
		let r = RetrievalConfig::default();
		assert_eq!(t.half_life_secs, HeatConfig::default().half_life_secs);
		assert_eq!(t.dedup_threshold, IngestConfig::default().dedup_threshold);
		assert_eq!(t.seed_k, r.seed_k);
		assert_eq!(t.max_expansions, r.max_expansions);
		assert_eq!(t.max_deliver_results, r.max_deliver_results);
	}

	#[test]
	fn relaxed_and_tight_move_the_knobs_in_opposite_directions() {
		let r = applied(Preset::Relaxed);
		let m = applied(Preset::Medium);
		let t = applied(Preset::Tight);
		assert!(r.heat.half_life_secs > m.heat.half_life_secs);
		assert!(t.heat.half_life_secs < m.heat.half_life_secs);
		assert!(r.retrieval.max_deliver_results > m.retrieval.max_deliver_results);
		assert!(t.retrieval.max_deliver_results < m.retrieval.max_deliver_results);
		assert!(r.ingest.dedup_threshold > m.ingest.dedup_threshold);
		assert!(t.ingest.dedup_threshold < m.ingest.dedup_threshold);
	}

	#[test]
	fn the_default_preset_is_relaxed() {
		assert_eq!(Preset::default(), Preset::Relaxed);
	}
}
mod secrets_tests {
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
mod embed_tests {
	use super::*;

	#[test]
	fn default_uses_the_shared_constants() {
		let c = EmbedConfig::default();
		assert_eq!(c.url, DEFAULT_EMBED_URL);
		assert_eq!(c.model, DEFAULT_EMBED_MODEL);
		assert!(c.key.is_empty(), "no API key by default (local Ollama)");
	}
}
mod gnn_tests {
	use super::*;
	use base::base_constants::KERN_CAP_DISABLED;

	#[test]
	fn default_bounds_resident_kerns_conservatively() {
		// 128 is a safety bound, not a tuning knob: normal use is <10 kerns, and
		// eviction is proven safe (see GraphConfig::default). `usize::MAX` stays
		// the uncapped marker for an explicit opt-out.
		assert_eq!(GraphConfig::default().max_kerns, 128);
		assert_eq!(
			KERN_CAP_DISABLED,
			usize::MAX,
			"sentinel value is the uncapped marker"
		);
	}

	#[test]
	fn default_arms_disk_spill() {
		// disk_threshold 0 = every store-backed graph loads its ANN indexes from
		// mmap'd DiskANN snapshots (RECALL_PLAN F4) — the ~4.5s per-CLI-invocation
		// HNSW rebuild that blew pi's tool timeouts. The item-75 crash window that
		// kept it disabled is closed (staging-dir atomic swap in build_and_save).
		assert_eq!(GraphConfig::default().disk_threshold, 0);
	}
}
mod graph_tests {
	use super::*;
	use base::base_types::ReviewState;

	#[test]
	fn default_validates_and_bad_knobs_are_rejected() {
		assert!(
			IngestConfig::default().validate().is_ok(),
			"shipped defaults are valid"
		);

		let out_of_range = IngestConfig {
			dedup_threshold: 2.0,
			..Default::default()
		};
		assert!(
			out_of_range.validate().is_err(),
			"threshold outside [0,1] is rejected"
		);
	}

	#[test]
	fn an_unknown_review_policy_scheme_is_flagged() {
		let typo = IngestConfig {
			review_policy: ReviewPolicy::from([("files".to_string(), ReviewState::Pending)]),
			..Default::default()
		};
		assert!(
			typo.validate().unwrap_err().contains("review_policy"),
			"a scheme that names nothing must not read as a working policy"
		);

		let good = IngestConfig {
			review_policy: ReviewPolicy::from([("file".to_string(), ReviewState::Pending)]),
			..Default::default()
		};
		assert!(good.validate().is_ok(), "a real scheme is accepted");
	}
}
mod hub_tests {
	use super::*;

	#[test]
	fn defaults_are_on_with_sane_tunables() {
		let c = IntakeConfig::default();
		assert!(c.enabled);
		assert_eq!(c.dir, ".kern/intake");
		assert_eq!(c.poll_secs, 5);
		assert_eq!(c.done_retention_secs, 604_800, "7 days in seconds");
		assert_eq!(c.retention_secs, 0, "no standing TTL unless a host asks");
	}

	#[test]
	fn validate_rejects_a_retention_that_can_never_become_a_deadline() {
		let unusable = IntakeConfig {
			retention_secs: u64::MAX,
			..Default::default()
		};
		assert!(
			unusable.validate().unwrap_err().contains("overflows"),
			"a retention no clock can represent is refused at load, not per drain"
		);

		let thirty_days = IntakeConfig {
			retention_secs: 30 * 24 * 60 * 60,
			..Default::default()
		};
		assert!(thirty_days.validate().is_ok(), "a real policy is accepted");
	}

	#[test]
	fn validate_rejects_zero_poll_only_when_enabled() {
		assert!(
			IntakeConfig::default().validate().is_ok(),
			"default (enabled, non-zero poll) is valid"
		);

		let zero_poll = IntakeConfig {
			enabled: true,
			poll_secs: 0,
			..Default::default()
		};
		assert!(zero_poll.validate().unwrap_err().contains("poll_secs"));

		let disabled_zero = IntakeConfig {
			enabled: false,
			poll_secs: 0,
			..Default::default()
		};
		assert!(
			disabled_zero.validate().is_ok(),
			"disabled intake ignores its poll interval"
		);
	}
}
mod ingest_tests {
	use super::*;

	#[test]
	fn default_config_is_valid() {
		assert!(
			RetrievalConfig::default().validate().is_empty(),
			"shipped defaults must validate"
		);
	}

	#[test]
	fn weights_not_summing_to_one_are_flagged() {
		let mut cfg = RetrievalConfig::default();
		cfg.weights_hybrid.content = 0.9;
		let errs = cfg.validate();
		assert!(
			errs.iter().any(|e| e.contains("weights_hybrid")),
			"got {errs:?}"
		);
	}

	#[test]
	fn out_of_range_bm25_params_are_flagged() {
		let bad_b = RetrievalConfig {
			bm25_b: 2.0,
			..Default::default()
		};
		assert!(
			bad_b.validate().iter().any(|e| e.contains("bm25_b")),
			"bm25_b > 1"
		);

		let neg_k1 = RetrievalConfig {
			bm25_k1: -0.5,
			..Default::default()
		};
		assert!(
			neg_k1.validate().iter().any(|e| e.contains("bm25_k1")),
			"negative bm25_k1"
		);
	}

	// A typo'd scheme weights nothing and reads exactly like a working knob, which
	// is the failure an operator cannot see from the ranking.
	#[test]
	fn an_unknown_or_negative_source_trust_key_is_flagged() {
		let typo = RetrievalConfig {
			source_trust: BTreeMap::from([("files".to_string(), 0.5)]),
			..Default::default()
		};
		assert!(
			typo.validate().iter().any(|e| e.contains("files")),
			"got {:?}",
			typo.validate()
		);

		let negative = RetrievalConfig {
			source_trust: BTreeMap::from([("file".to_string(), -0.5)]),
			..Default::default()
		};
		assert!(
			negative.validate().iter().any(|e| e.contains("file")),
			"got {:?}",
			negative.validate()
		);

		let ok = RetrievalConfig {
			source_trust: BTreeMap::from([("agent".to_string(), 1.5)]),
			..Default::default()
		};
		assert!(
			ok.validate().is_empty(),
			"a real scheme above 1.0 lifts it over baseline and is valid: {:?}",
			ok.validate()
		);
	}

	#[test]
	fn retrieval_breaking_values_are_flagged() {
		let neg_rrf = RetrievalConfig {
			rrf_k: -1.0,
			..Default::default()
		};
		assert!(
			neg_rrf.validate().iter().any(|e| e.contains("rrf_k")),
			"negative rrf_k"
		);

		let zero_seed = RetrievalConfig {
			seed_k: 0,
			..Default::default()
		};
		assert!(
			zero_seed.validate().iter().any(|e| e.contains("seed_k")),
			"seed_k 0"
		);

		let zero_deliver = RetrievalConfig {
			max_deliver_results: 0,
			..Default::default()
		};
		assert!(
			zero_deliver
				.validate()
				.iter()
				.any(|e| e.contains("max_deliver_results")),
			"max_deliver_results 0"
		);

		let neg_gravity = RetrievalConfig {
			gravity_weight: -0.1,
			..Default::default()
		};
		assert!(
			neg_gravity
				.validate()
				.iter()
				.any(|e| e.contains("gravity_weight")),
			"negative gravity_weight"
		);

		let zero_rrf = RetrievalConfig {
			rrf_k: 0.0,
			..Default::default()
		};
		assert!(
			!zero_rrf.validate().iter().any(|e| e.contains("rrf_k")),
			"rrf_k 0 is valid, must not flag"
		);

		let neg_boost = RetrievalConfig {
			lexical_top_boost: -0.1,
			..Default::default()
		};
		assert!(
			neg_boost
				.validate()
				.iter()
				.any(|e| e.contains("lexical_top_boost")),
			"negative lexical_top_boost"
		);
	}
}
mod intake_tests {
	use super::*;

	#[test]
	fn open_private_append_creates_then_appends_without_truncating() {
		let dir = tempfile::tempdir().unwrap();
		let p = dir.path().join("d.log");

		{
			use std::io::Write;
			let mut f = open_private_append(&p).expect("create");
			f.write_all(b"first\n").unwrap();
		}
		{
			use std::io::Write;
			let mut f = open_private_append(&p).expect("reopen");
			f.write_all(b"second\n").unwrap();
		}

		assert_eq!(
			std::fs::read_to_string(&p).unwrap(),
			"first\nsecond\n",
			"a reopen must not erase what explains the restart"
		);
	}

	#[cfg(unix)]
	#[test]
	fn open_private_append_tightens_a_world_readable_file() {
		use std::os::unix::fs::PermissionsExt;
		let dir = tempfile::tempdir().unwrap();
		let p = dir.path().join("loose.log");
		std::fs::write(&p, "x").unwrap();
		std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();

		open_private_append(&p).expect("open");

		let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
		assert_eq!(
			mode, 0o600,
			"captured text must not stay readable: {mode:o}"
		);
	}
}
mod reload_tests {
	use super::*;

	#[test]
	fn effective_roots_falls_back_to_cwd_when_enabled_and_empty() {
		let cfg = WatcherConfig {
			enabled: true,
			..Default::default()
		};
		assert_eq!(
			cfg.effective_roots(Path::new("/proj")),
			vec![PathBuf::from("/proj")]
		);
	}

	#[test]
	fn effective_roots_uses_configured_roots_when_present() {
		let cfg = WatcherConfig {
			enabled: true,
			roots: vec!["a".into(), "/elsewhere/b".into()],
			..Default::default()
		};
		assert_eq!(
			cfg.effective_roots(Path::new("/proj")),
			vec![PathBuf::from("/proj/a"), PathBuf::from("/elsewhere/b")],
			"configured roots win over the cwd fallback, and a relative one is \
			 pinned to cwd so event paths and the denied prefixes share a frame"
		);
	}

	#[test]
	fn effective_roots_is_empty_when_disabled() {
		let cfg = WatcherConfig {
			enabled: false,
			roots: vec!["a".into()],
			..Default::default()
		};
		assert!(
			cfg.effective_roots(Path::new("/proj")).is_empty(),
			"a disabled watcher has nothing to watch even with roots set"
		);
	}
}
mod serve_tests {
	use super::*;

	#[test]
	fn one_log_file_per_spawn_arg() {
		let dir = Path::new("/tmp/kern-logs");
		assert_eq!(log_path(dir, "hub"), dir.join("hub.log"));
		assert_eq!(
			log_path(dir, "--daemon"),
			dir.join("daemon.log"),
			"the leading dashes are not part of the name"
		);
	}

	#[test]
	fn opening_creates_the_dir_and_appends_rather_than_truncating() {
		let dir = tempfile::tempdir().unwrap();
		let logs = dir.path().join("nested").join("logs");

		{
			use std::io::Write;
			let (mut out, _) = open(&logs, "hub").expect("first open creates the dir");
			out.write_all(b"first\n").unwrap();
		}
		{
			use std::io::Write;
			let (mut out, _) = open(&logs, "hub").expect("reopen");
			out.write_all(b"second\n").unwrap();
		}

		assert_eq!(
			std::fs::read_to_string(log_path(&logs, "hub")).unwrap(),
			"first\nsecond\n",
			"a reopen must not erase what explains the restart"
		);
	}

	#[test]
	fn an_unopenable_log_is_an_error_the_caller_can_see() {
		let dir = tempfile::tempdir().unwrap();
		let blocked = dir.path().join("not-a-dir");
		std::fs::write(&blocked, "i am a file").unwrap();

		assert!(
			open(&blocked, "hub").is_err(),
			"create_dir_all over an existing file must fail, so the fallback is reachable"
		);
	}
}
mod llm_timeout_tests {
	use super::*;

	#[test]
	fn the_unconfigured_timeout_is_the_const_it_replaced() {
		let cfg = Config::default();
		assert_eq!(
			cfg.reason.timeout_secs,
			llm::DEFAULT_REASON_TIMEOUT_SECS,
			"an unconfigured kern must post under exactly the old ceiling"
		);
	}
}
