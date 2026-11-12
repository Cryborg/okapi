use std::fmt;

/// An operating system that has its own set of rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Os {
    Linux,
    MacOs,
    Windows,
}

impl Os {
    /// Every operating system with a rule set.
    pub const ALL: [Self; 3] = [Self::Linux, Self::MacOs, Self::Windows];

    /// The operating system this program was compiled for, if it has a rule set.
    #[must_use]
    pub const fn current() -> Option<Self> {
        if cfg!(target_os = "linux") {
            Some(Self::Linux)
        } else if cfg!(target_os = "macos") {
            Some(Self::MacOs)
        } else if cfg!(target_os = "windows") {
            Some(Self::Windows)
        } else {
            None
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Linux => "Linux",
            Self::MacOs => "macOS",
            Self::Windows => "Windows",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_names_the_system_being_built_for() {
        let expected = if cfg!(target_os = "linux") {
            Some(Os::Linux)
        } else if cfg!(target_os = "macos") {
            Some(Os::MacOs)
        } else if cfg!(target_os = "windows") {
            Some(Os::Windows)
        } else {
            None
        };
        assert_eq!(Os::current(), expected);
    }
}
