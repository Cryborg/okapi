//! Searches folders for large files and prints how many there are at each
//! floor the screen offers and of each kind, the largest, and how long it
//! took: `cargo run --release -p okapi-tools --features large-files
//! --example large_files -- <folder>...` (the home folder without one). A
//! development tool: it reads, it never removes.
#![expect(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "a development tool prints and fails by panicking"
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use okapi_engine::Exclusions;
use okapi_tools::large_files::{FLOORS, FileKind, LargeFileSearch, MIN_SIZE, SearchOptions};

/// How many files are printed.
const SHOWN: usize = 10;

fn main() {
    let mut roots: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if roots.is_empty() {
        roots.push(std::env::var_os("HOME").unwrap().into());
    }
    let started = Instant::now();
    let search = LargeFileSearch::start(
        &roots,
        SearchOptions {
            min_size: MIN_SIZE,
            exclusions: Exclusions::none(),
        },
    )
    .unwrap();
    let outcome = loop {
        if let Some(outcome) = search.wait(Duration::from_secs(1)) {
            break outcome;
        }
        println!("… {:?}", search.progress());
    };
    let report = search.report().unwrap();
    println!(
        "{outcome:?} in {:?}: {} files, {} bytes on disk, {:?}",
        started.elapsed(),
        report.files().len(),
        report.total(),
        report.stats()
    );
    for floor in FLOORS {
        let above = report
            .files()
            .iter()
            .filter(|file| file.on_disk() >= floor)
            .count();
        println!("{floor:>14} and more: {above}");
    }
    for kind in FileKind::ALL {
        let of_kind = report
            .files()
            .iter()
            .filter(|file| file.kind() == kind)
            .count();
        println!("{:>14}: {of_kind}", kind.key());
    }
    for file in report.files().iter().take(SHOWN) {
        println!("{:>14} {}", file.on_disk(), file.path().display());
    }
}
