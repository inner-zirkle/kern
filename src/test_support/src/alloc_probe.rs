use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
	static BYTES: Cell<usize> = const { Cell::new(0) };
	static PEAK: Cell<usize> = const { Cell::new(0) };
}

// `added` is what the heap grew by, `block` the size of the region handed back —
// they differ on `realloc`, where the caller ends up holding `block` bytes but
// only paid for the difference.
fn record(added: usize, block: usize) {
	let _ = BYTES.try_with(|b| b.set(b.get() + added));
	let _ = PEAK.try_with(|p| p.set(p.get().max(block)));
}

pub struct Counting;

// Every method forwards to `System` untouched. The counters are const-init
// `Cell`s with no destructor, so touching them allocates nothing and cannot
// re-enter the allocator.
unsafe impl GlobalAlloc for Counting {
	unsafe fn alloc(&self, l: Layout) -> *mut u8 {
		record(l.size(), l.size());
		unsafe { System.alloc(l) }
	}
	unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
		record(l.size(), l.size());
		unsafe { System.alloc_zeroed(l) }
	}
	unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
		unsafe { System.dealloc(p, l) }
	}
	unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
		record(new.saturating_sub(l.size()), new);
		unsafe { System.realloc(p, l, new) }
	}
}

#[derive(Debug)]
pub struct Allocated {
	pub total: usize,
	// The largest single block. An N-wide buffer shows up here and nowhere
	// else, so this is what separates it from the top-k tail.
	pub peak: usize,
}

// Bytes handed to the CALLING thread while `f` ran.
pub fn measure<R>(f: impl FnOnce() -> R) -> (R, Allocated) {
	let before = BYTES.with(|b| b.get());
	PEAK.with(|p| p.set(0));
	let r = f();
	let a = Allocated {
		total: BYTES.with(|b| b.get()) - before,
		peak: PEAK.with(|p| p.get()),
	};
	(r, a)
}
