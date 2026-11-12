use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use okapi_platform::FileIdentity;
use okapi_rules::{
    AgeDays, AppCacheRule, CleanTarget, Effect, MiscRules, PathMatcher, RelativePath,
    SingleFileTarget, TemplatePath, TemplateVariables,
};

use crate::options::ScanOptions;
use crate::paths::is_absent;
use crate::protection::ProtectedRoots;
use crate::report::{
    AppFile, Category, EntryKind, GroupAttributes, Origin, ScanIssue, ScanStats, UnsafeReason,
};
use crate::walk::{Found, Readers, by_name};

/// A rule to scan, from any of the rule files the scan handles.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RuleEntry<'a> {
    Target(&'a CleanTarget),
    File(&'a SingleFileTarget),
    App {
        file: AppFile,
        rule: &'a AppCacheRule,
    },
}

impl RuleEntry<'_> {
    pub(crate) const fn category(&self) -> Category {
        match self {
            Self::Target(_) | Self::File(_) => Category::System,
            Self::App { file, .. } => Category::of_file(*file),
        }
    }

    /// Whether the rule is selected before the user chooses (invariant 5).
    pub(crate) const fn is_selected_by_default(&self) -> bool {
        match self {
            Self::Target(target) => target.is_selected_by_default(),
            Self::File(target) => target.is_selected_by_default(),
            Self::App { rule, .. } => rule.is_selected_by_default(),
        }
    }
}

/// What a planned root is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlannedKind {
    /// A directory whose contents are walked.
    Directory,
    /// A single file, already found offerable.
    File(Found),
    /// A directory a rule reaches only across a link, another device or a
    /// mount, or a file target that is a bind mount: never walked nor offered
    /// for that rule, it still joins the group of the rules that share it, so
    /// that their AND (invariant 5) and strictest age hold for whoever walks
    /// or offers it. Met below another rule's root, it is reserved as any root
    /// is: then nobody offers it, rather than that rule without this one's
    /// restrictions.
    Withheld,
}

/// A root an expansion reached, walked or withheld.
struct Candidate {
    path: PathBuf,
    withheld: bool,
}

impl Candidate {
    fn walked(path: PathBuf) -> Self {
        Self {
            path,
            withheld: false,
        }
    }
}

/// What a segment of an expansion is.
enum Segment {
    /// A plain directory on the base's filesystem: the expansion goes on.
    Plain,
    /// A link, another device or a mount: never entered.
    Refused,
    /// Absent, not a directory, or unreadable (reported).
    Missing,
}

/// A canonical root, checked, with the group it fills.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedRoot {
    pub(crate) path: PathBuf,
    pub(crate) identity: FileIdentity,
    pub(crate) cutoff: SystemTime,
    pub(crate) attributes: GroupAttributes,
    pub(crate) kind: PlannedKind,
}

/// Turns rules into checked roots, recording why a root is not walked.
pub(crate) struct Planner<'a> {
    pub(crate) variables: &'a TemplateVariables,
    pub(crate) options: &'a ScanOptions,
    pub(crate) misc: &'a MiscRules,
    pub(crate) protected: &'a ProtectedRoots,
    pub(crate) readers: Readers,
    pub(crate) issues: Vec<ScanIssue>,
    pub(crate) stats: ScanStats,
}

