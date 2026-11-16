//! The texts of the rules as the user reads them (D-3): the French of each
//! label and description shown, from `lang/fr.po`, keyed by the English the
//! data write; the English is the data itself. And the language of the
//! session, which both interfaces follow unless the user chose one in the
//! settings.

use std::collections::HashMap;
use std::ffi::OsString;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::BrowserKind;

/// The French of the texts, as written by hand from the data.
const FRENCH: &str = include_str!("../lang/fr.po");

/// The context of a label's entry.
const LABEL: &str = "label";

/// The context of a description's entry.
const DESCRIPTION: &str = "description";

/// What gettext reads to choose a language, in its order of precedence: the
/// list of languages, then the locale (D-7).
pub const SESSION_VARIABLES: [&str; 4] = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"];

/// The language of the texts shown, written by its code where the user
/// chooses it (`settings.json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "fr")]
    French,
    #[serde(rename = "en")]
    English,
}

impl Language {
    /// The language of the session, as gettext chooses its messages (D-7):
    /// unless the locale is `C` or `POSIX`, a `LANGUAGE` list decides alone,
    /// by the first language of it that Okapi speaks (English when there is
    /// none, the texts untranslated); otherwise the locale, from `LC_ALL`,
    /// `LC_MESSAGES` or `LANG` (the first not empty); English when nothing
    /// says French. Both interfaces follow it, unless the settings choose a
    /// language (`okapi_store::Settings::language`). As glibc,
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

/// The texts of a catalogue, by their key, one table per context.
#[derive(Debug, Default, PartialEq, Eq)]
struct Texts {
    labels: HashMap<String, String>,
    descriptions: HashMap<String, String>,
}

impl Texts {
    /// Those of `entries` that have a text, each in its context's table;
    /// any other context is not read.
    fn of(entries: Vec<Entry>) -> Self {
        let mut texts = Self::default();
        for entry in entries.into_iter().filter(|entry| !entry.text.is_empty()) {
            let table = match entry.context.as_str() {
                LABEL => &mut texts.labels,
                DESCRIPTION => &mut texts.descriptions,
                context => {
                    tracing::warn!(
                        context,
                        key = entry.key,
                        "catalogue entry of no known context, ignored"
                    );
                    continue;
                }
            };
            table.insert(entry.key, entry.text);
        }
        texts
    }
}

/// The French texts, read once.
fn french() -> &'static Texts {
    static TEXTS: OnceLock<Texts> = OnceLock::new();
    TEXTS.get_or_init(|| match entries(FRENCH) {
        Ok(entries) => Texts::of(entries),
        Err(Unreadable { line }) => {
            // The tests read the same catalogue: never shipped this way.
            tracing::error!(line, "French catalogue of the rules unreadable");
            Texts::default()
        }
    })
}

/// `english`, a text of the rules in the context `context`, in `language`
/// from the French `table`: the English as is when it has no French, said.
fn translated<'a>(
    language: Language,
    table: fn(&Texts) -> &HashMap<String, String>,
    context: &str,
    english: &'a str,
) -> &'a str {
    if language == Language::English {
        return english;
    }
    table(french()).get(english).map_or_else(
        || {
            tracing::warn!(context, text = english, "rule text without its French");
            english
        },
        String::as_str,
    )
}

/// The label a rule writes as `english`, in `language`: the English as is
/// when it has no French, said.
#[must_use]
pub fn label(language: Language, english: &str) -> &str {
    translated(language, |texts| &texts.labels, LABEL, english)
}

/// The description a rule writes as `english` (what it removes and why
/// that is safe), in `language`: the English as is when it has no French,
/// said.
#[must_use]
pub fn description(language: Language, english: &str) -> &str {
    translated(language, |texts| &texts.descriptions, DESCRIPTION, english)
}

