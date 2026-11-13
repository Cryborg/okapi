//! How Okapi reaches the administrator: through its privileged helper, which
//! pkexec runs when the polkit policy installed with it allows.

/// Where `okapi-helper` is installed: the exact path pkexec is given, which
/// its polkit policy names (`packaging/linux/`), never through a link.
pub const HELPER_PATH: &str = "/usr/libexec/okapi/okapi-helper";

/// pkexec, by its absolute path: never found through `PATH`, which the
/// session could point elsewhere.
pub const PKEXEC_PATH: &str = "/usr/bin/pkexec";
