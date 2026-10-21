//! Standalone composer edit measurements. Build with:
//! rustc --edition=2024 -O scripts/benchmark-compose-document.rs -o /tmp/benchmark-compose-document
//! /tmp/benchmark-compose-document 40 > compose-document-raw.csv
//! To compare an older revision that lacks `synchronize_key_edit`:
//! COMPOSE_DOCUMENT_SOURCE=/absolute/path/to/rich_compose.rs \
//!   rustc --edition=2024 -O --cfg benchmark_baseline \
//!   scripts/benchmark-compose-document.rs -o /tmp/benchmark-compose-baseline
//! /tmp/benchmark-compose-baseline 40 uniform > baseline-uniform.csv
//!
//! This samples requested heap bytes in this process, not allocator overhead,
//! process RSS/PSS, Slint input, layout, or painting. Fixture construction and
//! input-string creation occur outside each timed/allocation-counted edit.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    sync::atomic::{AtomicUsize, Ordering},
    time::Instant,
};

#[cfg(benchmark_baseline)]
#[allow(dead_code)]
mod rich_compose {
    include!(env!("COMPOSE_DOCUMENT_SOURCE"));
}
#[cfg(not(benchmark_baseline))]
#[allow(dead_code)]
#[path = "../src/rich_compose.rs"]
mod rich_compose;
use rich_compose::RichComposeDocument;

struct CountingAllocator;

static LIVE_BYTES: AtomicUsize = AtomicUsize::new(0);
static PEAK_BYTES: AtomicUsize = AtomicUsize::new(0);
static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static ALLOCATED_BYTES: AtomicUsize = AtomicUsize::new(0);

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocated(bytes: usize) {
    let live = LIVE_BYTES.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK_BYTES.fetch_max(live, Ordering::Relaxed);
    ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
    ALLOCATED_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, old, new_size) };
        if !replacement.is_null() {
            if new_size >= old.size() {
                allocated(new_size - old.size());
            } else {
                LIVE_BYTES.fetch_sub(old.size() - new_size, Ordering::Relaxed);
                ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            }
        }
        replacement
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE_BYTES.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
enum Scenario {
    Uniform,
    ManyStyles,
    LinkedPassage,
}

impl Scenario {
    fn name(self) -> &'static str {
        match self {
            Self::Uniform => "uniform",
            Self::ManyStyles => "many_styles",
            Self::LinkedPassage => "linked_passage",
        }
    }
}

#[derive(Clone, Copy)]
enum Edit {
    TypeCharacter,
    PasteKilobyte,
}

impl Edit {
    fn name(self) -> &'static str {
        match self {
            Self::TypeCharacter => "type_character",
            Self::PasteKilobyte => "paste_1kib",
        }
    }
}

fn prepare_document(base: &str, scenario: Scenario) -> RichComposeDocument {
    let mut document = RichComposeDocument::default();
    let end = base.len() as i32;
    document.synchronize(base, end, end);
    match scenario {
        Scenario::Uniform => {}
        Scenario::ManyStyles => {
            // Thirty-two isolated marks create 64 alternating runs.
            for index in 0..32 {
                let start = (index * base.len() / 64) as i32;
                document.format("bold", base, start, start + 1);
            }
        }
        Scenario::LinkedPassage => {
            document.set_link("example.org/benchmark", base, 0, end);
        }
    }
    document.update_selection(end, end);
    document
}

fn sample(
    base: &str,
    target: &str,
    scenario: Scenario,
    edit: Edit,
) -> (u128, usize, i64, usize, usize, Option<usize>, usize) {
    let mut document = prepare_document(base, scenario);
    let start_live = LIVE_BYTES.load(Ordering::Relaxed);
    PEAK_BYTES.store(start_live, Ordering::Relaxed);
    ALLOC_CALLS.store(0, Ordering::Relaxed);
    ALLOCATED_BYTES.store(0, Ordering::Relaxed);

    let start = Instant::now();
    let end = target.len() as i32;
    match edit {
        Edit::TypeCharacter => {
            #[cfg(not(benchmark_baseline))]
            document.synchronize_key_edit(target, end, end, "y");
            #[cfg(benchmark_baseline)]
            document.synchronize(target, end, end);
        }
        Edit::PasteKilobyte => {
            document.synchronize(target, end, end);
        }
    }
    let elapsed = start.elapsed().as_nanos();
    let peak_extra = PEAK_BYTES
        .load(Ordering::Relaxed)
        .saturating_sub(start_live);
    let net_live = LIVE_BYTES.load(Ordering::Relaxed) as i64 - start_live as i64;
    let allocations = ALLOC_CALLS.load(Ordering::Relaxed);
    let allocated_bytes = ALLOCATED_BYTES.load(Ordering::Relaxed);
    #[cfg(not(benchmark_baseline))]
    let owned = Some(document.memory_usage().total_bytes);
    #[cfg(benchmark_baseline)]
    let owned: Option<usize> = None;
    let run_count = document.style_runs().len();
    (
        elapsed,
        peak_extra,
        net_live,
        allocations,
        allocated_bytes,
        owned,
        run_count,
    )
}

fn main() {
    let mut arguments = std::env::args().skip(1);
    let samples = arguments
        .next()
        .map(|value| {
            value
                .parse::<usize>()
                .expect("samples must be a positive integer")
        })
        .unwrap_or(40);
    assert!(samples > 0);
    let scenario_filter = arguments.next();
    let size_filter = arguments.next().map(|value| {
        value
            .parse::<usize>()
            .expect("draft size must be an integer")
    });
    println!(
        "model,scenario,draft_bytes,edit,sample,elapsed_ns,peak_extra_bytes,net_live_bytes,alloc_calls,allocated_bytes,document_owned_bytes,style_runs"
    );
    for size in [10 * 1024, 100 * 1024, 1024 * 1024] {
        if size_filter.is_some_and(|filter| filter != size) {
            continue;
        }
        let base = "x".repeat(size);
        for scenario in [
            Scenario::Uniform,
            Scenario::ManyStyles,
            Scenario::LinkedPassage,
        ] {
            if scenario_filter
                .as_deref()
                .is_some_and(|filter| filter != scenario.name())
            {
                continue;
            }
            for edit in [Edit::TypeCharacter, Edit::PasteKilobyte] {
                let mut target = base.clone();
                match edit {
                    Edit::TypeCharacter => target.push('y'),
                    Edit::PasteKilobyte => target.push_str(&"p".repeat(1024)),
                }
                // Warm the same code path without including setup in the sample.
                for _ in 0..3 {
                    let _ = sample(&base, &target, scenario, edit);
                }
                for index in 0..samples {
                    let (elapsed, peak, net, calls, allocated, owned, runs) =
                        sample(&base, &target, scenario, edit);
                    println!(
                        "{},{},{},{},{},{},{},{},{},{},{},{}",
                        if cfg!(benchmark_baseline) {
                            "baseline"
                        } else {
                            "current"
                        },
                        scenario.name(),
                        size,
                        edit.name(),
                        index,
                        elapsed,
                        peak,
                        net,
                        calls,
                        allocated,
                        owned.map_or(String::new(), |bytes| bytes.to_string()),
                        runs
                    );
                }
            }
        }
    }
}
