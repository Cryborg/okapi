use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::PathBuf;

use okapi_platform::FileIdentity;
use okapi_rules::{TemplateVariables, Variable};

use crate::paths::{canonicalize_longest_prefix, is_absent};
use crate::walk::IdentityReader;

/// A variable's directory, or one of its ancestors, could not be read: what it
/// protects cannot be vouched for, so nothing is scanned.
#[derive(Debug)]
pub(crate) struct UnreadableVariable {
    pub(crate) variable: Variable,
    pub(crate) path: PathBuf,
    pub(crate) error: io::ErrorKind,
}

/// The directories of the template variables, which no scan root may equal or
/// contain, `${TMPDIR}`'s own directory excepted (invariant 6): emptying such a
/// root would empty `${HOME}`, `${CONFIG}` or a whole drive.
///
/// Roots are compared by identity, not by path. A root exists, so if a
/// variable lies below it, the root is one of the variable's ancestors, found
/// even when the value does not exist yet (through its longest existing
/// prefix); identity sees through case-insensitive spellings, Unicode
/// normalization and bind mounts, which a path comparison would miss.
#[derive(Debug)]
pub(crate) struct ProtectedRoots {
    /// The identity of each existing value and of all its ancestors, with the
    /// variable it protects. `${TMPDIR}`'s own directory is not in it.
    ancestors: BTreeMap<FileIdentity, Variable>,
    /// The identity of each existing value itself.
    directories: BTreeSet<FileIdentity>,
}

impl ProtectedRoots {
    pub(crate) fn new(
        variables: &TemplateVariables,
        identity_of: IdentityReader,
    ) -> Result<Self, UnreadableVariable> {
        let mut protected = Self {
            ancestors: BTreeMap::new(),
            directories: BTreeSet::new(),
        };
        for (variable, value) in variables.iter() {
            let unreadable = |path: PathBuf, error: &io::Error| UnreadableVariable {
                variable,
                path,
                error: error.kind(),
            };
            let canonical = canonicalize_longest_prefix(value)
                .map_err(|error| unreadable(value.to_path_buf(), &error))?;
            for (depth, ancestor) in canonical.ancestors().enumerate() {
                let metadata = match fs::metadata(ancestor) {
                    Ok(metadata) => metadata,
                    // The part of a value that does not exist yet.
                    Err(error) if is_absent(&error) => continue,
                    Err(error) => return Err(unreadable(ancestor.to_path_buf(), &error)),
                };
                let Some(identity) = identity_of(&metadata) else {
                    let error = io::Error::from(io::ErrorKind::Unsupported);
                    return Err(unreadable(ancestor.to_path_buf(), &error));
                };
                if depth == 0 {
                    protected.directories.insert(identity);
                }
                // `${TMPDIR}` is the one directory a rule may empty: its own
                // identity is allowed, its ancestors are not.
                if depth > 0 || variable != Variable::TempDirectory {
                    protected.ancestors.entry(identity).or_insert(variable);
                }
            }
        }
        Ok(protected)
    }

    /// The variable that forbids walking a root of this identity, if any: one
    /// whose directory is the root or lies below it. A root that is
    /// `${TMPDIR}` is allowed, unless it also holds another variable.
    pub(crate) fn refusal(&self, identity: FileIdentity) -> Option<Variable> {
        self.ancestors.get(&identity).copied()
    }

    /// Whether a directory met below a root is a variable's directory or one of
    /// their ancestors, reached through a bind mount: it is never entered.
    pub(crate) fn is_protected_directory(&self, identity: FileIdentity) -> bool {
        self.directories.contains(&identity) || self.ancestors.contains_key(&identity)
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use okapi_platform::file_identity;

    fn identity(path: &Path) -> FileIdentity {
        file_identity(&fs::metadata(path).unwrap()).unwrap()
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
    }

    fn fixture(directories: &[&str]) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        for relative in directories {
            fs::create_dir_all(root.join(relative)).unwrap();
        }
        Fixture {
            _directory: directory,
            root,
        }
    }

    fn protected(values: &[(Variable, PathBuf)]) -> ProtectedRoots {
        let mut variables = TemplateVariables::new();
        for (variable, value) in values {
            variables.insert(*variable, value.clone()).unwrap();
        }
        ProtectedRoots::new(&variables, file_identity).unwrap()
    }

