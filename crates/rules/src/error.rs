use std::path::PathBuf;

use thiserror::Error;

use crate::os::Os;
use crate::rule_set::RuleFileKind;
use crate::template::Variable;

/// A value in a rule file breaks one of the format's constraints.
///
/// Raised while a rule file is deserialized. Its message reaches the caller
/// inside [`RulesError::Parse`], with the file name and, when `serde_json` can
/// give one, the position of the faulty value. An unknown `$name` reference is
/// only found once the whole file is read: it has no position, and names its
/// database target instead.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("empty text")]
    EmptyText,

    #[error("empty list")]
    EmptyList,

    #[error("duplicate value `{value}` in `{field}`")]
    DuplicateEntry { field: &'static str, value: String },

    #[error("duplicate value `{value}`")]
    DuplicateValue { value: String },

    #[error(
        "`{name}` is not a single literal name (no separator, `:` or `*`, and not `.` or `..`)"
    )]
    InvalidName { name: String },

    #[error("`{pattern}` is not a single name pattern (no separator or `:`, and not `.` or `..`)")]
    InvalidNamePattern { pattern: String },

    #[error("`{path}` is not a relative path of literal names")]
    InvalidRelativePath { path: String },

    #[error("`{path}` is not an anchor path: each segment is a literal name or a whole `*`")]
    InvalidAnchorPath { path: String },

    #[error("`{path}` must start with a `${{VARIABLE}}`, `/` or a drive such as `D:/`")]
    InvalidTemplatePath { path: String },

    #[error("`{path}`: `{relative}` is not a relative path of literal names")]
    InvalidRelativePart { path: String, relative: String },

    #[error("unknown template variable `${{{name}}}`")]
    UnknownVariable { name: String },

    #[error("`{path}` alone would empty a whole user or system directory; target a subdirectory")]
    WholeVariableDirectory { path: String },

    #[error("`{path}` alone needs `anchorPaths`: a free search below it could reach user data")]
    UnboundedRecursiveSearch { path: String },

    #[error("`{id}` is not a rule identifier (lowercase letters, digits and hyphens)")]
    InvalidRuleId { id: String },

    #[error("{days} days is outside the allowed age range ({min} to {max})")]
    AgeOutOfRange { days: u32, min: u16, max: u16 },

    #[error("maximum depth {depth} is outside the allowed range ({min} to {max})")]
    DepthOutOfRange { depth: u32, min: u8, max: u8 },

    #[error("`{rule}` combines `{first}` and `{second}`, which exclude each other")]
    ConflictingFields {
        rule: String,
        first: &'static str,
        second: &'static str,
    },

    #[error("anchor path `{anchor_path}` must end with the anchor `{anchor}`")]
    AnchorPathMismatch { anchor: String, anchor_path: String },

    #[error("database target `{label}`: `{reference}` does not name a shared database file set")]
    UnknownDatabaseFileSet { label: String, reference: String },

    #[error("`profilePattern` needs `multiProfile: true` (database target `{label}`)")]
    ProfilePatternWithoutProfiles { label: String },
}

/// A set of rules for one operating system cannot be loaded.
#[derive(Debug, Error)]
pub enum RulesError {
    #[error("{os} rules, {file}: {source}")]
    Parse {
        os: Os,
        file: RuleFileKind,
        #[source]
        source: serde_json::Error,
    },

    #[error("{os} rules, {file}: its `type` field announces the content of {found}")]
    UnexpectedFileType {
        os: Os,
        file: RuleFileKind,
        found: RuleFileKind,
    },

    #[error("{os} rules, {file}: `{path}` uses a variable or a root that does not exist on {os}")]
    PathUnavailable {
        os: Os,
        file: RuleFileKind,
        path: String,
    },

    #[error("{os} rules: the rule identifier `{id}` is used in {first} and again in {second}")]
    DuplicateRuleId {
        os: Os,
        id: String,
        first: RuleFileKind,
        second: RuleFileKind,
    },

    #[error("{os} rules, databases.json: the label `{label}` is used by two targets")]
    DuplicateDatabaseLabel { os: Os, label: String },

    /// The privileged helper is asked for its targets by their path.
    #[error(
        "{os} rules, system.json: `{path}` is the path of a target that needs the administrator and of another target"
    )]
    DuplicateAdminTarget { os: Os, path: String },
}

/// A template path or a variable value cannot be used to build a real path.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum TemplateError {
    #[error("template variable `${{{}}}` has no value", variable.name())]
    MissingVariable { variable: Variable },

    #[error("template variable `${{{}}}` must be an absolute path, got `{}`", variable.name(), value.display())]
    RelativeValue { variable: Variable, value: PathBuf },

    #[error("template variable `${{{}}}` must be a plain directory below a root, got `{}`", variable.name(), value.display())]
    UnsafeValue { variable: Variable, value: PathBuf },

    #[error("`${{TMPDIR}}` (`{}`) would contain `${{{}}}` (`{}`): emptying it would empty that directory", temp.display(), variable.name(), value.display())]
    TempDirectoryContainsVariable {
        temp: PathBuf,
        variable: Variable,
        value: PathBuf,
    },

    #[error("`{}` is not an absolute path on this system", path.display())]
    NotAbsolute { path: PathBuf },
}
