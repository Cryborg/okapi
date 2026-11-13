use std::time::Duration;

/// What the system says of itself; each part `None` (or empty) when it
/// cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SystemReading {
    /// `Ubuntu 26.04.1 LTS`.
    pub distribution: Option<String>,
    /// `7.0.0-42-generic`.
    pub kernel: Option<String>,
    /// The desktops the session says it runs, the most specific first
    /// (`ubuntu`, `GNOME`).
    pub desktops: Vec<String>,
    /// `wayland` or `x11`.
    pub session: Option<String>,
    pub uptime: Option<Duration>,
}