/// The label of a group as shown: a browser's cache by the browser's name
/// and the directory's label (`Chrome : Cache du code`), any other by its
/// label alone, in `language`.
#[must_use]
pub fn group_label(language: Language, browser: Option<BrowserKind>, english: &str) -> String {
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
    use crate::{Effect, Label, Os, RuleSet};

    /// A rule as a cleaning shows it: its label, whether it offers anything
    /// to remove (a maintenance offers no group, so no description is shown
    /// for it), and its description.
    struct ShownRule {
        label: String,
        deletes: bool,
        description: Option<String>,
    }

    /// Every rule a cleaning shows on Linux: the system targets and files,
    /// the application rules and the browsers' cache directories.
    fn rules_on_linux() -> Vec<ShownRule> {
        let shown = |label: &Label, deletes, description: Option<&Label>| ShownRule {
            label: label.to_string(),
            deletes,
            description: description.map(Label::to_string),
        };
        let rules = RuleSet::embedded(Os::Linux).unwrap();
        let system = rules.system();
        let browsers = rules.browsers();
        let directories = browsers.chromium_cache_directories();
        system
            .clean_targets()
            .iter()
            .map(|target| {
                let deletes = matches!(target.effect(), Effect::Delete(_));
                shown(target.subcategory(), deletes, target.description())
            })
            .chain(
                system
                    .single_file_targets()
                    .iter()
                    .map(|target| shown(target.subcategory(), true, target.description())),
            )
            .chain(
                rules
                    .apps()
                    .iter()
                    .chain(rules.gaming())
                    .chain(rules.gpu_cache())
                    .map(|rule| {
                        let deletes = matches!(rule.effect(), Effect::Delete(_));
                        shown(rule.name(), deletes, rule.description())
                    }),
            )
            .chain(
                directories
                    .profile()
                    .iter()
                    .chain(directories.shared())
                    .chain(browsers.firefox_cache_directories())
                    .map(|directory| shown(directory.label(), true, directory.description())),
            )
            .collect()
    }

    /// Checks the catalogue's entries of `context` against the texts
    /// `shown` in it: each has its French, each is there once, two told
    /// apart in English are in French, none is left over.
    fn check_context(entries: &[Entry], context: &str, shown: &BTreeSet<String>) {
        let entries: Vec<&Entry> = entries
            .iter()
            .filter(|entry| entry.context == context)
            .collect();
        let keys: BTreeSet<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
        assert_eq!(keys.len(), entries.len(), "each {context} once");
        let texts: BTreeSet<&str> = entries.iter().map(|entry| entry.text.as_str()).collect();
        assert_eq!(
            texts.len(),
            entries.len(),
            "two {context}s told apart in English are in French"
        );
        let missing: Vec<String> = shown
            .iter()
            .filter(|text| !keys.contains(text.as_str()))
            .map(|text| format!("msgctxt \"{context}\"\nmsgid \"{text}\"\nmsgstr \"\"\n"))
            .collect();
        assert!(
            missing.is_empty(),
            "{context}s without their French, to add to lang/fr.po:\n{}",
            missing.join("\n")
        );
        let left_over: Vec<&&str> = keys.iter().filter(|key| !shown.contains(**key)).collect();
        assert!(
            left_over.is_empty(),
            "no rule shows the {context}s {left_over:?}"
        );
    }

    #[test]
    fn a_language_is_written_by_its_code() {
        for (language, code) in [(Language::French, "\"fr\""), (Language::English, "\"en\"")] {
            assert_eq!(serde_json::to_string(&language).unwrap(), code);
            assert_eq!(serde_json::from_str::<Language>(code).unwrap(), language);
        }
        for unknown in ["\"de\"", "\"French\"", "\"FR\""] {
            assert!(
                serde_json::from_str::<Language>(unknown).is_err(),
                "{unknown}"
            );
        }
    }

    #[test]
    fn every_text_shown_has_its_french_and_no_entry_is_left_over() {
        let entries = entries(FRENCH).unwrap();
        assert_eq!(
            entries.len(),
            FRENCH.matches("\nmsgctxt ").count(),
            "every entry read"
        );
        let rules = rules_on_linux();
        let labels: BTreeSet<String> = rules.iter().map(|rule| rule.label.clone()).collect();
        check_context(&entries, LABEL, &labels);
        let descriptions: BTreeSet<String> = rules
            .iter()
            .filter(|rule| rule.deletes)
            .filter_map(|rule| rule.description.clone())
            .collect();
        check_context(&entries, DESCRIPTION, &descriptions);
        for english in labels.iter().chain(&descriptions) {
            assert!(
                !english.contains(['\u{2014}', '\u{2013}']),
                "no dash of that kind is shown in English either: {english}"
            );
        }
        for entry in &entries {
            assert!(
                [LABEL, DESCRIPTION].contains(&entry.context.as_str()),
                "{entry:?}"
            );
            assert!(!entry.text.is_empty(), "{entry:?}");
            assert!(
                !entry.text.contains(['\u{2014}', '\u{2013}']),
                "no dash of that kind is shown: {entry:?}"
            );
        }
    }

    #[test]
    fn every_rule_shown_says_why_it_is_safe() {
        let undescribed: Vec<String> = rules_on_linux()
            .into_iter()
            .filter(|rule| rule.deletes && rule.description.is_none())
            .map(|rule| rule.label)
            .collect();
        assert!(
            undescribed.is_empty(),
            "rules shown without a description: {undescribed:?}"
        );
    }

    #[test]
    fn a_description_is_shown_in_the_language_of_the_session() {
        let pip = "pip downloaded wheel and source distribution cache";
        assert_eq!(
            description(Language::French, pip),
            "Paquets wheel et archives de sources téléchargés par pip."
        );
        assert_eq!(description(Language::English, pip), pip);
        assert_eq!(
            description(Language::French, "No such description"),
            "No such description",
            "the English when there is no French"
        );
        assert_eq!(
            description(Language::French, "pip Cache"),
            "pip Cache",
            "a label is no description"
        );
        assert_eq!(
            label(Language::French, pip),
            pip,
            "nor a description a label"
        );
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
        let opera = Some(BrowserKind::Chromium(crate::ChromiumBrowserKind::OperaGx));
        assert_eq!(
            group_label(Language::French, opera, "Code Cache"),
            "Opera GX : Cache du code"
        );
        assert_eq!(
            group_label(Language::English, opera, "Code Cache"),
            "Opera GX: Code Cache"
        );
        assert_eq!(
            group_label(
                Language::French,
                Some(BrowserKind::Firefox(crate::FirefoxKind::Librewolf)),
                "Startup Cache"
            ),
            "LibreWolf : Cache de démarrage"
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
    fn each_context_keeps_its_own_texts() {
        let entry = |context: &str, key: &str, text: &str| Entry {
            context: context.to_owned(),
            key: key.to_owned(),
            text: text.to_owned(),
        };
        let table = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(key, text)| ((*key).to_owned(), (*text).to_owned()))
                .collect()
        };
        let texts = Texts::of(vec![
            entry("label", "pip Cache", "Cache de pip"),
            // Placed after the label of the same key: if read as one, it
            // would replace it.
            entry("description", "pip Cache", "Une description"),
            entry("description", "npm cache", "Le cache de npm"),
            // Before the label of the same key: it would be replaced.
            entry("label", "npm cache", "Cache de npm"),
            entry("label", "yarn Cache", ""),
            entry("note", "pip Cache", "Une note"),
        ]);
        assert_eq!(
            texts,
            Texts {
                labels: table(&[("pip Cache", "Cache de pip"), ("npm cache", "Cache de npm")]),
                descriptions: table(&[
                    ("pip Cache", "Une description"),
                    ("npm cache", "Le cache de npm")
                ]),
            }
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
