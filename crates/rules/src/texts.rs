//! The texts of the rules as the user reads them (D-3): the French of each
//! label shown, from `lang/fr.po`, keyed by the English the data write; the
//! English is the data itself. And the one rule every interface follows to
//! choose its language.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::OnceLock;

use crate::ChromiumBrowserKind;

/// The French of the labels, as written by hand from the data.
const FRENCH: &str = include_str!("../lang/fr.po");

/// The context of a label's entry.
const LABEL: &str = "label";

/// What gettext reads to choose a language, in its order of precedence: the
/// list of languages, then the locale (D-7).
pub const SESSION_VARIABLES: [&str; 4] = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"];

/// The language of the texts shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    French,
    English,
}

impl Language {
    /// The language of the session, as gettext chooses its messages (D-7):
    /// unless the locale is `C` or `POSIX`, a `LANGUAGE` list decides alone,
    /// by the first language of it that Okapi speaks (English when there is
    /// none, the texts untranslated); otherwise the locale, from `LC_ALL`,
    /// `LC_MESSAGES` or `LANG` (the first not empty); English when nothing
    /// says French. Every interface follows this one rule. As glibc,
    /// `C.UTF-8` is the `C` locale.
    #[must_use]
    pub fn of_session(read: impl Fn(&str) -> Option<OsString>) -> Self {
        let [list, locales @ ..] = SESSION_VARIABLES;
        let locale = locales
            .into_iter()
            .filter_map(&read)
            .map(|value| value.to_string_lossy().into_owned())
            .find(|value| !value.is_empty());
        let plain = locale
            .as_deref()
            .is_none_or(|locale| matches!(Self::code_of(locale), "C" | "POSIX"));
        let listed = read(list)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !plain && !listed.is_empty() {
            return listed
                .split(':')
                .find_map(|entry| match Self::code_of(entry) {
                    "fr" => Some(Self::French),
                    // As glibc, `C` in the list ends it: the texts untranslated.
                    "en" | "C" | "POSIX" => Some(Self::English),
                    _ => None,
                })
                .unwrap_or(Self::English);
        }
        match locale.as_deref().map(Self::code_of) {
            Some("fr") => Self::French,
            _ => Self::English,
        }
    }

    /// The language code of a locale: `fr_FR.UTF-8`, `fr.UTF-8`, `fr@euro`
    /// and `fr` all say `fr`.
    fn code_of(locale: &str) -> &str {
        locale.split(['_', '.', '@']).next().unwrap_or_default()
    }
}

/// An entry of a catalogue: its context, its key and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    context: String,
    key: String,
    text: String,
}

/// A line of the catalogue this module cannot read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unreadable {
    line: usize,
}

/// The value of `field "…"` on `line`, its escapes (`\"`, `\\`) undone.
fn quoted(line: &str, field: &str) -> Option<String> {
    let value = line
        .strip_prefix(field)?
        .strip_prefix(" \"")?
        .strip_suffix('"')?;
    let mut text = String::new();
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => match characters.next()? {
                escaped @ ('"' | '\\') => text.push(escaped),
                _ => return None,
            },
            '"' => return None,
            other => text.push(other),
        }
    }
    Some(text)
}

