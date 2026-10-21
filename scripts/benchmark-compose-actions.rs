//! Measure production document export, block formatting, selection formatting and replay.
//! rustc --edition=2024 -O scripts/benchmark-compose-actions.rs -o /tmp/compose-actions
//! /tmp/compose-actions export uniform 1048576 10
//! For a matched source baseline, set COMPOSE_DOCUMENT_SOURCE to its absolute
//! path and build with --cfg benchmark_baseline. One condition per process.
//! Requested-live heap bytes exclude allocator overhead, layout, painting and
//! app RSS/PSS. Fixture construction and entered text are outside the action.

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
struct Counter;
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
#[global_allocator]
static ALLOCATOR: Counter = Counter;
fn grow(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::Relaxed) + bytes;
    PEAK.fetch_max(live, Ordering::Relaxed);
}
unsafe impl GlobalAlloc for Counter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            grow(layout.size());
            CALLS.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            grow(layout.size());
            CALLS.fetch_add(1, Ordering::Relaxed);
        }
        pointer
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe { System.realloc(pointer, layout, size) };
        if !next.is_null() {
            if size >= layout.size() {
                grow(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Ordering::Relaxed);
            }
            CALLS.fetch_add(1, Ordering::Relaxed);
        }
        next
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}
fn fingerprint(value: &str) -> u64 {
    value.bytes().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
    })
}
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    assert_eq!(args.len(), 5, "expected action scenario bytes samples");
    let action = args[1].as_str();
    let scenario = args[2].as_str();
    assert!(matches!(
        action,
        "export" | "block" | "list" | "format" | "undo" | "redo" | "replace"
    ));
    assert!(matches!(scenario, "uniform" | "fragmented" | "linked"));
    let bytes: usize = args[3].parse().expect("draft bytes");
    let samples: usize = args[4].parse().expect("samples");
    assert!((1024..=4 * 1024 * 1024).contains(&bytes));
    assert!((1..=100).contains(&samples));
    let base = if action == "list" {
        format!("• {}", "x".repeat(bytes - 4))
    } else {
        "x".repeat(bytes)
    };
    let entered = format!("{base}\n");
    println!(
        "{{\"schema_version\":1,\"scope\":\"document_requested_heap\",\"action\":\"{action}\",\"scenario\":\"{scenario}\",\"draft_bytes\":{bytes},\"warmup_actions\":1,\"samples\":["
    );
    for index in 0..=samples {
        let mut document = RichComposeDocument::default();
        let end = base.len() as i32;
        document.synchronize(&base, end, end);
        match scenario {
            "fragmented" => {
                for mark in 0..32 {
                    let start = (mark * base.len() / 64) as i32;
                    document.format("bold", &base, start, start + 1);
                }
            }
            "linked" => {
                document.set_link("https://example.org/?a=1&b=2", &base, 0, end);
            }
            _ => {}
        }
        if matches!(action, "undo" | "redo") {
            document.format("italic", &base, end / 2, end / 2 + 1);
            if action == "redo" {
                document.history("undo").expect("prepared undo");
            }
        }
        let before = LIVE.load(Ordering::Relaxed);
        PEAK.store(before, Ordering::Relaxed);
        CALLS.store(0, Ordering::Relaxed);
        let started = Instant::now();
        let output = match action {
            "export" => document.body_html(),
            "format" => {
                document.format("italic", &base, end / 2, end / 2 + 1);
                None
            }
            "undo" | "redo" => {
                document.history(action).expect("prepared history action");
                None
            }
            "replace" => {
                document.synchronize("y", 1, 1);
                None
            }
            "block" => {
                document.format("bullet", &base, 0, end);
                None
            }
            "list" => {
                let end = entered.len() as i32;
                document.synchronize(&entered, end, end);
                None
            }
            _ => unreachable!(),
        };
        let elapsed_us = started.elapsed().as_micros();
        let peak_extra = PEAK.load(Ordering::Relaxed).saturating_sub(before);
        let settled_extra = LIVE.load(Ordering::Relaxed) as i128 - before as i128;
        let allocation_calls = CALLS.load(Ordering::Relaxed);
        let rendered = output.unwrap_or_else(|| document.body_html().unwrap());
        if index > 0 {
            if index > 1 {
                println!(",");
            }
            print!(
                "{{\"elapsed_us\":{elapsed_us},\"peak_extra_bytes\":{peak_extra},\"settled_extra_bytes\":{settled_extra},\"allocation_calls\":{allocation_calls},\"html_bytes\":{},\"html_fingerprint\":{},\"selection\":[{},{}]}}",
                rendered.len(),
                fingerprint(&rendered),
                document.selection().start,
                document.selection().end
            );
        }
        std::hint::black_box((&document, &rendered));
    }
    println!("\n]}}");
}
