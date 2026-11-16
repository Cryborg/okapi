//! A leftover walked under a low limit of open files, as a desktop session
//! sets it (1 024): alone in its process, the limit lowered touches no other
//! test.

#![cfg(all(feature = "apps", target_os = "linux"))]

use std::collections::BTreeSet;
use std::fs;

use okapi_engine::{Exclusions, ProtectedRoots};
use okapi_rules::TemplateVariables;
use okapi_tools::apps::leftovers::{self, Installed, Known, MOST_ENTRIES, Places};
use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

/// The open files this test allows itself.
const OPEN_FILES: u64 = 64;
/// More folders than that, side by side.
const FOLDERS: usize = 300;

#[test]
fn a_leftover_of_many_folders_is_walked_with_few_descriptors() {
    // Catches a descriptor held per folder waiting: a browser's profile,
    // one folder per site, would pass the limit and its leftover never be
    // proposed.
    let root = tempfile::tempdir().unwrap();
    let home = fs::canonicalize(root.path()).unwrap().join("home");
    let profile = home.join(".var/app/org.mozilla.firefox");
    for site in 0..FOLDERS {
        fs::create_dir_all(profile.join(format!("site{site}"))).unwrap();
    }
    let limit = getrlimit(Resource::Nofile);
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(OPEN_FILES),
            maximum: limit.maximum,
        },
    )
    .unwrap();
    let none = || Known::Read(BTreeSet::new());
    let report = leftovers::find(
        &Places {
            home,
            user: okapi_platform::effective_user().unwrap(),
            most_entries: MOST_ENTRIES,
        },
        &Installed {
            snaps: none(),
            flatpaks: none(),
        },
        &ProtectedRoots::of(&TemplateVariables::new()).unwrap(),
        &Exclusions::none(),
    );
    setrlimit(Resource::Nofile, limit).unwrap();
    assert_eq!(report.leftovers().len(), 1, "{report:?}");
    assert!(report.leftovers()[0].checkable());
}