    #[test]
    fn refuses_a_root_that_holds_a_variable() {
        let fixture = fixture(&["guest", "config/app"]);
        let root = &fixture.root;
        let protected = protected(&[
            (Variable::Home, root.join("guest")),
            (Variable::Config, root.join("config")),
        ]);
        // Both are below the root: which one is named does not matter.
        assert!(protected.refusal(identity(root)).is_some());
        let config = root.join("config");
        assert_eq!(protected.refusal(identity(&config)), Some(Variable::Config));
        let app = root.join("config/app");
        assert_eq!(protected.refusal(identity(&app)), None);
    }

    #[test]
    fn allows_the_temporary_directory_itself_but_not_its_ancestors() {
        let fixture = fixture(&["outer/temp"]);
        let temp = fixture.root.join("outer/temp");
        let protected = protected(&[(Variable::TempDirectory, temp.clone())]);
        assert_eq!(protected.refusal(identity(&temp)), None);
        let outer = fixture.root.join("outer");
        assert_eq!(
            protected.refusal(identity(&outer)),
            Some(Variable::TempDirectory)
        );
    }

    #[test]
    fn refuses_the_temporary_directory_when_it_holds_another_variable() {
        // Lexically apart, so `TemplateVariables::insert` accepts them; once
        // canonicalized, `${HOME}` is inside `${TMPDIR}`.
        let fixture = fixture(&["temp/guest"]);
        let temp = fixture.root.join("temp");
        std::os::unix::fs::symlink(&temp, fixture.root.join("alias")).unwrap();
        let protected = protected(&[
            (Variable::TempDirectory, temp.clone()),
            (Variable::Home, fixture.root.join("alias/guest")),
        ]);
        assert_eq!(protected.refusal(identity(&temp)), Some(Variable::Home));
    }

    #[test]
    fn protects_a_variable_that_does_not_exist_yet() {
        let fixture = fixture(&[]);
        let root = &fixture.root;
        let protected = protected(&[(Variable::Home, root.join("guest"))]);
        assert_eq!(protected.refusal(identity(root)), Some(Variable::Home));
    }

    #[test]
    fn sees_through_aliases_on_either_side() {
        use std::os::unix::fs::symlink;

        // A link in an ancestor of the root.
        let first = fixture(&["h/b/guest"]);
        let root = &first.root;
        symlink(root.join("h"), root.join("a")).unwrap();
        let protected_home = protected(&[(Variable::Home, root.join("h/b/guest"))]);
        let through_alias = fs::canonicalize(root.join("a/b")).unwrap();
        assert_eq!(
            protected_home.refusal(identity(&through_alias)),
            Some(Variable::Home)
        );

        // A link in the variable's value.
        let second = fixture(&["real/guest"]);
        let root = &second.root;
        symlink(root.join("real"), root.join("alias")).unwrap();
        let protected_alias = protected(&[(Variable::Home, root.join("alias/guest"))]);
        let real = root.join("real");
        assert_eq!(
            protected_alias.refusal(identity(&real)),
            Some(Variable::Home)
        );
    }

    #[test]
    fn guards_the_variable_directories_and_their_ancestors_below_a_root() {
        let fixture = fixture(&["home/guest", "temp", "other"]);
        let root = &fixture.root;
        let protected = protected(&[
            (Variable::Home, root.join("home/guest")),
            (Variable::TempDirectory, root.join("temp")),
        ]);
        for guarded in ["home/guest", "home", "temp", ""] {
            assert!(
                protected.is_protected_directory(identity(&root.join(guarded))),
                "{guarded}"
            );
        }
        assert!(!protected.is_protected_directory(identity(&root.join("other"))));
        assert_eq!(protected.refusal(identity(&root.join("temp"))), None);
    }

    #[test]
    fn a_variable_whose_identity_is_unknown_stops_everything() {
        fn unknown(_: &fs::Metadata) -> Option<FileIdentity> {
            None
        }
        let fixture = fixture(&["guest"]);
        let mut variables = TemplateVariables::new();
        variables
            .insert(Variable::Home, fixture.root.join("guest"))
            .unwrap();
        let error = ProtectedRoots::new(&variables, unknown).unwrap_err();
        assert_eq!(error.variable, Variable::Home);
        assert_eq!(error.error, io::ErrorKind::Unsupported);
    }

    #[test]
    fn a_variable_that_cannot_be_located_stops_everything() {
        let fixture = fixture(&[]);
        let root = &fixture.root;
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling")).unwrap();
        let mut variables = TemplateVariables::new();
        variables
            .insert(Variable::Home, root.join("dangling/guest"))
            .unwrap();
        let error = ProtectedRoots::new(&variables, file_identity).unwrap_err();
        assert_eq!(error.variable, Variable::Home);
    }
}
