// Filesystem events are racy (especially Windows): these tests assert on observed
// *kinds* within a time budget, never exact ordering or counts.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tempfile::TempDir;
use tokio::sync::Mutex;

use kern::watcher::{
	FileWatcher, IgnoreRules, IngestPipeline, IngestRecord, IngestSink, WatchEvent, WatchKind,
};

const POLL_BUDGET: Duration = Duration::from_secs(5);

async fn collect_events(w: &mut FileWatcher, budget: Duration) -> Vec<WatchEvent> {
	let mut out = Vec::new();
	let deadline = tokio::time::Instant::now() + budget;
	loop {
		let now = tokio::time::Instant::now();
		if now >= deadline {
			break;
		}
		match tokio::time::timeout(deadline - now, w.next_event()).await {
			Ok(Some(ev)) => out.push(ev),
			Ok(None) => break,
			Err(_) => break,
		}
	}
	out
}

// Use only for *presence* assertions: early-exits once `done` holds. Negative
// assertions must use `collect_events`, which drains the full budget.
async fn collect_until(
	w: &mut FileWatcher,
	budget: Duration,
	done: impl Fn(&[WatchEvent]) -> bool,
) -> Vec<WatchEvent> {
	let mut out = Vec::new();
	let deadline = tokio::time::Instant::now() + budget;
	loop {
		if done(&out) {
			break;
		}
		let now = tokio::time::Instant::now();
		if now >= deadline {
			break;
		}
		match tokio::time::timeout(deadline - now, w.next_event()).await {
			Ok(Some(ev)) => out.push(ev),
			Ok(None) => break,
			Err(_) => break,
		}
	}
	out
}

