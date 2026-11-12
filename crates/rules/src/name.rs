use std::fmt;

use serde::Deserialize;

use crate::error::ValidationError;

/// The separator rule files use between path segments, whatever the OS.
pub(crate) const RULE_SEPARATOR: char = '/';

/// The only wildcard of the rule format: any run of characters within one name.
pub(crate) const WILDCARD: char = '*';

/// Whether `text` is one path segment that cannot climb out of, or replace, the
/// directory it is joined to.
///
/// Rejected: empty, `.`, `..`, separators, NUL, and `:` (on Windows, `C:` or
/// `D:x` would replace the base path when pushed). `*` is accepted only when
/// `wildcard_allowed`.
fn is_plain_segment(text: &str, wildcard_allowed: bool) -> bool {
    !matches!(text, "" | "." | "..")
        && !text.contains([RULE_SEPARATOR, '\\', '\0', ':'])
        && (wildcard_allowed || !text.contains(WILDCARD))
}

/// A literal file or directory name: one path segment, no wildcard.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct Name(String);

impl Name {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Name {
    type Error = ValidationError;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        if is_plain_segment(&name, false) {
            Ok(Self(name))
        } else {
            Err(ValidationError::InvalidName { name })
        }
    }
}

impl fmt::Display for Name {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A pattern matched against one name, where `*` stands for any run of
/// characters (`Profile *`, `*.default*`). Like [`Name`], it never spans segments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct NamePattern(String);

impl NamePattern {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for NamePattern {
    type Error = ValidationError;

    fn try_from(pattern: String) -> Result<Self, Self::Error> {
        if is_plain_segment(&pattern, true) {
            Ok(Self(pattern))
        } else {
            Err(ValidationError::InvalidNamePattern { pattern })
        }
    }
}

impl fmt::Display for NamePattern {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A relative path written `a/b/c` in rule files: literal [`Name`] segments
/// only, so it always stays below the directory it is joined to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize)]
#[serde(try_from = "String")]
pub struct RelativePath(Vec<Name>);

impl RelativePath {
    /// The segments, from the outermost directory to the last name.
    #[must_use]
    pub fn segments(&self) -> &[Name] {
        &self.0
    }
}

impl TryFrom<String> for RelativePath {
    type Error = ValidationError;

    fn try_from(path: String) -> Result<Self, Self::Error> {
        // `split` yields at least one segment, and an empty path yields one empty
        // segment, which `Name` rejects: the result is never empty.
        let segments: Result<Vec<Name>, ValidationError> = path
            .split(RULE_SEPARATOR)
            .map(|segment| Name::try_from(segment.to_owned()))
            .collect();
        match segments {
            Ok(segments) => Ok(Self(segments)),
            Err(_) => Err(ValidationError::InvalidRelativePath { path }),
        }
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, segment) in self.0.iter().enumerate() {
            if index > 0 {
                write!(formatter, "{RULE_SEPARATOR}")?;
            }
            formatter.write_str(segment.as_str())?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Result<Name, ValidationError> {
        Name::try_from(text.to_owned())
    }

    fn pattern(text: &str) -> Result<NamePattern, ValidationError> {
        NamePattern::try_from(text.to_owned())
    }

    fn relative(text: &str) -> Result<RelativePath, ValidationError> {
        RelativePath::try_from(text.to_owned())
    }

    #[test]
    fn name_accepts_ordinary_names() {
        for text in ["Cache", "Code Cache", ".cache", "..hidden", "security.evtx"] {
            assert_eq!(name(text).unwrap().as_str(), text);
        }
    }

    #[test]
    fn name_rejects_anything_that_is_not_one_literal_segment() {
        for text in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "nul\0byte",
            "C:",
            "D:Games",
            "Cache:stream",
            "*",
            "*.default*",
        ] {
            assert_eq!(
                name(text),
                Err(ValidationError::InvalidName {
                    name: text.to_owned()
                }),
                "{text}"
            );
        }
    }

    #[test]
    fn a_pattern_accepts_wildcards_but_stays_one_segment() {
        for text in ["*.default*", "Profile *", "Default"] {
            assert_eq!(pattern(text).unwrap().as_str(), text);
        }
        for text in ["", "..", "*/x", "C:*", "a\\*"] {
            assert_eq!(
                pattern(text),
                Err(ValidationError::InvalidNamePattern {
                    pattern: text.to_owned()
                }),
                "{text}"
            );
        }
    }

    #[test]
    fn relative_path_keeps_its_segments_in_order() {
        let path = relative("Data/Library/Caches").unwrap();
        let segments: Vec<&str> = path.segments().iter().map(Name::as_str).collect();
        assert_eq!(segments, ["Data", "Library", "Caches"]);
        assert_eq!(path.to_string(), "Data/Library/Caches");
    }

    #[test]
    fn relative_path_rejects_escapes_and_empty_segments() {
        for text in [
            "", "/etc", "a//b", "a/", "../a", "a/./b", "a\\b", "a/C:/b", "a/*",
        ] {
            assert_eq!(
                relative(text),
                Err(ValidationError::InvalidRelativePath {
                    path: text.to_owned()
                }),
                "{text}"
            );
        }
    }
}
