//! How an application asked to close is waited for: a browser before its
//! caches are cleaned, an application before it is uninstalled.

use std::time::Duration;

/// How often an application asked to close is looked at again.
pub(crate) const CLOSE_LOOK_EVERY: Duration = Duration::from_millis(200);
/// How many looks it has to close: time for it to save its session.
pub(crate) const CLOSE_LOOKS: u32 = 75;