fn has_kind(events: &[WatchEvent], path: &PathBuf, want: fn(&WatchKind) -> bool) -> bool {
	events.iter().any(|e| &e.path == path && want(&e.kind))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_modify_delete_cycle_emits_expected_events() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path().to_path_buf();
	let mut w = FileWatcher::new(vec![root.clone()], IgnoreRules::empty()).unwrap();

	// Let notify install its OS-level hook before we mutate.
	tokio::time::sleep(Duration::from_millis(100)).await;

	let file = root.join("a.txt");
	tokio::fs::write(&file, b"hello").await.unwrap();
	tokio::time::sleep(Duration::from_millis(150)).await;
	tokio::fs::write(&file, b"world").await.unwrap();
	tokio::time::sleep(Duration::from_millis(150)).await;
	tokio::fs::remove_file(&file).await.unwrap();

	// Windows often collapses the initial write into Modified — accept either.
	let want = |evs: &[WatchEvent]| {
		has_kind(evs, &file, |k| {
			matches!(k, WatchKind::Created | WatchKind::Modified)
		}) && has_kind(evs, &file, |k| matches!(k, WatchKind::Deleted))
	};
	let events = collect_until(&mut w, POLL_BUDGET, want).await;

	let created_or_modified = has_kind(&events, &file, |k| {
		matches!(k, WatchKind::Created | WatchKind::Modified)
	});
	let deleted = has_kind(&events, &file, |k| matches!(k, WatchKind::Deleted));
	assert!(
		created_or_modified,
		"expected create/modify event for {file:?}, saw {events:?}"
	);
	assert!(
		deleted,
		"expected delete event for {file:?}, saw {events:?}"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debounce_collapses_rapid_modifies_to_one_event() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path().to_path_buf();
	let file = root.join("burst.txt");
	tokio::fs::write(&file, b"seed").await.unwrap();

	// Watch *after* the seed write so its Created doesn't pollute the count.
	let mut w = FileWatcher::new(vec![root.clone()], IgnoreRules::empty()).unwrap();
	tokio::time::sleep(Duration::from_millis(100)).await;

	// 5 writes inside one 50 ms debounce window.
	for i in 0..5u8 {
		tokio::fs::write(&file, [b'x', b'0' + i]).await.unwrap();
		tokio::time::sleep(Duration::from_millis(5)).await;
	}

	// Wait for the debouncer to flush.
	tokio::time::sleep(Duration::from_millis(250)).await;

	let events = collect_events(&mut w, Duration::from_millis(500)).await;
	let for_file: Vec<_> = events.iter().filter(|e| e.path == file).collect();

	// Allow 2: the debouncer can flush mid-burst on slow CI.
	assert!(
		!for_file.is_empty() && for_file.len() <= 2,
		"expected 1-2 coalesced events for {file:?}, got {}: {events:?}",
		for_file.len()
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gitignore_is_respected() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path().to_path_buf();
	tokio::fs::write(root.join(".gitignore"), b"ignored.txt\n")
		.await
		.unwrap();

	let mut w = FileWatcher::new(
		vec![root.clone()],
		IgnoreRules::from_roots(std::slice::from_ref(&root)),
	)
	.unwrap();
	tokio::time::sleep(Duration::from_millis(100)).await;

	let ignored = root.join("ignored.txt");
	let kept = root.join("kept.txt");
	tokio::fs::write(&ignored, b"x").await.unwrap();
	tokio::fs::write(&kept, b"x").await.unwrap();
	tokio::time::sleep(Duration::from_millis(250)).await;

	let events = collect_events(&mut w, Duration::from_millis(800)).await;

	assert!(
		events.iter().any(|e| e.path == kept),
		"expected event for kept.txt, got {events:?}"
	);
	assert!(
		!events.iter().any(|e| e.path == ignored),
		"did not expect event for ignored.txt, got {events:?}"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rename_within_root_emits_renamed_or_delete_create_pair() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path().to_path_buf();
	let src = root.join("old.txt");
	tokio::fs::write(&src, b"content").await.unwrap();

	// Watch after the seed write so its Created doesn't muddy the rename window.
	let mut w = FileWatcher::new(vec![root.clone()], IgnoreRules::empty()).unwrap();
	tokio::time::sleep(Duration::from_millis(100)).await;

	let dst = root.join("new.txt");
	tokio::fs::rename(&src, &dst).await.unwrap();

	// Platforms report either one `Renamed` or a `Deleted` + `Created` pair; accept both.
	let saw_rename = |evs: &[WatchEvent]| {
		let renamed = evs
			.iter()
			.any(|e| matches!(&e.kind, WatchKind::Renamed { from, to } if from == &src && to == &dst));
		let deleted_old = has_kind(evs, &src, |k| matches!(k, WatchKind::Deleted));
		let created_new = has_kind(evs, &dst, |k| matches!(k, WatchKind::Created));
		renamed || (deleted_old && created_new)
	};
	let events = collect_until(&mut w, POLL_BUDGET, saw_rename).await;
	assert!(
		saw_rename(&events),
		"expected Renamed or Deleted+Created for {src:?} -> {dst:?}, saw {events:?}"
	);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn watches_multiple_roots_simultaneously() {
	let tmp_a = TempDir::new().unwrap();
	let tmp_b = TempDir::new().unwrap();
	let root_a = tmp_a.path().to_path_buf();
	let root_b = tmp_b.path().to_path_buf();
	let mut w = FileWatcher::new(vec![root_a.clone(), root_b.clone()], IgnoreRules::empty()).unwrap();
	tokio::time::sleep(Duration::from_millis(100)).await;

	let file_a = root_a.join("a.txt");
	let file_b = root_b.join("b.txt");
	tokio::fs::write(&file_a, b"a").await.unwrap();
	tokio::fs::write(&file_b, b"b").await.unwrap();

	let both = |evs: &[WatchEvent]| {
		let a = has_kind(evs, &file_a, |k| {
			matches!(k, WatchKind::Created | WatchKind::Modified)
		});
		let b = has_kind(evs, &file_b, |k| {
			matches!(k, WatchKind::Created | WatchKind::Modified)
		});
		a && b
	};
	let events = collect_until(&mut w, POLL_BUDGET, both).await;
	assert!(
		both(&events),
		"expected a create/modify event under each root, saw {events:?}"
	);
}

#[derive(Default, Clone)]
struct CapturingSink {
	records: Arc<Mutex<Vec<IngestRecord>>>,
}

#[async_trait::async_trait]
impl IngestSink for CapturingSink {
	async fn ingest(&self, record: IngestRecord) {
		self.records.lock().await.push(record);
	}
}

#[tokio::test]
async fn pipeline_skips_files_over_one_megabyte() {
	let tmp = TempDir::new().unwrap();
	let root = tmp.path().to_path_buf();

	let big = root.join("big.bin");
	let big_bytes = vec![b'a'; (1024 * 1024) + 1];
	tokio::fs::write(&big, &big_bytes).await.unwrap();

	let small = root.join("small.txt");
	tokio::fs::write(&small, b"hi").await.unwrap();

	let sink = CapturingSink::default();
	let pipeline = IngestPipeline::new(sink.clone());

	pipeline
		.handle(WatchEvent {
			path: big.clone(),
			kind: WatchKind::Created,
			ts: SystemTime::now(),
		})
		.await;
	pipeline
		.handle(WatchEvent {
			path: small.clone(),
			kind: WatchKind::Modified,
			ts: SystemTime::now(),
		})
		.await;

	let recs = sink.records.lock().await.clone();
	assert_eq!(
		recs.len(),
		1,
		"only the small file should ingest, got {recs:?}"
	);
	assert!(recs[0].source_uri.starts_with("file://"));
	assert_eq!(recs[0].content, "hi");
	assert_eq!(recs[0].language_hint.as_deref(), Some("txt"));
}

#[tokio::test]
async fn pipeline_drops_delete_events() {
	let tmp = TempDir::new().unwrap();
	let path = tmp.path().join("gone.txt");

	let sink = CapturingSink::default();
	let pipeline = IngestPipeline::new(sink.clone());
	pipeline
		.handle(WatchEvent {
			path,
			kind: WatchKind::Deleted,
			ts: SystemTime::now(),
		})
		.await;

	assert!(sink.records.lock().await.is_empty());
}
