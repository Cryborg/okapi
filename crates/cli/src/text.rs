//! Every text the command line shows, in French and English; the labels of
//! the rules come from their catalogue (`okapi_rules::label`).

use std::io;
use std::path::Path;

use okapi_engine::{
    ByteSize, CleanResult, ExclusionError, KeepReason, Limit, Origin, ScanIssue, SizeUnit, Stop,
    UnsafeReason,
};
use okapi_platform::VariablesError;
use okapi_rules::{ChromiumBrowserKind, Language, TemplateError, Variable};
use okapi_store::StoreError;

use crate::args::ArgumentError;
use crate::selection::{Decision, rule_name};

/// `path` as shown in a terminal: a control character in a name (an escape
/// sequence a stranger planted in `/tmp`, a line break) is written escaped,
/// never sent to the terminal.
fn shown(path: impl AsRef<Path>) -> String {
    path.as_ref()
        .to_string_lossy()
        .chars()
        .flat_map(|character| {
            let escaped: Vec<char> = if character.is_control() {
                character.escape_default().collect()
            } else {
                vec![character]
            };
            escaped
        })
        .collect()
}

/// Something the command line says.
#[derive(Debug)]
pub enum Message<'a> {
    /// The help, with where the settings and the history are read and kept.
    Help {
        settings: Option<&'a Path>,
        history: Option<&'a Path>,
    },
    Version,
    Argument(&'a ArgumentError),
    Exclusion(&'a ExclusionError),
    ExclusionMissing(&'a Path),
    /// An exclusion of the settings file (first) that does not exist as
    /// written.
    SettingsExclusionMissing(&'a Path, &'a Path),
    /// The cleaning is done, but its history line could not be written.
    HistoryNotRecorded(&'a StoreError),
    Store(&'a StoreError),
    SettingsExclusion(&'a Path, &'a ExclusionError),
    SettingsMinAge(&'a Path, u64),
    DeletionLogUnavailable,
    SettingsDirectoryUnknown,
    HistoryDirectoryUnknown,
    HistoryContinues,
    InternalDefect,
    Variables(&'a VariablesError),
    RulesUnavailable,
    RunningAsRoot,
    Stop(&'a Stop),
    CleaningStopped(&'a Stop),
    JournalRotationFailed(&'a io::Error),
    Issue(&'a ScanIssue),
    Group {
        browser: Option<ChromiumBrowserKind>,
        label: &'a str,
        entries: usize,
        bytes: u64,
        decision: Decision,
        clean: bool,
    },
    Entry {
        path: &'a Path,
        bytes: u64,
    },
    Found {
        entries: usize,
        bytes: u64,
    },
    NothingFound,
    NothingToClean,
    Cleaned {
        bytes: u64,
        entries: usize,
    },
    NotRemoved {
        path: &'a Path,
        result: CleanResult,
        blocked: Option<&'a Path>,
    },
    Journal(&'a Path),
    JournalUnavailable(&'a Path, &'a io::Error),
    StateDirectoryUnknown,
    JournalWriteFailed(&'a io::Error),
    OutputFailed(&'a io::Error),
    Licenses,
}

/// The program's version, as Cargo knows it.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// `message` in `language`.
#[expect(
    clippy::too_many_lines,
    reason = "one arm per message, the long ones delegated: a table, not logic"
)]
pub fn text(language: Language, message: &Message<'_>) -> String {
    let french = language == Language::French;
    let pick = |fr: String, en: String| if french { fr } else { en };
    match message {
        Message::Help { settings, history } => help(language, *settings, *history),
        Message::Version => format!("okapi {VERSION}"),
        Message::Argument(error) => argument_text(language, error),
        Message::Exclusion(error) => exclusion(language, error),
        Message::ExclusionMissing(path) => exclusion_missing(language, path),
        Message::SettingsExclusionMissing(settings, path) => {
            in_settings(language, settings, &exclusion_missing(language, path))
        }
        Message::HistoryNotRecorded(error) => {
            let error = store_text(language, error);
            pick(
                format!("La ligne d'historique de ce nettoyage manque. {error}"),
                format!("This cleaning's history line is missing. {error}"),
            )
        }
        Message::Store(error) => store_text(language, error),
        Message::SettingsExclusion(path, error) => settings_exclusion(language, path, error),
        Message::SettingsMinAge(path, minutes) => settings_min_age(language, path, *minutes),
        Message::SettingsDirectoryUnknown => pick(
            "Dossier des réglages inconnu (ni XDG_CONFIG_HOME ni HOME absolu) : rien n'est fait.".to_owned(),
            "No settings directory (no absolute XDG_CONFIG_HOME nor HOME): nothing is done.".to_owned(),
        ),
        Message::HistoryDirectoryUnknown => pick(
            "Dossier de l'historique inconnu (ni XDG_STATE_HOME ni HOME absolu).".to_owned(),
            "No history directory (no absolute XDG_STATE_HOME nor HOME).".to_owned(),
        ),
        Message::HistoryContinues => pick(
            "Ce nettoyage ne sera pas inscrit à l'historique.".to_owned(),
            "This cleaning will not be added to the history.".to_owned(),
        ),
        Message::DeletionLogUnavailable => pick(
            "Le journal des suppressions est demandé mais ne peut pas être tenu : rien n'est nettoyé.".to_owned(),
            "A deletion log is asked for but cannot be kept: nothing is cleaned.".to_owned(),
        ),
        Message::InternalDefect => pick(
            "Un identifiant inconnu a été transmis au nettoyage : c'est un défaut d'Okapi.".to_owned(),
            "An unknown identifier reached the cleaning: this is a defect of Okapi.".to_owned(),
        ),
        Message::Variables(error) => variables(language, error),
        Message::RulesUnavailable => pick(
            "Les règles de nettoyage intégrées sont illisibles : c'est un défaut d'Okapi.".to_owned(),
            "The embedded cleaning rules cannot be read: this is a defect of Okapi.".to_owned(),
        ),
        Message::RunningAsRoot => pick(
            "Okapi ne se lance pas en root : ce qui exige l'administrateur passera par son outil dédié.".to_owned(),
            "Okapi does not run as root: what needs the administrator will go through its own helper.".to_owned(),
        ),
        Message::Stop(stop) => stop_text(language, stop, false),
        Message::CleaningStopped(stop) => stop_text(language, stop, true),
        Message::JournalRotationFailed(error) => failure(
            language,
            error,
            ("L'ancien journal n'a pas pu être mis de côté", " Okapi continue dans le même."),
            ("The old journal could not be set aside", " Appending to it."),
        ),
        Message::Issue(issue) => issue_text(language, issue),
        Message::Group {
            browser,
            label,
            entries,
            bytes,
            decision,
            clean,
        } => group_text(language, *browser, label, *entries, *bytes, *decision, *clean),
        Message::Entry { path, bytes } => {
            format!("    {} ({})", shown(path), size(language, *bytes))
        }
        Message::Found { entries, bytes } => {
            let size = size(language, *bytes);
            let count = entries_text(language, *entries);
            pick(format!("Trouvé : {count}, {size}."), format!("Found: {count}, {size}."))
        }
        Message::NothingFound => pick(
            "Rien trouvé dans les catégories demandées.".to_owned(),
            "Nothing found in the categories asked for.".to_owned(),
        ),
        Message::NothingToClean => pick(
            "Rien à nettoyer par défaut : les groupes trouvés sont à choisir ou exigent l'administrateur.".to_owned(),
            "Nothing to clean by default: the groups found are to be chosen or need the administrator.".to_owned(),
        ),
        Message::Cleaned { bytes, entries } => {
            let size = size(language, *bytes);
            let count = entries_text(language, *entries);
            pick(
                format!("Nettoyé : {size} ({count} retirée{}).", if *entries > 1 { "s" } else { "" }),
                format!("Cleaned: {size} ({count} removed)."),
            )
        }
        Message::NotRemoved {
            path,
            result,
            blocked,
        } => not_removed_text(language, path, *result, *blocked),
        Message::Journal(path) => pick(
            format!("Journal : {}", shown(path)),
            format!("Journal: {}", shown(path)),
        ),
        Message::JournalUnavailable(path, error) => journal_unavailable(language, path, error),
        Message::StateDirectoryUnknown => pick(
            "Journal indisponible : dossier d'état inconnu (ni XDG_STATE_HOME ni HOME absolu). Okapi continue sans.".to_owned(),
            "Journal unavailable: no state directory (no absolute XDG_STATE_HOME nor HOME). Running without it.".to_owned(),
        ),
        Message::JournalWriteFailed(error) => failure(
            language,
            error,
            ("Le journal ne peut plus être écrit", " Okapi continue sans."),
            ("The journal can no longer be written", " Running without it."),
        ),
        Message::OutputFailed(error) => failure(
            language,
            error,
            ("La sortie standard a échoué", ""),
            ("Standard output failed", ""),
        ),
        Message::Licenses => pick(
            "Okapi est sous licence MIT. Il embarque des éléments tiers :".to_owned(),
            "Okapi is under the MIT license. It embeds third-party material:".to_owned(),
        ),
    }
}

fn argument_text(language: Language, error: &ArgumentError) -> String {
    let french = language == Language::French;
    match error {
        ArgumentError::Unknown(argument) => {
            if french {
                format!("Argument inconnu : « {argument} ». Voir okapi --help.")
            } else {
                format!("Unknown argument: \"{argument}\". See okapi --help.")
            }
        }
        ArgumentError::MissingValue(flag) => {
            if french {
                format!("{flag} attend une valeur.")
            } else {
                format!("{flag} needs a value.")
            }
        }
        ArgumentError::InvalidMinAge(value) => {
            if french {
                format!("Âge minimal invalide : {value} (un nombre entier de minutes).")
            } else {
                format!("Invalid minimum age: {value} (a whole number of minutes).")
            }
        }
        ArgumentError::VerboseAndQuiet => {
            if french {
                "--verbose et --quiet ne vont pas ensemble.".to_owned()
            } else {
                "--verbose and --quiet cannot go together.".to_owned()
            }
        }
    }
}

fn exclusion_missing(language: Language, path: &Path) -> String {
    match language {
        Language::French => format!(
            "Attention : l'exclusion {} n'existe pas telle quelle (faute de frappe ?).",
            shown(path)
        ),
        Language::English => format!(
            "Warning: the exclusion {} does not exist as written (a typing mistake?).",
            shown(path)
        ),
    }
}

/// `message`, said of the settings file at `path`.
fn in_settings(language: Language, path: &Path, message: &str) -> String {
    match language {
        Language::French => format!("Dans les réglages {} : {message}", shown(path)),
        Language::English => format!("In the settings {}: {message}", shown(path)),
    }
}

fn settings_exclusion(language: Language, path: &Path, error: &ExclusionError) -> String {
    in_settings(language, path, &exclusion(language, error))
}

fn settings_min_age(language: Language, path: &Path, minutes: u64) -> String {
    match language {
        Language::French => format!(
            "Dans les réglages {} : minAgeMinutes ({minutes}) dépasse ce que l'horloge peut compter.",
            shown(path)
        ),
        Language::English => format!(
            "In the settings {}: minAgeMinutes ({minutes}) is beyond what the clock can count.",
            shown(path)
        ),
    }
}

fn store_text(language: Language, error: &StoreError) -> String {
    let french = language == Language::French;
    let pick = |fr: String, en: String| if french { fr } else { en };
    match error {
        StoreError::Unreadable { path, error } => {
            let error = error_text(language, error.kind(), error.raw_os_error());
            if french {
                format!(
                    "Impossible de lire {} : {error}. Rien n'est fait.",
                    shown(path)
                )
            } else {
                format!("{} cannot be read: {error}. Nothing is done.", shown(path))
            }
        }
        StoreError::BrokenLink { path } => pick(
            format!(
                "{} est un lien qui ne mène nulle part : rien n'est fait.",
                shown(path)
            ),
            format!(
                "{} is a link that leads nowhere: nothing is done.",
                shown(path)
            ),
        ),
        StoreError::NotAFile { path } => pick(
            format!(
                "{} n'est pas un fichier ordinaire : rien n'est fait.",
                shown(path)
            ),
            format!("{} is not a regular file: nothing is done.", shown(path)),
        ),
        StoreError::TooLarge { path, limit } => pick(
            format!(
                "{} dépasse {} : ce ne sont pas des réglages, rien n'est fait.",
                shown(path),
                size(language, *limit)
            ),
            format!(
                "{} is larger than {}: these are no settings, nothing is done.",
                shown(path),
                size(language, *limit)
            ),
        ),
        StoreError::Invalid { path, error } => {
            // The parser's own words (in English) say which field is wrong;
            // its position is said once, in the user's language.
            let (line, column) = (error.line(), error.column());
            let full = error.to_string();
            let error = full
                .strip_suffix(&format!(" at line {line} column {column}"))
                .unwrap_or(&full);
            if french {
                format!(
                    "{} est invalide (ligne {line}, colonne {column} : {error}) : rien n'est fait.",
                    shown(path)
                )
            } else {
                format!(
                    "{} is invalid (line {line}, column {column}: {error}): nothing is done.",
                    shown(path)
                )
            }
        }
        StoreError::UnsupportedVersion { path, version, .. } => {
            if french {
                format!(
                    "{} est en version {version}, que cet Okapi ne lit pas : rien n'est fait.",
                    shown(path)
                )
            } else {
                format!(
                    "{} is in version {version}, which this Okapi cannot read: nothing is done.",
                    shown(path)
                )
            }
        }
        StoreError::Unwritable { path, error } => {
            let error = error_text(language, error.kind(), error.raw_os_error());
            if french {
                format!("Impossible d'écrire {} : {error}.", shown(path))
            } else {
                format!("{} cannot be written: {error}.", shown(path))
            }
        }
    }
}

fn journal_unavailable(language: Language, path: &Path, error: &io::Error) -> String {
    let error = error_text(language, error.kind(), error.raw_os_error());
    let path = shown(path);
    match language {
        Language::French => {
            format!("Journal indisponible ({path}) : {error}. Okapi continue sans.")
        }
        Language::English => format!("Journal unavailable ({path}): {error}. Running without it."),
    }
}

/// `what` failed with `error`, then `then`, as (what, then) in each language.
fn failure(
    language: Language,
    error: &io::Error,
    french: (&str, &str),
    english: (&str, &str),
) -> String {
    let error = error_text(language, error.kind(), error.raw_os_error());
    match language {
        Language::French => format!("{} : {error}.{}", french.0, french.1),
        Language::English => format!("{}: {error}.{}", english.0, english.1),
    }
}

fn group_text(
    language: Language,
    browser: Option<ChromiumBrowserKind>,
    label: &str,
    entries: usize,
    bytes: u64,
    decision: Decision,
    clean: bool,
) -> String {
    let label = okapi_rules::group_label(language, browser, label);
    let size = size(language, bytes);
    let mark = decision_mark(language, decision, clean);
    let count = entries_text(language, entries);
    match language {
        Language::French => format!("{label} : {count}, {size}{mark}"),
        Language::English => format!("{label}: {count}, {size}{mark}"),
    }
}

/// `count` entries, singular or plural.
fn entries_text(language: Language, count: usize) -> String {
    match (language, count) {
        (Language::French, 0 | 1) => format!("{count} entrée"),
        (Language::French, _) => format!("{count} entrées"),
        (Language::English, 1) => format!("{count} entry"),
        (Language::English, _) => format!("{count} entries"),
    }
}

fn not_removed_text(
    language: Language,
    path: &Path,
    result: CleanResult,
    blocked: Option<&Path>,
) -> String {
    let separator = match language {
        Language::French => " : ",
        Language::English => ": ",
    };
    let mut line = format!(
        "  {}{separator}{}",
        shown(path),
        result_text(language, result)
    );
    if let Some(blocked) = blocked {
        let because = match language {
            Language::French => " ; à cause de ",
            Language::English => "; because of ",
        };
        line.push_str(because);
        line.push_str(&shown(blocked));
    }
    line
}

fn help(language: Language, settings: Option<&Path>, history: Option<&Path>) -> String {
    let default_minutes = okapi_engine::DEFAULT_MIN_AGE_MINUTES;
    let categories = okapi_engine::Category::ALL
        .map(|category| format!("--{}", category.name()))
        .join(" ");
    let unknown = match language {
        Language::French => "inconnu (ni XDG ni HOME absolu)",
        Language::English => "unknown (no absolute XDG nor HOME)",
    };
    let settings = settings.map_or_else(|| unknown.to_owned(), shown);
    let history = history.map_or_else(|| unknown.to_owned(), shown);
    match language {
        Language::French => format!(
            "okapi {VERSION} : trouve et nettoie ce que les applis laissent derrière elles.

Utilisation :
  okapi [options]      analyse ; avec --clean, nettoie aussi
  okapi licenses       licences d'Okapi et des éléments qu'il embarque

Options :
  --clean              nettoie les groupes présélectionnés (jamais les réinitialisations
                       de cache ni ce qui exige l'administrateur)
  --json               un document JSON sur la sortie standard
  {categories}
                       seulement ces catégories (toutes par défaut)
  --exclude CHEMIN     ne jamais toucher à ce chemin, ou à *.ext (répétable)
  --min-age MINUTES    âge minimal de ce qui est retiré ; l'emporte sur minAgeMinutes
                       des réglages (défaut : {default_minutes})
  -v, --verbose        liste chaque entrée ; le détail aussi sur la sortie d'erreur
  -q, --quiet          seulement le résultat
  -h, --help           cette aide
  -V, --version        la version

Réglages : {settings}
  (exclusions, ajoutées à --exclude ; minAgeMinutes ; deletionLog)
Historique des nettoyages : {history}

Codes de sortie : 0 succès, 1 échec, 2 arguments ou réglages invalides,
3 permission refusée, 4 nettoyage partiel, 5 rien trouvé."
        ),
        Language::English => format!(
            "okapi {VERSION}: finds and cleans what applications leave behind.

Usage:
  okapi [options]      scan; with --clean, clean too
  okapi licenses       licenses of Okapi and of the material it embeds

Options:
  --clean              clean the preselected groups (never cache resets nor what
                       needs the administrator)
  --json               one JSON document on standard output
  {categories}
                       only these categories (all by default)
  --exclude PATH       never touch this path, or *.ext (repeatable)
  --min-age MINUTES    minimum age of what is removed; wins over the settings'
                       minAgeMinutes (default: {default_minutes})
  -v, --verbose        list every entry; the detail on standard error too
  -q, --quiet          only the result
  -h, --help           this help
  -V, --version        the version

Settings: {settings}
  (exclusions, added to --exclude; minAgeMinutes; deletionLog)
Cleaning history: {history}

Exit codes: 0 success, 1 failure, 2 invalid arguments or settings,
3 permission denied, 4 partial cleaning, 5 nothing found."
        ),
    }
}

fn exclusion(language: Language, error: &ExclusionError) -> String {
    let french = language == Language::French;
    match error {
        ExclusionError::Empty => {
            if french {
                "Une exclusion ne peut pas être vide.".to_owned()
            } else {
                "An exclusion cannot be empty.".to_owned()
            }
        }
        ExclusionError::Invalid { path } => {
            if french {
                format!(
                    "Exclusion refusée : {} (un chemin absolu sans .., ou *.ext).",
                    shown(path)
                )
            } else {
                format!(
                    "Exclusion refused: {} (an absolute path without .., or *.ext).",
                    shown(path)
                )
            }
        }
        ExclusionError::Unresolvable { path, error } => {
            let error = error_text(language, *error, None);
            if french {
                format!("Exclusion introuvable : {} ({error}).", shown(path))
            } else {
                format!("Exclusion cannot be located: {} ({error}).", shown(path))
            }
        }
    }
}

fn variables(language: Language, error: &VariablesError) -> String {
    match error {
        VariablesError::NoHome => match language {
            Language::French => {
                "Dossier personnel introuvable : HOME n'est pas défini ou n'est pas un chemin absolu.".to_owned()
            }
            Language::English => {
                "Home directory not found: HOME is not set or not an absolute path.".to_owned()
            }
        },
        VariablesError::Invalid(error) => template_error(language, error),
    }
}

fn template_error(language: Language, error: &TemplateError) -> String {
    let french = language == Language::French;
    match error {
        TemplateError::MissingVariable { variable } => variable_missing(language, *variable),
        TemplateError::RelativeValue { variable, value }
        | TemplateError::UnsafeValue { variable, value } => {
            let (name, value) = (variable.name(), shown(value));
            if french {
                format!("Valeur inutilisable pour {name} : {value}.")
            } else {
                format!("Unusable value for {name}: {value}.")
            }
        }
        TemplateError::TempDirectoryContainsVariable {
            temp,
            variable,
            value,
        } => {
            let (temp, name, value) = (shown(temp), variable.name(), shown(value));
            if french {
                format!("TMPDIR ({temp}) contiendrait {name} ({value}) : il ne peut pas être vidé.")
            } else {
                format!("TMPDIR ({temp}) would hold {name} ({value}): it cannot be emptied.")
            }
        }
        TemplateError::NotAbsolute { path } => {
            if french {
                format!("Chemin non absolu : {}.", shown(path))
            } else {
                format!("Not an absolute path: {}.", shown(path))
            }
        }
    }
}

/// Why nothing was looked at or offered, or with `cleaning`, why nothing was
/// cleaned.
fn stop_text(language: Language, stop: &Stop, cleaning: bool) -> String {
    let french = language == Language::French;
    let prefix = match (french, cleaning) {
        (true, false) => "Rien n'a été regardé. ",
        (false, false) => "Nothing was looked at. ",
        (true, true) => "Rien n'a été nettoyé. ",
        (false, true) => "Nothing was cleaned. ",
    };
    let reason = match stop {
        Stop::UnsupportedSystem => {
            if french {
                "Ce système ne permet pas de reconnaître montages et remplacements.".to_owned()
            } else {
                "This system cannot tell mounts and replaced entries apart.".to_owned()
            }
        }
        Stop::MissingVariable { variable } => variable_missing(language, *variable),
        Stop::UnreadableVariable {
            variable,
            path,
            error,
        } => {
            let (name, path, error) = (
                variable.name(),
                shown(path),
                error_text(language, *error, None),
            );
            if french {
                format!("Le dossier de {name} ({path}) est illisible : {error}.")
            } else {
                format!("The directory of {name} ({path}) cannot be read: {error}.")
            }
        }
        Stop::MinAgeBeyondClock => {
            if french {
                "L'âge minimal remonte avant l'origine de l'horloge.".to_owned()
            } else {
                "The minimum age reaches before the clock's origin.".to_owned()
            }
        }
        // Something may have been looked at: what was found is not offered.
        Stop::Cancelled => {
            return if french {
                "Analyse annulée : rien n'est proposé.".to_owned()
            } else {
                "Scan cancelled: nothing is offered.".to_owned()
            };
        }
    };
    format!("{prefix}{reason}")
}

fn variable_missing(language: Language, variable: Variable) -> String {
    let name = variable.name();
    match language {
        Language::French => format!("La variable {name} n'a pas de valeur."),
        Language::English => format!("The variable {name} has no value."),
    }
}

/// A rule as the text names it: the label of a system target in the
/// language, the identifier of an application rule.
fn shown_rule(language: Language, origin: &Origin) -> String {
    match origin {
        Origin::SystemTarget { subcategory, .. } | Origin::SystemFile { subcategory, .. } => {
            okapi_rules::label(language, subcategory.as_str()).to_owned()
        }
        Origin::App { .. } => rule_name(origin),
        // The directory, as an application's identifier: its label is the
        // group's, not the rule's.
        Origin::Browser { browser, directory } => format!("{}/{directory}", browser.name()),
    }
}

fn origins_text(language: Language, origins: &[Origin]) -> String {
    origins
        .iter()
        .map(|origin| shown_rule(language, origin))
        .collect::<Vec<_>>()
        .join(", ")
}

fn issue_text(language: Language, issue: &ScanIssue) -> String {
    let french = language == Language::French;
    match issue {
        ScanIssue::Stopped(stop) => stop_text(language, stop, false),
        ScanIssue::AgeBeyondClock { origin } => {
            let origin = shown_rule(language, origin);
            if french {
                format!("{origin} : l'âge de la règle remonte avant l'origine de l'horloge.")
            } else {
                format!("{origin}: the rule's age reaches before the clock's origin.")
            }
        }
        ScanIssue::Unsupported { origin } => {
            let origin = shown_rule(language, origin);
            if french {
                format!("{origin} : règle pas encore prise en charge.")
            } else {
                format!("{origin}: rule not handled yet.")
            }
        }
        ScanIssue::Unresolvable { origin, error } => {
            format!(
                "{} : {}",
                shown_rule(language, origin),
                template_error(language, error)
            )
        }
        ScanIssue::UnreadableRoot {
            origins,
            path,
            error,
        } => {
            let (origins, path, error) = (
                origins_text(language, origins),
                shown(path),
                error_text(language, *error, None),
            );
            if french {
                format!("{origins} : {path} illisible ({error}).")
            } else {
                format!("{origins}: {path} cannot be read ({error}).")
            }
        }
        ScanIssue::UnsafeRoot {
            origins,
            path,
            reason,
        } => {
            let (origins, path) = (origins_text(language, origins), shown(path));
            let reason = unsafe_reason(language, *reason);
            if french {
                format!("{origins} : {path} écarté, {reason}.")
            } else {
                format!("{origins}: {path} left alone, {reason}.")
            }
        }
        ScanIssue::ProtectedRoot {
            origins,
            path,
            variable,
        } => {
            let (origins, path, name) = (
                origins_text(language, origins),
                shown(path),
                variable.name(),
            );
            if french {
                format!("{origins} : {path} contient le dossier de {name}, jamais touché.")
            } else {
                format!("{origins}: {path} holds the directory of {name}, never touched.")
            }
        }
        ScanIssue::Excluded { origins, path } => {
            let (origins, path) = (origins_text(language, origins), shown(path));
            if french {
                format!("{origins} : {path} exclu.")
            } else {
                format!("{origins}: {path} excluded.")
            }
        }
        ScanIssue::LimitReached {
            origins,
            root,
            limit,
        } => {
            let (origins, root) = (origins_text(language, origins), shown(root));
            let limit = match (limit, french) {
                (Limit::Depth, true) => "trop de niveaux de dossiers",
                (Limit::Depth, false) => "too many directory levels",
                (Limit::Entries, true) => "trop d'entrées",
                (Limit::Entries, false) => "too many entries",
            };
            if french {
                format!("{origins} : {root} parcouru en partie ({limit}).")
            } else {
                format!("{origins}: {root} only partly scanned ({limit}).")
            }
        }
    }
}

fn unsafe_reason(language: Language, reason: UnsafeReason) -> &'static str {
    let french = language == Language::French;
    match (reason, french) {
        (UnsafeReason::Replaced, true) => "remplacé pendant l'analyse",
        (UnsafeReason::Replaced, false) => "replaced during the scan",
        (UnsafeReason::Link, true) => "c'est un lien",
        (UnsafeReason::Link, false) => "it is a link",
        (UnsafeReason::NotADirectory, true) => "ce n'est pas un dossier",
        (UnsafeReason::NotADirectory, false) => "it is not a directory",
        (UnsafeReason::NotAFile, true) => "ce n'est pas un fichier",
        (UnsafeReason::NotAFile, false) => "it is not a file",
    }
}

fn decision_mark(language: Language, decision: Decision, clean: bool) -> String {
    let french = language == Language::French;
    let mark = match (decision, french) {
        (Decision::Selected, _) if !clean => return String::new(),
        (Decision::Selected, true) => "sélectionné",
        (Decision::Selected, false) => "selected",
        (Decision::NotPreselected, true) => "à choisir, jamais par défaut",
        (Decision::NotPreselected, false) => "to be chosen, never by default",
        (Decision::NeedsAdmin, true) => "exige l'administrateur",
        (Decision::NeedsAdmin, false) => "needs the administrator",
        // Such groups are only in the journal, never shown.
        (Decision::OutsideCategories, _) => return String::new(),
    };
    format!(" [{mark}]")
}

fn result_text(language: Language, result: CleanResult) -> String {
    let french = language == Language::French;
    match result {
        CleanResult::Removed { bytes } => {
            let size = size(language, bytes);
            if french {
                format!("retiré ({size})")
            } else {
                format!("removed ({size})")
            }
        }
        CleanResult::Partial { bytes, reason } => {
            let (size, reason) = (size(language, bytes), keep_reason(language, reason));
            if french {
                format!("retiré en partie ({size}), arrêté : {reason}")
            } else {
                format!("partly removed ({size}), stopped: {reason}")
            }
        }
        CleanResult::Gone => {
            if french {
                "déjà parti".to_owned()
            } else {
                "already gone".to_owned()
            }
        }
        CleanResult::Kept(reason) => {
            let reason = keep_reason(language, reason);
            if french {
                format!("gardé : {reason}")
            } else {
                format!("kept: {reason}")
            }
        }
    }
}

fn keep_reason(language: Language, reason: KeepReason) -> String {
    let french = language == Language::French;
    let text = match (reason, french) {
        (KeepReason::Failed { kind, code }, _) => return error_text(language, kind, code),
        (KeepReason::UnknownItem, true) => "identifiant inconnu (défaut d'Okapi)",
        (KeepReason::UnknownItem, false) => "unknown identifier (a defect of Okapi)",
        (KeepReason::Replaced, true) => "remplacé depuis l'analyse",
        (KeepReason::Replaced, false) => "replaced since the scan",
        (KeepReason::Recent, true) => "modifié trop récemment",
        (KeepReason::Recent, false) => "changed too recently",
        (KeepReason::Changed, true) => "son contenu a changé depuis l'analyse",
        (KeepReason::Changed, false) => "its contents changed since the scan",
        (KeepReason::Excluded, true) => "exclu",
        (KeepReason::Excluded, false) => "excluded",
        (KeepReason::Protected, true) => "protégé",
        (KeepReason::Protected, false) => "protected",
        (KeepReason::Cancelled, true) => "nettoyage annulé avant son tour",
        (KeepReason::Cancelled, false) => "cleaning cancelled before its turn",
        (KeepReason::BrowserOpen, true) => "le navigateur est ouvert",
        (KeepReason::BrowserOpen, false) => "the browser is open",
    };
    text.to_owned()
}

/// A system error: in words when it is a common one, by the name of its kind
/// otherwise; the system's own number follows when known.
fn error_text(language: Language, kind: io::ErrorKind, code: Option<i32>) -> String {
    let french = language == Language::French;
    let words = match (kind, french) {
        (io::ErrorKind::PermissionDenied, true) => Some("permission refusée"),
        (io::ErrorKind::PermissionDenied, false) => Some("permission denied"),
        (io::ErrorKind::NotFound, true) => Some("introuvable"),
        (io::ErrorKind::NotFound, false) => Some("not found"),
        (io::ErrorKind::ReadOnlyFilesystem, true) => Some("système de fichiers en lecture seule"),
        (io::ErrorKind::ReadOnlyFilesystem, false) => Some("read-only filesystem"),
        (io::ErrorKind::ResourceBusy, true) => Some("en cours d'utilisation"),
        (io::ErrorKind::ResourceBusy, false) => Some("in use"),
        (io::ErrorKind::Unsupported, true) => Some("non pris en charge"),
        (io::ErrorKind::Unsupported, false) => Some("not supported"),
        _ => None,
    };
    let text = words.map_or_else(
        || {
            if french {
                format!("erreur système {kind:?}")
            } else {
                format!("system error {kind:?}")
            }
        },
        str::to_owned,
    );
    match code {
        Some(code) => format!("{text} ({code})"),
        None => text,
    }
}

/// `bytes` with a decimal unit: `o`, `ko`... in French (decimal comma), `B`,
/// `kB`... in English.
fn size(language: Language, bytes: u64) -> String {
    let size = ByteSize::of(bytes);
    let unit = match (language, size.unit()) {
        (Language::French, SizeUnit::Byte) => "o",
        (Language::French, SizeUnit::Kilobyte) => "ko",
        (Language::French, SizeUnit::Megabyte) => "Mo",
        (Language::French, SizeUnit::Gigabyte) => "Go",
        (Language::French, SizeUnit::Terabyte) => "To",
        (Language::English, SizeUnit::Byte) => "B",
        (Language::English, SizeUnit::Kilobyte) => "kB",
        (Language::English, SizeUnit::Megabyte) => "MB",
        (Language::English, SizeUnit::Gigabyte) => "GB",
        (Language::English, SizeUnit::Terabyte) => "TB",
    };
    let Some(tenths) = size.tenths() else {
        return format!("{} {unit}", size.whole());
    };
    let separator = match language {
        Language::French => ',',
        Language::English => '.',
    };
    format!("{}{separator}{tenths} {unit}", size.whole())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn every_issue_names_a_system_rule_in_the_language() {
        let target = Origin::SystemTarget {
            subcategory: okapi_rules::Label::try_from("APT Logs".to_owned()).unwrap(),
            path: okapi_rules::TemplatePath::try_from("/var/log/apt".to_owned()).unwrap(),
        };
        let app = Origin::App {
            file: okapi_engine::AppFile::Apps,
            id: okapi_rules::RuleId::try_from("pip".to_owned()).unwrap(),
        };
        let path = PathBuf::from("/var/log/apt");
        let origins = vec![target.clone(), app];
        let one = [
            ScanIssue::AgeBeyondClock {
                origin: target.clone(),
            },
            ScanIssue::Unsupported {
                origin: target.clone(),
            },
            ScanIssue::Unresolvable {
                origin: target,
                error: TemplateError::MissingVariable {
                    variable: Variable::Home,
                },
            },
        ];
        let several = [
            ScanIssue::UnreadableRoot {
                origins: origins.clone(),
                path: path.clone(),
                error: io::ErrorKind::PermissionDenied,
            },
            ScanIssue::UnsafeRoot {
                origins: origins.clone(),
                path: path.clone(),
                reason: UnsafeReason::Link,
            },
            ScanIssue::ProtectedRoot {
                origins: origins.clone(),
                path: path.clone(),
                variable: Variable::Home,
            },
            ScanIssue::Excluded {
                origins: origins.clone(),
                path: path.clone(),
            },
            ScanIssue::LimitReached {
                origins,
                root: path,
                limit: Limit::Depth,
            },
        ];
        for (issues, french, english) in [
            (&one[..], "Journaux d'APT ", "APT Logs"),
            (&several[..], "Journaux d'APT, pip ", "APT Logs, pip"),
        ] {
            for issue in issues {
                let (in_french, in_english) = (
                    issue_text(Language::French, issue),
                    issue_text(Language::English, issue),
                );
                assert!(in_french.starts_with(french), "{in_french}");
                assert!(in_english.starts_with(english), "{in_english}");
            }
        }
    }

    #[test]
    fn a_rule_is_named_by_its_label_or_its_identifier() {
        let label = |text: &str| okapi_rules::Label::try_from(text.to_owned()).unwrap();
        let path = |text: &str| okapi_rules::TemplatePath::try_from(text.to_owned()).unwrap();
        let target = Origin::SystemTarget {
            subcategory: label("APT Logs"),
            path: path("/var/log/apt"),
        };
        let file = Origin::SystemFile {
            subcategory: label("pip Cache"),
            path: path("/var/log/x"),
        };
        let app = Origin::App {
            file: okapi_engine::AppFile::Apps,
            id: okapi_rules::RuleId::try_from("pip".to_owned()).unwrap(),
        };
        assert_eq!(shown_rule(Language::French, &file), "Cache de pip");
        assert_eq!(shown_rule(Language::French, &app), "pip", "its identifier");
        assert_eq!(shown_rule(Language::English, &target), "APT Logs");
        let browser = Origin::Browser {
            browser: ChromiumBrowserKind::OperaGx,
            directory: okapi_rules::RelativePath::try_from("GPUCache".to_owned()).unwrap(),
        };
        assert_eq!(
            shown_rule(Language::French, &browser),
            "Opera GX/GPUCache",
            "its directory, not its label"
        );
    }

    #[test]
    fn sizes_use_the_language_units() {
        assert_eq!(size(Language::French, 999), "999 o");
        assert_eq!(size(Language::French, 1_500), "1,5 ko");
        assert_eq!(size(Language::English, 18_400_000_000), "18.4 GB");
        assert_eq!(size(Language::English, 0), "0 B");
        assert_eq!(size(Language::French, u64::MAX), "18446744,0 To");
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one sample of every message: a table, not logic"
    )]
    fn no_text_holds_an_em_dash_nor_is_empty() {
        let path = PathBuf::from("/data/x");
        let error = io::Error::from(io::ErrorKind::PermissionDenied);
        let exclusion = ExclusionError::Invalid { path: path.clone() };
        let variables = VariablesError::NoHome;
        let stop = Stop::MinAgeBeyondClock;
        let unreadable = StoreError::Unreadable {
            path: path.clone(),
            error: io::Error::from(io::ErrorKind::PermissionDenied),
        };
        let unwritable = StoreError::Unwritable {
            path: path.clone(),
            error: io::Error::from(io::ErrorKind::StorageFull),
        };
        let invalid = StoreError::Invalid {
            path: path.clone(),
            error: serde_json::from_str::<u8>("x").unwrap_err(),
        };
        let issue = ScanIssue::Excluded {
            origins: Vec::new(),
            path: path.clone(),
        };
        let messages = [
            Message::Help {
                settings: Some(&path),
                history: None,
            },
            Message::Version,
            Message::Argument(&ArgumentError::Unknown("--x".to_owned())),
            Message::Argument(&ArgumentError::MissingValue("--exclude")),
            Message::Argument(&ArgumentError::InvalidMinAge("x".to_owned())),
            Message::Argument(&ArgumentError::VerboseAndQuiet),
            Message::Exclusion(&exclusion),
            Message::ExclusionMissing(&path),
            Message::InternalDefect,
            Message::Store(&StoreError::UnsupportedVersion {
                path: path.clone(),
                version: 2,
                supported: 1,
            }),
            Message::Store(&unreadable),
            Message::Store(&StoreError::BrokenLink { path: path.clone() }),
            Message::Store(&StoreError::NotAFile { path: path.clone() }),
            Message::Store(&StoreError::TooLarge {
                path: path.clone(),
                limit: 10,
            }),
            Message::Store(&invalid),
            Message::HistoryNotRecorded(&unwritable),
            Message::SettingsExclusionMissing(&path, &path),
            Message::SettingsExclusion(&path, &exclusion),
            Message::SettingsMinAge(&path, 1),
            Message::DeletionLogUnavailable,
            Message::SettingsDirectoryUnknown,
            Message::HistoryDirectoryUnknown,
            Message::HistoryContinues,
            Message::Variables(&variables),
            Message::RulesUnavailable,
            Message::RunningAsRoot,
            Message::Stop(&stop),
            Message::CleaningStopped(&stop),
            Message::Stop(&Stop::Cancelled),
            Message::NotRemoved {
                path: &path,
                result: CleanResult::Kept(KeepReason::Cancelled),
                blocked: None,
            },
            Message::JournalRotationFailed(&error),
            Message::Issue(&issue),
            Message::Group {
                browser: Some(ChromiumBrowserKind::Chrome),
                label: "pip Cache",
                entries: 1,
                bytes: 10,
                decision: Decision::NotPreselected,
                clean: true,
            },
            Message::Entry {
                path: &path,
                bytes: 10,
            },
            Message::Found {
                entries: 2,
                bytes: 20,
            },
            Message::NothingFound,
            Message::NothingToClean,
            Message::Cleaned {
                bytes: 10,
                entries: 1,
            },
            Message::NotRemoved {
                path: &path,
                result: CleanResult::Kept(KeepReason::Recent),
                blocked: Some(&path),
            },
            Message::Journal(&path),
            Message::JournalUnavailable(&path, &error),
            Message::StateDirectoryUnknown,
            Message::JournalWriteFailed(&error),
            Message::OutputFailed(&error),
            Message::Licenses,
        ];
        for language in [Language::French, Language::English] {
            for message in &messages {
                let text = text(language, message);
                assert!(!text.is_empty(), "{message:?}");
                assert!(
                    !text.contains(['\u{2014}', '\u{2013}']),
                    "{message:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn a_cancelled_scan_says_so_not_that_nothing_was_looked_at() {
        assert_eq!(
            text(Language::French, &Message::Stop(&Stop::Cancelled)),
            "Analyse annulée : rien n'est proposé."
        );
        assert_eq!(
            text(Language::English, &Message::Stop(&Stop::Cancelled)),
            "Scan cancelled: nothing is offered."
        );
        assert_eq!(
            keep_reason(Language::French, KeepReason::Cancelled),
            "nettoyage annulé avant son tour"
        );
        assert_eq!(
            keep_reason(Language::English, KeepReason::Cancelled),
            "cleaning cancelled before its turn"
        );
        assert_eq!(
            keep_reason(Language::French, KeepReason::BrowserOpen),
            "le navigateur est ouvert"
        );
        assert_eq!(
            keep_reason(Language::English, KeepReason::BrowserOpen),
            "the browser is open"
        );
    }

    #[test]
    fn invalid_settings_say_their_position_once() {
        let invalid = StoreError::Invalid {
            path: PathBuf::from("/c/settings.json"),
            error: serde_json::from_str::<u8>("x").unwrap_err(),
        };
        assert_eq!(
            text(Language::English, &Message::Store(&invalid)),
            "/c/settings.json is invalid (line 1, column 1: expected value): nothing is done."
        );
    }

    #[test]
    fn errors_say_their_kind_and_number() {
        assert_eq!(
            error_text(Language::French, io::ErrorKind::PermissionDenied, Some(13)),
            "permission refusée (13)"
        );
        assert_eq!(
            error_text(Language::English, io::ErrorKind::StorageFull, Some(28)),
            "system error StorageFull (28)"
        );
    }
}

#[cfg(test)]
mod shown_tests {
    use super::*;

    #[test]
    fn control_characters_in_a_name_never_reach_the_terminal() {
        let planted = Path::new("/tmp/\u{1b}]52;c;cGFzcw==\u{7}\nCleaned: 1 GB");
        let text = text(
            Language::English,
            &Message::Entry {
                path: planted,
                bytes: 1,
            },
        );
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert!(text.contains("\\u{1b}") && text.contains("\\n"), "{text}");
    }
}
