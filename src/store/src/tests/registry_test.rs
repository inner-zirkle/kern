//! Tests extracted from registry.rs
#![allow(unused)]
use super::*;

mod tests {
	use super::*;
	use std::time::Duration;

	fn dead_client() -> LlmClient {
		LlmClient::new_embed_only("http://127.0.0.1:1", "test", "")
	}

	#[tokio::test]
	async fn open_dedups_and_touches_on_reopen() {
		let dir = tempfile::tempdir().unwrap();
		let reg = Registry::new();
		let cfg = Config::default();

		let a = reg.open(dir.path(), &cfg, dead_client(), None, None);
		let first_touch = *a.last_touch.read();

		tokio::time::sleep(Duration::from_millis(2)).await;

		let b = reg.open(dir.path(), &cfg, dead_client(), None, None);
		assert!(
			Arc::ptr_eq(&a, &b),
			"re-open of the same dir returns the same StoreEntry"
		);
		assert_eq!(reg.len(), 1, "no duplicate store registered");
		assert!(
			*b.last_touch.read() > first_touch,
			"last_touch advanced on re-open",
		);
	}

	#[tokio::test]
	async fn distinct_dirs_get_isolated_stores() {
		let dir_a = tempfile::tempdir().unwrap();
		let dir_b = tempfile::tempdir().unwrap();
		let reg = Registry::new();
		let cfg = Config::default();

		let a = reg.open(dir_a.path(), &cfg, dead_client(), None, None);
		let b = reg.open(dir_b.path(), &cfg, dead_client(), None, None);

		assert_eq!(reg.len(), 2, "one store per distinct dir");
		assert!(
			!Arc::ptr_eq(&a.graph, &b.graph),
			"distinct dirs never share a graph"
		);
		assert_ne!(a.key, b.key, "distinct dirs get distinct keys");
	}

	#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
	async fn concurrent_open_of_same_dir_yields_one_store() {
		let dir = tempfile::tempdir().unwrap();
		let reg = Arc::new(Registry::new());
		let cfg = Config::default();
		let path = dir.path().to_path_buf();

		let mut handles = Vec::new();
		for _ in 0..4 {
			let reg = reg.clone();
			let cfg = cfg.clone();
			let path = path.clone();
			handles.push(tokio::spawn(async move {
				reg.open(&path, &cfg, dead_client(), None, None)
			}));
		}
		let mut entries = Vec::new();
		for h in handles {
			entries.push(h.await.unwrap());
		}

		for e in &entries[1..] {
			assert!(
				Arc::ptr_eq(&entries[0], e),
				"all concurrent opens share one StoreEntry"
			);
		}
		assert_eq!(
			reg.len(),
			1,
			"exactly one store registered despite the race"
		);
	}
}
