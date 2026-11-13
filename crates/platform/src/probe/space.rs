use std::io;
use std::path::Path;

/// The space of a filesystem, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Space {
    pub total: u64,
    /// What the user may still write (the space kept for root left out, as
    /// `df` shows it).
    pub available: u64,
    pub used: u64,
}

impl Space {
    /// The space of `blocks` fragments of `fragment` bytes, `free` of them
    /// free, `available` of those to the user.
    pub(crate) fn of(blocks: u64, free: u64, available: u64, fragment: u64) -> Self {
        Self {
            total: blocks.saturating_mul(fragment),
            available: available.saturating_mul(fragment),
            used: blocks.saturating_sub(free).saturating_mul(fragment),
        }
    }
}

/// The space of the filesystem mounted at `path`. May block for as long as a
/// network filesystem does not answer: call it off any thread that must go
/// on.
///
/// # Errors
///
/// The system's error; [`io::ErrorKind::Unsupported`] outside Linux.
#[cfg(target_os = "linux")]
pub fn space(path: &Path) -> io::Result<Space> {
    let status = rustix::fs::statvfs(path)?;
    Ok(Space::of(
        status.f_blocks,
        status.f_bfree,
        status.f_bavail,
        status.f_frsize,
    ))
}

/// The space of the filesystem mounted at `path`: unknown here.
///
/// # Errors
///
/// [`io::ErrorKind::Unsupported`].
#[cfg(not(target_os = "linux"))]
pub fn space(_path: &Path) -> io::Result<Space> {
    super::unsupported()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_space_left_to_the_user_excludes_the_space_kept_for_root() {
        assert_eq!(
            Space::of(1_000, 300, 250, 4_096),
            Space {
                total: 4_096_000,
                available: 1_024_000,
                used: 2_867_200,
            }
        );
        assert_eq!(Space::of(u64::MAX, 0, 0, 2).total, u64::MAX);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_space_of_the_root_holds_together() {
        let space = space(Path::new("/")).unwrap();
        assert!(space.total > 0);
        assert!(space.available <= space.total);
        assert!(space.used <= space.total);
    }
}