/// The entries of `catalogue`, written one field per line (`msgctxt`,
/// `msgid`, `msgstr`), after a header whose `msgid` is empty. Any other form,
/// a missing header included, is refused, never guessed (the header's own
/// quoted lines are skipped unread); the line refused is the one past the end
/// when the catalogue stops short.
fn entries(catalogue: &str) -> Result<Vec<Entry>, Unreadable> {
    // A flag (`#, fuzzy`) would say an entry is not to be trusted: refused.
    if let Some(line) = catalogue.lines().position(|line| line.starts_with("#,")) {
        return Err(Unreadable { line: line + 1 });
    }
    let end = catalogue.lines().count() + 1;
    let mut lines = catalogue
        .lines()
        .enumerate()
        .map(|(index, line)| (index + 1, line))
        .filter(|(_, line)| !line.is_empty() && !line.starts_with('#'))
        .peekable();
    // The header: `msgid ""`, `msgstr ""`, then its quoted lines.
    for name in ["msgid", "msgstr"] {
        match lines.next() {
            Some((_, line)) if quoted(line, name).as_deref() == Some("") => {}
            Some((number, _)) => return Err(Unreadable { line: number }),
            None => return Err(Unreadable { line: end }),
        }
    }
    while lines.next_if(|(_, line)| line.starts_with('"')).is_some() {}
    // Each field on the line read, or the line past the end.
    let field = |line: Option<(usize, &str)>, name| match line {
        Some((number, line)) => quoted(line, name).ok_or(Unreadable { line: number }),
        None => Err(Unreadable { line: end }),
    };
    let mut entries = Vec::new();
    while let Some(line) = lines.next() {
        let context = field(Some(line), "msgctxt")?;
        let key = field(lines.next(), "msgid")?;
        let text = field(lines.next(), "msgstr")?;
        entries.push(Entry { context, key, text });
    }
    Ok(entries)
}

/// The labels among `entries`: those of their context that have a text.
fn labels_of(entries: Vec<Entry>) -> HashMap<String, String> {
    entries
        .into_iter()
        .filter(|entry| entry.context == LABEL && !entry.text.is_empty())
        .map(|entry| (entry.key, entry.text))
        .collect()
}

/// The French labels, read once.
fn french_labels() -> &'static HashMap<String, String> {
    static LABELS: OnceLock<HashMap<String, String>> = OnceLock::new();
    LABELS.get_or_init(|| match entries(FRENCH) {
        Ok(entries) => labels_of(entries),
        Err(Unreadable { line }) => {
            // The tests read the same catalogue: never shipped this way.
            tracing::error!(line, "French catalogue of the rules unreadable");
            HashMap::new()
        }
    })
}

/// The label a rule writes as `english`, in `language`: the English as is
/// when it has no French, said.
#[must_use]
pub fn label(language: Language, english: &str) -> &str {
    if language == Language::English {
        return english;
    }
    french_labels().get(english).map_or_else(
        || {
            tracing::warn!(label = english, "rule label without its French");
            english
        },
        String::as_str,
    )
}

