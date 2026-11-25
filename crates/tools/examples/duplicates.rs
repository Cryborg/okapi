//! Searches folders for copies and prints the largest groups, how long it
//! took and the memory the process holds: `cargo run --release -p
//! okapi-tools --features duplicates --example duplicates -- <folder>...`
//! (the home folder without one). A development tool: it reads, it never
//! removes. One thread reads unless `THREADS` says otherwise.
#![expect(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "a development tool prints and fails by panicking"
)]

use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use okapi_engine::Exclusions;
use okapi_tools::duplicates::{DuplicateSearch, MIN_SIZE, SearchOptions};

/// How many groups are printed.
const SHOWN: usize = 10;

fn main() {
    let mut roots: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        roots.push(std::env::var_os("HOME").unwrap().into());
    }
    let threads = std::env::var("THREADS")
        .ok()
        .and_then(|threads| threads.parse().ok())
        .unwrap_or(NonZeroUsize::MIN);
    let started = Instant::now();
    let search = DuplicateSearch::start(
        &roots,
        SearchOptions {
            min_size: MIN_SIZE,
            threads,
            exclusions: Exclusions::none(),
        },
    )
    .unwrap();
    let outcome = loop {
        if let Some(outcome) = search.wait(Duration::from_secs(1)) {
            break outcome;
        }
        println!(
            "… {:?}, {} files listed",
            search.progress(),
            search.files_listed()
        );
    };
    let report = search.report().unwrap();
    println!(
        "{outcome:?} in {:?}: {} groups, {} bytes to gain, {:?}, {:?}",
        started.elapsed(),
        report.groups().len(),
        report.gain(),
        search.progress(),
        report.stats()
    );
    for group in report.groups().iter().take(SHOWN) {
        println!("{:>14} × {}", group.size(), group.copies().len());
        for copy in group.copies() {
            println!("    {}", copy.path().display());
        }
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status
        .lines()
        .filter(|line| line.starts_with("VmHWM") || line.starts_with("VmRSS"))
    {
        println!("{line}");
    }
}
