//! Analyzes a folder and prints its largest subfolders, how long it took and
//! the memory the process holds: `cargo run --release -p okapi-tools
//! --features disk-usage --example disk_usage -- <folder>`. A development
//! tool: it reads, it never removes.
#![expect(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "a development tool prints and fails by panicking"
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use okapi_tools::disk_usage::{NodeId, UsageScan, default_threads};

/// How many subfolders are printed.
const SHOWN: usize = 10;

fn main() {
    let root: PathBuf = std::env::args_os()
        .nth(1)
        .map_or_else(|| std::env::var_os("HOME").unwrap().into(), PathBuf::from);
    let started = Instant::now();
    let scan = UsageScan::start(&root, default_threads()).unwrap();
    let outcome = loop {
        if let Some(outcome) = scan.wait(Duration::from_secs(1)) {
            break outcome;
        }
        let progress = scan.progress();
        println!("… {} files, {} bytes", progress.files, progress.allocated);
    };
    let view = scan.view(NodeId::ROOT).unwrap();
    println!(
        "{outcome:?} in {:?}: {} directories, {} files, {} bytes allocated, partial {}",
        started.elapsed(),
        scan.progress().directories,
        view.directory.total.files,
        view.directory.total.allocated,
        view.directory.partial
    );
    let mut children = view.children;
    children.sort_by_key(|child| std::cmp::Reverse(child.total.allocated));
    for child in children.iter().take(SHOWN) {
        println!(
            "{:>16}  {} {:?}",
            child.total.allocated,
            child.name.display(),
            child.state
        );
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status
        .lines()
        .filter(|line| line.starts_with("VmHWM") || line.starts_with("VmRSS"))
    {
        println!("{line}");
    }
}