/// The label of a group as shown: a browser's cache by the browser's name
/// and the directory's label (`Chrome : Cache du code`), any other by its
/// label alone, in `language`.
#[must_use]
pub fn group_label(
    language: Language,
    browser: Option<ChromiumBrowserKind>,
    english: &str,
) -> String {
    let label = label(language, english);
    match (browser, language) {
        (None, _) => label.to_owned(),
        (Some(browser), Language::French) => format!("{} : {label}", browser.name()),
        (Some(browser), Language::English) => format!("{}: {label}", browser.name()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{Os, RuleSet};

    /// Every label a cleaning shows on Linux: the subcategories of the system
    /// targets, the names of the application rules and the labels of the
    /// browsers' cache directories.
    fn shown_on_linux() -> BTreeSet<String> {
        let rules = RuleSet::embedded(Os::Linux).unwrap();
        let system = rules.system();
        system
            .clean_targets()
            .iter()
            .map(|target| target.subcategory().to_string())
            .chain(
                system
                    .single_file_targets()
                    .iter()
                    .map(|target| target.subcategory().to_string()),
            )
            .chain(
                rules
                    .apps()
                    .iter()
                    .chain(rules.gaming())
                    .chain(rules.gpu_cache())
                    .map(|rule| rule.name().to_string()),
            )
            .chain({
                let directories = rules.browsers().chromium_cache_directories();
                directories
                    .profile()
                    .iter()
                    .chain(directories.shared())
                    .map(|directory| directory.label().to_string())
            })
            .collect()
    }

    #[test]
    fn every_label_shown_has_its_french_and_no_entry_is_left_over() {
        let entries = entries(FRENCH).unwrap();
        assert_eq!(
            entries.len(),
            FRENCH.matches("\nmsgctxt ").count(),
            "every entry read"
        );
        let keys: BTreeSet<String> = entries.iter().map(|entry| entry.key.clone()).collect();
        assert_eq!(keys.len(), entries.len(), "each label once");
        let texts: BTreeSet<&str> = entries.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(
            texts.len(),
            entries.len(),
            "two labels told apart in English are in French"
        );
        let shown = shown_on_linux();
        let missing: Vec<String> = shown
            .difference(&keys)
            .map(|label| format!("msgctxt \"label\"\nmsgid \"{label}\"\nmsgstr \"\"\n"))
            .collect();
        assert!(
            missing.is_empty(),
            "labels without their French, to add to lang/fr.po:\n{}",
            missing.join("\n")
        );
        let left_over: Vec<&String> = keys.difference(&shown).collect();
        assert!(left_over.is_empty(), "no rule shows {left_over:?}");
        for entry in &entries {
            assert_eq!(entry.context, LABEL, "{entry:?}");
            assert!(!entry.text.is_empty(), "{entry:?}");
            assert!(
                !entry.text.contains(['\u{2014}', '\u{2013}']),
                "no dash of that kind is shown: {entry:?}"
            );
        }
    }

    #[test]
    fn a_label_is_shown_in_the_language_of_the_session() {
        assert_eq!(label(Language::French, "pip Cache"), "Cache de pip");
        assert_eq!(label(Language::English, "pip Cache"), "pip Cache");
        assert_eq!(
            label(Language::French, "Discord"),
            "Discord",
            "a product's name"
        );
        assert_eq!(
            label(Language::French, "No Such Rule"),
            "No Such Rule",
            "the English when there is no French"
        );
    }

    #[test]
    fn a_browser_cache_is_named_by_its_browser() {
        let opera = Some(ChromiumBrowserKind::OperaGx);
        assert_eq!(
            group_label(Language::French, opera, "Code Cache"),
            "Opera GX : Cache du code"
        );
        assert_eq!(
            group_label(Language::English, opera, "Code Cache"),
            "Opera GX: Code Cache"
        );
        assert_eq!(
            group_label(Language::French, None, "pip Cache"),
            "Cache de pip"
        );
    }

    #[test]
    fn the_catalogue_is_read_as_written_or_refused() {
        let header = "msgid \"\"\nmsgstr \"\"\n\"Language: fr\\n\"\n\n";
        let read = entries(&format!(
            "{header}msgctxt \"label\"\nmsgid \"A \\\"quoted\\\" \\\\ one\"\nmsgstr \"Un\"\n"
        ))
        .unwrap();
        assert_eq!(
            read,
            [Entry {
                context: String::from("label"),
                key: String::from("A \"quoted\" \\ one"),
                text: String::from("Un"),
            }]
        );
        for wrong in [
            "msgctxt \"label\"\nmsgid \"A\"\n",
            "msgctxt \"label\"\nmsgstr \"Un\"\nmsgid \"A\"\n",
            "msgctxt \"label\"\nmsgid \"A\\n\"\nmsgstr \"Un\"\n",
            "msgctxt \"label\"\nmsgid \"A\"\nmsgstr \"Un\"\n\"continued\"\n",
            "#, fuzzy\nmsgctxt \"label\"\nmsgid \"A\"\nmsgstr \"Un\"\n",
            "msgctxt \"label\"\nmsgid \"A\"B\"\nmsgstr \"Un\"\n",
        ] {
            assert!(entries(&format!("{header}{wrong}")).is_err(), "{wrong:?}");
        }
        let refused = |catalogue: &str| entries(catalogue).unwrap_err().line;
        assert_eq!(
            refused("msgctxt \"label\"\nmsgid \"A\"\nmsgstr \"Un\"\n"),
            1,
            "no header"
        );
        assert_eq!(refused("msgid \"\"\n"), 2, "a header cut short");
        assert_eq!(
            refused(&format!("{header}#, fuzzy\nmsgctxt \"label\"\n")),
            5,
            "the flag's line"
        );
        assert_eq!(
            refused("msgid \"A\"\nmsgstr \"Un\"\n"),
            1,
            "an entry is no header"
        );
        assert_eq!(
            refused(&format!(
                "{header}msgctxt \"label\"\nmsgid \"A\"\nmsgid \"B\"\n"
            )),
            7,
            "the line at fault"
        );
    }

    #[test]
    fn only_the_labels_with_a_text_are_kept() {
        let entry = |context: &str, key: &str, text: &str| Entry {
            context: context.to_owned(),
            key: key.to_owned(),
            text: text.to_owned(),
        };
        let labels = labels_of(vec![
            entry("label", "pip Cache", "Cache de pip"),
            // Placed after the label: if kept, it would replace it.
            entry("description", "pip Cache", "Une description"),
            entry("label", "npm Cache", ""),
        ]);
        assert_eq!(
            labels,
            HashMap::from([(String::from("pip Cache"), String::from("Cache de pip"))])
        );
    }

    #[test]
    fn the_session_language_follows_gettext() {
        let session = |values: &'static [(&'static str, &'static str)]| {
            Language::of_session(move |name| {
                values
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            })
        };
        assert_eq!(session(&[("LANG", "fr_FR.UTF-8")]), Language::French);
        assert_eq!(
            session(&[("LC_ALL", "en_US.UTF-8"), ("LANG", "fr_FR.UTF-8")]),
            Language::English,
            "LC_ALL first"
        );
        assert_eq!(
            session(&[("LC_ALL", ""), ("LC_MESSAGES", "fr"), ("LANG", "en_US")]),
            Language::French,
            "an empty value is ignored"
        );
        assert_eq!(
            session(&[("LC_ALL", "fr_FR.UTF-8"), ("LC_MESSAGES", "en_US.UTF-8")]),
            Language::French,
            "LC_ALL before LC_MESSAGES"
        );
        assert_eq!(
            session(&[("LC_MESSAGES", "en_US.UTF-8"), ("LANG", "fr_FR.UTF-8")]),
            Language::English,
            "LC_MESSAGES before LANG"
        );
        assert_eq!(session(&[]), Language::English);
        assert_eq!(session(&[("LANG", "C")]), Language::English);
        assert_eq!(session(&[("LANG", "de_DE.UTF-8")]), Language::English);
        assert_eq!(session(&[("LANG", "french")]), Language::English);
        assert_eq!(
            session(&[("LANGUAGE", "de:en"), ("LANG", "fr_FR.UTF-8")]),
            Language::English,
            "the first of LANGUAGE that Okapi speaks"
        );
        assert_eq!(
            session(&[("LANGUAGE", "C:fr"), ("LANG", "fr_FR.UTF-8")]),
            Language::English,
            "C ends the list"
        );
        assert_eq!(
            session(&[("LANGUAGE", "POSIX:fr"), ("LANG", "fr_FR.UTF-8")]),
            Language::English,
            "POSIX ends it too"
        );
        assert_eq!(
            session(&[("LANGUAGE", "fr"), ("LC_ALL", "C"), ("LANG", "fr_FR.UTF-8")]),
            Language::English,
            "the locale is the first set, not LANG"
        );
        assert_eq!(session(&[("LANG", "fr.UTF-8")]), Language::French);
        assert_eq!(
            session(&[("LANGUAGE", "fr_CA:en"), ("LANG", "en_US.UTF-8")]),
            Language::French
        );
        assert_eq!(
            session(&[("LANGUAGE", "fr"), ("LANG", "C")]),
            Language::English,
            "LANGUAGE is not read for the C locale"
        );
        for plain in ["POSIX", "C.UTF-8"] {
            assert_eq!(
                Language::of_session(|name| match name {
                    "LANGUAGE" => Some(OsString::from("fr")),
                    "LANG" => Some(OsString::from(plain)),
                    _ => None,
                }),
                Language::English,
                "nor for {plain}"
            );
        }
        assert_eq!(
            session(&[("LANGUAGE", "fr")]),
            Language::English,
            "nor without a locale"
        );
        assert_eq!(
            session(&[("LANGUAGE", "de"), ("LANG", "fr_FR.UTF-8")]),
            Language::English,
            "nothing spoken there: untranslated, as gettext"
        );
        assert_eq!(
            session(&[("LANGUAGE", ""), ("LANG", "fr_FR.UTF-8")]),
            Language::French,
            "an empty list: the locale"
        );
        assert_eq!(session(&[("LANG", "fr@euro")]), Language::French);
    }
}