impl Planner<'_> {
    pub(crate) fn plan(&mut self, entry: RuleEntry<'_>) -> Vec<PlannedRoot> {
        match entry {
            RuleEntry::Target(target) => self.plan_target(target),
            RuleEntry::File(target) => self.plan_file(target).into_iter().collect(),
            RuleEntry::App { file, rule } => self.plan_app(file, rule),
        }
    }

    fn plan_target(&mut self, target: &CleanTarget) -> Vec<PlannedRoot> {
        let Effect::Delete(deletion) = target.effect() else {
            return Vec::new();
        };
        let origin = Origin::SystemTarget {
            subcategory: target.subcategory().clone(),
            path: deletion.path().clone(),
        };
        let attributes = GroupAttributes {
            origins: vec![origin.clone()],
            label: target.subcategory().clone(),
            rule_group: None,
            cache_reset: deletion.is_cache_reset(),
            selected_by_default: target.is_selected_by_default(),
            needs_admin: target.needs_admin(),
        };
        let Some(base) = self.open_base(&origin, deletion.path()) else {
            return Vec::new();
        };
        let roots = match deletion.child_subdirectory() {
            Some(subdirectory) => self.expand(&origin, &base, subdirectory),
            None => vec![Candidate::walked(base)],
        };
        let rule_age = deletion.min_age().map(AgeDays::duration);
        self.check_roots(&origin, roots, rule_age, &attributes)
    }

    fn plan_app(&mut self, file: AppFile, rule: &AppCacheRule) -> Vec<PlannedRoot> {
        let Effect::Delete(deletion) = rule.effect() else {
            return Vec::new();
        };
        let origin = Origin::App {
            file,
            id: rule.id().clone(),
        };
        let subdirectory = match deletion.matcher() {
            PathMatcher::WholeDirectory => None,
            PathMatcher::ChildSubdirectory(subdirectory) => Some(subdirectory),
            PathMatcher::Recursive(_) | PathMatcher::DirectFiles(_) => {
                self.issues.push(ScanIssue::Unsupported { origin });
                return Vec::new();
            }
        };
        let attributes = GroupAttributes {
            origins: vec![origin.clone()],
            label: rule.name().clone(),
            rule_group: rule.group().cloned(),
            cache_reset: deletion.is_cache_reset(),
            selected_by_default: rule.is_selected_by_default(),
            needs_admin: false,
        };
        let mut roots = Vec::new();
        for path in deletion.paths() {
            let Some(base) = self.open_base(&origin, path) else {
                continue;
            };
            match subdirectory {
                Some(subdirectory) => roots.extend(self.expand(&origin, &base, subdirectory)),
                None => roots.push(Candidate::walked(base)),
            }
        }
        let rule_age = deletion.min_age().map(AgeDays::duration);
        self.check_roots(&origin, roots, rule_age, &attributes)
    }

    fn plan_file(&mut self, target: &SingleFileTarget) -> Option<PlannedRoot> {
        let origin = Origin::SystemFile {
            subcategory: target.subcategory().clone(),
            path: target.path().clone(),
        };
        let resolved = self.resolve(&origin, target.path())?;
        let metadata = match fs::symlink_metadata(&resolved) {
            Ok(metadata) => metadata,
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(&origin, resolved, error.kind()),
        };
        if !metadata.file_type().is_file() {
            return self.unsafe_root(&origin, resolved, &metadata, UnsafeReason::NotAFile);
        }
        let canonical = match fs::canonicalize(&resolved) {
            Ok(canonical) => canonical,
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(&origin, resolved, error.kind()),
        };
        let metadata = match fs::symlink_metadata(&canonical) {
            Ok(again) if again.file_type().is_file() => match self.same_object(&metadata, &again) {
                Some(true) => again,
                Some(false) => return self.replaced(&origin, canonical),
                None => return self.unreadable(&origin, canonical, io::ErrorKind::Unsupported),
            },
            Ok(_) => return self.replaced(&origin, canonical),
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(&origin, canonical, error.kind()),
        };
        let mount_root = match (self.readers.mount_root)(&canonical) {
            Ok(mount_root) => mount_root == Some(true),
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(&origin, canonical, error.kind()),
        };
        let (Some(last_change), Some(identity)) = (
            (self.readers.last_change)(&metadata),
            (self.readers.identity)(&metadata),
        ) else {
            return self.unreadable(&origin, canonical, io::ErrorKind::Unsupported);
        };
        if let Some(variable) = self.protected.refusal(identity) {
            self.issues.push(ScanIssue::ProtectedRoot {
                origins: vec![origin],
                path: canonical,
                variable,
            });
            return None;
        }
        if self.options.exclusions().excludes(&canonical) {
            self.issues.push(ScanIssue::Excluded {
                origins: vec![origin],
                path: canonical,
            });
            return None;
        }
        if by_name(&canonical, false, true).is_some() || self.misc.protects(&canonical) {
            tracing::debug!(path = %canonical.display(), "file target has a protected name");
            self.stats.protected_names += 1;
            return None;
        }
        // The global age was checked before planning: it is within the clock.
        let cutoff = self.options.cutoff(None)?;
        let attributes = GroupAttributes {
            origins: vec![origin],
            label: target.subcategory().clone(),
            rule_group: None,
            cache_reset: false,
            selected_by_default: target.is_selected_by_default(),
            needs_admin: false,
        };
        // A bind mount of a file: never offered by this rule, it still joins
        // the rules that share it (invariant 5, strictest age).
        if mount_root {
            tracing::trace!(path = %canonical.display(), "file target is a mount root");
            self.stats.other_filesystems += 1;
            return Some(PlannedRoot {
                path: canonical,
                identity,
                cutoff,
                attributes,
                kind: PlannedKind::Withheld,
            });
        }
        if last_change > cutoff {
            tracing::trace!(path = %canonical.display(), "file target recent");
            self.stats.recent += 1;
            return None;
        }
        Some(PlannedRoot {
            path: canonical.clone(),
            identity,
            cutoff,
            attributes,
            kind: PlannedKind::File(Found {
                path: canonical,
                size: metadata.len(),
                last_change,
                kind: EntryKind::File,
                identity,
            }),
        })
    }

    fn resolve(&mut self, origin: &Origin, path: &TemplatePath) -> Option<PathBuf> {
        match path.resolve(self.variables) {
            Ok(resolved) => Some(resolved),
            Err(error) => {
                self.issues.push(ScanIssue::Unresolvable {
                    origin: origin.clone(),
                    error,
                });
                None
            }
        }
    }

    /// The canonical directory a rule path names, if it exists and is a real
    /// directory: the last component is never a link (an ancestor may be). It
    /// may be a mount (`/tmp`), so that is not read here.
    fn open_base(&mut self, origin: &Origin, path: &TemplatePath) -> Option<PathBuf> {
        let resolved = self.resolve(origin, path)?;
        let metadata = match fs::symlink_metadata(&resolved) {
            Ok(metadata) => metadata,
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(origin, resolved, error.kind()),
        };
        if !metadata.is_dir() {
            return self.unsafe_root(origin, resolved, &metadata, UnsafeReason::NotADirectory);
        }
        let canonical = match fs::canonicalize(&resolved) {
            Ok(canonical) => canonical,
            Err(error) if is_absent(&error) => return None,
            Err(error) => return self.unreadable(origin, resolved, error.kind()),
        };
        // Read again under its canonical name: it must still be the directory
        // read first, not a link swapped in since.
        match fs::symlink_metadata(&canonical) {
            Ok(again) if again.is_dir() => match self.same_object(&metadata, &again) {
                Some(true) => Some(canonical),
                Some(false) => self.replaced(origin, canonical),
                None => self.unreadable(origin, canonical, io::ErrorKind::Unsupported),
            },
            Ok(_) => self.replaced(origin, canonical),
            Err(error) if is_absent(&error) => None,
            Err(error) => self.unreadable(origin, canonical, error.kind()),
        }
    }

    /// The directories `<base>/*/<subdirectory>`: walked when reached
    /// without following any link nor leaving the base's filesystem, withheld
    /// otherwise.
    fn expand(
        &mut self,
        origin: &Origin,
        base: &Path,
        subdirectory: &RelativePath,
    ) -> Vec<Candidate> {
        let device = match fs::symlink_metadata(base) {
            Ok(metadata) => (self.readers.identity)(&metadata).map(FileIdentity::device),
            Err(error) if is_absent(&error) => return Vec::new(),
            Err(error) => {
                self.unreadable::<()>(origin, base.to_path_buf(), error.kind());
                return Vec::new();
            }
        };
        let entries = match fs::read_dir(base) {
            Ok(entries) => entries,
            Err(error) if is_absent(&error) => return Vec::new(),
            Err(error) => {
                self.unreadable::<()>(origin, base.to_path_buf(), error.kind());
                return Vec::new();
            }
        };
        let mut roots = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::debug!(base = %base.display(), %error, "expanded entry unreadable");
                    self.stats.unreadable += 1;
                    continue;
                }
            };
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    tracing::debug!(path = %entry.path().display(), %error, "expanded entry unreadable");
                    self.stats.unreadable += 1;
                    continue;
                }
            };
            // A link is judged by `reach`, as any segment: withheld, it still
            // joins the rules that reach its target directly.
            if (file_type.is_dir() || file_type.is_symlink())
                && let Some(root) = self.reach(origin, entry.path(), subdirectory, device)
            {
                roots.push(root);
            }
        }
        roots
    }

    /// `<child>/<subdirectory>`: a candidate walked when the child and every
    /// segment is a plain directory on the base's device, withheld from the
    /// first one that is a link, another device or a mount
    /// ([`Self::segment`]; the segments after it are not judged), none when
    /// one before is missing.
    fn reach(
        &mut self,
        origin: &Origin,
        child: PathBuf,
        subdirectory: &RelativePath,
        device: Option<u64>,
    ) -> Option<Candidate> {
        // The child first, as segment zero: the listing only gave its type.
        let mut path = child;
        let mut withheld = false;
        for segment in std::iter::once(None).chain(subdirectory.segments().iter().map(Some)) {
            if let Some(segment) = segment {
                path.push(segment.as_str());
            }
            if !withheld {
                match self.segment(origin, &path, device) {
                    Segment::Plain => {}
                    Segment::Refused => withheld = true,
                    Segment::Missing => return None,
                }
            }
        }
        Some(Candidate { path, withheld })
    }

    /// What `path` is to an expansion below a base on `device`: the base
    /// itself is never judged so (it may be a mount, as `/tmp` is).
    fn segment(&mut self, origin: &Origin, path: &Path, device: Option<u64>) -> Segment {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if is_absent(&error) => return Segment::Missing,
            Err(error) => {
                self.unreadable::<()>(origin, path.to_path_buf(), error.kind());
                return Segment::Missing;
            }
        };
        if metadata.file_type().is_symlink() {
            tracing::trace!(path = %path.display(), "expanded segment is a link");
            self.stats.links += 1;
            return Segment::Refused;
        }
        if !metadata.is_dir() {
            return Segment::Missing;
        }
        // Another device is enough: its mount status is then never read.
        let mut other_filesystem =
            (self.readers.identity)(&metadata).map(FileIdentity::device) != device;
        if !other_filesystem {
            other_filesystem = match (self.readers.mount_root)(path) {
                Ok(mount_root) => mount_root == Some(true),
                Err(error) if is_absent(&error) => return Segment::Missing,
                Err(error) => {
                    self.unreadable::<()>(origin, path.to_path_buf(), error.kind());
                    return Segment::Missing;
                }
            };
        }
        if other_filesystem {
            tracing::trace!(path = %path.display(), "expanded segment on another filesystem");
            self.stats.other_filesystems += 1;
            return Segment::Refused;
        }
        Segment::Plain
    }

    /// The identity of a withheld root, read through links: it is never
    /// walked, only matched against the roots of other rules. `None` when it
    /// is gone or not a directory (no other directory root can share it), or
    /// when it cannot be read: then reported, since a rule that walks it
    /// directly would do so without this rule's restrictions.
    fn withheld_identity(&mut self, origin: &Origin, path: &Path) -> Option<FileIdentity> {
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => match (self.readers.identity)(&metadata) {
                Some(identity) => Some(identity),
                None => self.unreadable(origin, path.to_path_buf(), io::ErrorKind::Unsupported),
            },
            Ok(_) => None,
            Err(error) if is_absent(&error) => None,
            Err(error) => self.unreadable(origin, path.to_path_buf(), error.kind()),
        }
    }

    /// Keeps the roots a walk may enter: not a variable's directory nor one of
    /// its ancestors (invariant 6), not excluded. Not judged as a mount here:
    /// a rule's own path may be one (`/tmp`); an expanded root was judged by
    /// [`Self::reach`]. A rule age beyond the clock
    /// leaves nothing old enough; it is reported once the rule has a root.
    fn check_roots(
        &mut self,
        origin: &Origin,
        roots: Vec<Candidate>,
        rule_age: Option<Duration>,
        attributes: &GroupAttributes,
    ) -> Vec<PlannedRoot> {
        if roots.is_empty() {
            return Vec::new();
        }
        let Some(cutoff) = self.options.cutoff(rule_age) else {
            self.issues.push(ScanIssue::AgeBeyondClock {
                origin: origin.clone(),
            });
            return Vec::new();
        };
        let mut planned = Vec::new();
        for Candidate {
            path: root,
            withheld,
        } in roots
        {
            if withheld {
                if let Some(identity) = self.withheld_identity(origin, &root) {
                    planned.push(PlannedRoot {
                        path: root,
                        identity,
                        cutoff,
                        attributes: attributes.clone(),
                        kind: PlannedKind::Withheld,
                    });
                }
                continue;
            }
            let metadata = match fs::symlink_metadata(&root) {
                Ok(metadata) => metadata,
                Err(error) if is_absent(&error) => continue,
                Err(error) => {
                    self.unreadable::<()>(origin, root, error.kind());
                    continue;
                }
            };
            if !metadata.is_dir() {
                self.unsafe_root::<()>(origin, root, &metadata, UnsafeReason::NotADirectory);
                continue;
            }
            let Some(identity) = (self.readers.identity)(&metadata) else {
                self.unreadable::<()>(origin, root, io::ErrorKind::Unsupported);
                continue;
            };
            if let Some(variable) = self.protected.refusal(identity) {
                self.issues.push(ScanIssue::ProtectedRoot {
                    origins: vec![origin.clone()],
                    path: root,
                    variable,
                });
                continue;
            }
            let exclusions = self.options.exclusions();
            if exclusions.excludes(&root)
                || exclusions.excludes_by_identity(&root, self.readers.identity)
            {
                self.issues.push(ScanIssue::Excluded {
                    origins: vec![origin.clone()],
                    path: root,
                });
                continue;
            }
            planned.push(PlannedRoot {
                path: root,
                identity,
                cutoff,
                attributes: attributes.clone(),
                kind: PlannedKind::Directory,
            });
        }
        planned
    }

    /// Whether two reads saw the same object; `None` when an identity cannot
    /// be read, which says nothing either way.
    fn same_object(&self, first: &fs::Metadata, second: &fs::Metadata) -> Option<bool> {
        Some((self.readers.identity)(first)? == (self.readers.identity)(second)?)
    }

    fn replaced<T>(&mut self, origin: &Origin, path: PathBuf) -> Option<T> {
        self.issues.push(ScanIssue::UnsafeRoot {
            origins: vec![origin.clone()],
            path,
            reason: UnsafeReason::Replaced,
        });
        None
    }

    fn unreadable<T>(&mut self, origin: &Origin, path: PathBuf, error: io::ErrorKind) -> Option<T> {
        self.issues.push(ScanIssue::UnreadableRoot {
            origins: vec![origin.clone()],
            path,
            error,
        });
        None
    }

    fn unsafe_root<T>(
        &mut self,
        origin: &Origin,
        path: PathBuf,
        metadata: &fs::Metadata,
        expected: UnsafeReason,
    ) -> Option<T> {
        let reason = if metadata.file_type().is_symlink() {
            UnsafeReason::Link
        } else {
            expected
        };
        self.issues.push(ScanIssue::UnsafeRoot {
            origins: vec![origin.clone()],
            path,
            reason,
        });
        None
    }
}
