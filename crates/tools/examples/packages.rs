//! Asks PackageKit and snapd what waits for an update, and prints it with
//! how long each took: `cargo run -p okapi-tools --features packages
//! --example packages`. A development tool: it reads, it installs nothing.
#![expect(
    clippy::print_stdout,
    clippy::unwrap_used,
    reason = "a development tool prints and fails by panicking"
)]

use std::path::Path;
use std::time::Instant;

use okapi_tools::packages::packagekit::{PackageId, PackageKit};
use okapi_tools::packages::reboot::{DIRECTORY, reboot_required};
use okapi_tools::packages::snapd::Snapd;

fn main() {
    let started = Instant::now();
    let waiting = |waiting| println!("PackageKit waits for another program: {waiting}");
    match PackageKit::connect("fr_FR.UTF-8").and_then(|daemon| daemon.get_updates(&waiting)) {
        Ok(packages) => {
            println!(
                "PackageKit: {} updates in {:?}",
                packages.len(),
                started.elapsed()
            );
            for package in &packages {
                let id = PackageId::parse(&package.id).unwrap();
                println!(
                    "  [{}] {} {} ({}): {}",
                    package.info, id.name, id.version, id.data, package.summary
                );
            }
        }
        Err(error) => println!("PackageKit: {error} ({error:?})"),
    }
    let started = Instant::now();
    let snapd = Snapd::system();
    match snapd.refreshes() {
        Ok(refreshes) => {
            println!(
                "snapd: {} refreshes in {:?}",
                refreshes.len(),
                started.elapsed()
            );
            for refresh in &refreshes {
                println!("  {refresh:?}");
            }
        }
        Err(error) => println!("snapd: {error}"),
    }
    println!("snapd schedule: {:?}", snapd.schedule());
    println!("restart: {:?}", reboot_required(Path::new(DIRECTORY)));
}
