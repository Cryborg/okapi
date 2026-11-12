//! Every text the command line shows, in French and English. The labels of
//! the rules are shown as the rules write them, in English, until they are
//! translated.

use std::ffi::OsString;
use std::io;
use std::path::Path;

use okapi_engine::{
    CleanResult, ExclusionError, KeepReason, Limit, Origin, ScanIssue, Stop, UnsafeReason,
};
use okapi_platform::VariablesError;
use okapi_rules::{TemplateError, Variable};

use crate::args::{ArgumentError, SECONDS_PER_MINUTE};
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

/// The language of the texts shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    French,
    English,
}

/// The language of the session: `LC_ALL`, then `LC_MESSAGES`, then `LANG`,
/// an empty value being ignored; French when its language code is `fr`,
/// English otherwise. `LANGUAGE` is not read.
pub fn language(read: impl Fn(&str) -> Option<OsString>) -> Language {
    let chosen = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .filter_map(read)
        .find(|value| !value.is_empty());
    // `fr_FR.UTF-8`, `fr.UTF-8`, `fr`: the language code comes first.
    let french = chosen.is_some_and(|value| {
        value
            .to_string_lossy()
            .split(['_', '.', '@'])
            .next()
            .is_some_and(|code| code == "fr")
    });
    if french {
        Language::French
    } else {
        Language::English
    }
}

/// Something the command line says.
#[derive(Debug)]
pub enum Message<'a> {
    Help,
    Version,
    Argument(&'a ArgumentError),
    Exclusion(&'a ExclusionError),
    ExclusionMissing(&'a Path),
    InternalDefect,
    Variables(&'a VariablesError),
    RulesUnavailable,
    RunningAsRoot,
    Stop(&'a Stop),
    CleaningStopped(&'a Stop),
    JournalRotationFailed(&'a io::Error),
    Issue(&'a ScanIssue),
    Group {
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
pub fn text(language: Language, message: &Message<'_>) -> String {
    let french = language == Language::French;
    let pick = |fr: String, en: String| if french { fr } else { en };
    match message {
        Message::Help => help(language),
        Message::Version => format!("okapi {VERSION}"),
        Message::Argument(error) => argument_text(language, error),
        Message::Exclusion(error) => exclusion(language, error),
        Message::ExclusionMissing(path) => pick(
            format!("Attention : l'exclusion {} n'existe pas telle quelle (faute de frappe ?).", shown(path)),
            format!("Warning: the exclusion {} does not exist as written (a typing mistake?).", shown(path)),
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
            label,
            entries,
            bytes,
            decision,
            clean,
        } => group_text(language, label, *entries, *bytes, *decision, *clean),
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
            "Journal indisponible : dossier d'état inconnu (ni XDG_STATE_HOME ni HOME). Okapi continue sans.".to_owned(),
            "Journal unavailable: no state directory (neither XDG_STATE_HOME nor HOME). Running without it.".to_owned(),
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
    label: &str,
    entries: usize,
    bytes: u64,
    decision: Decision,
    clean: bool,
) -> String {
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

fn help(language: Language) -> String {
    let default_minutes = okapi_engine::DEFAULT_MIN_AGE.as_secs() / SECONDS_PER_MINUTE;
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
  --system --apps --gaming --gpu
                       seulement ces catégories (toutes par défaut)
  --exclude CHEMIN     ne jamais toucher à ce chemin, ou à *.ext (répétable)
  --min-age MINUTES    âge minimal de ce qui est retiré (défaut : {default_minutes})
  -v, --verbose        liste chaque entrée ; le détail aussi sur la sortie d'erreur
  -q, --quiet          seulement le résultat
  -h, --help           cette aide
  -V, --version        la version

Codes de sortie : 0 succès, 1 échec, 2 arguments invalides, 3 permission refusée,
4 nettoyage partiel, 5 rien trouvé."
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
  --system --apps --gaming --gpu
                       only these categories (all by default)
  --exclude PATH       never touch this path, or *.ext (repeatable)
  --min-age MINUTES    minimum age of what is removed (default: {default_minutes})
  -v, --verbose        list every entry; the detail on standard error too
  -q, --quiet          only the result
  -h, --help           this help
  -V, --version        the version

Exit codes: 0 success, 1 failure, 2 invalid arguments, 3 permission denied,
4 partial cleaning, 5 nothing found."
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
            Language::French => "Dossier personnel introuvable : HOME n'est pas défini.".to_owned(),
            Language::English => "Home directory not found: HOME is not set.".to_owned(),
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

/// Why nothing was looked at, or with `cleaning`, why nothing was cleaned.
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

fn origins_text(origins: &[Origin]) -> String {
    origins.iter().map(rule_name).collect::<Vec<_>>().join(", ")
}

fn issue_text(language: Language, issue: &ScanIssue) -> String {
    let french = language == Language::French;
    match issue {
        ScanIssue::Stopped(stop) => stop_text(language, stop, false),
        ScanIssue::AgeBeyondClock { origin } => {
            let origin = rule_name(origin);
            if french {
                format!("{origin} : l'âge de la règle remonte avant l'origine de l'horloge.")
            } else {
                format!("{origin}: the rule's age reaches before the clock's origin.")
            }
        }
        ScanIssue::Unsupported { origin } => {
            let origin = rule_name(origin);
            if french {
                format!("{origin} : règle pas encore prise en charge.")
            } else {
                format!("{origin}: rule not handled yet.")
            }
        }
        ScanIssue::Unresolvable { origin, error } => {
            format!(
                "{} : {}",
                rule_name(origin),
                template_error(language, error)
            )
        }
        ScanIssue::UnreadableRoot {
            origins,
            path,
            error,
        } => {
            let (origins, path, error) = (
                origins_text(origins),
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
            let (origins, path) = (origins_text(origins), shown(path));
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
            let (origins, path, name) = (origins_text(origins), shown(path), variable.name());
            if french {
                format!("{origins} : {path} contient le dossier de {name}, jamais touché.")
            } else {
                format!("{origins}: {path} holds the directory of {name}, never touched.")
            }
        }
        ScanIssue::Excluded { origins, path } => {
            let (origins, path) = (origins_text(origins), shown(path));
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
            let (origins, root) = (origins_text(origins), shown(root));
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
    const STEP: u64 = 1000;
    let units: [&str; 5] = match language {
        Language::French => ["o", "ko", "Mo", "Go", "To"],
        Language::English => ["B", "kB", "MB", "GB", "TB"],
    };
    let mut unit = 0;
    let mut scaled = bytes;
    let mut tenths = 0;
    while scaled >= STEP && unit + 1 < units.len() {
        tenths = (scaled % STEP) / (STEP / 10);
        scaled /= STEP;
        unit += 1;
    }
    if unit == 0 {
        return format!("{scaled} {}", units[0]);
    }
    let separator = match language {
        Language::French => ',',
        Language::English => '.',
    };
    format!("{scaled}{separator}{tenths} {}", units[unit])
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn session(
        values: &'static [(&'static str, &'static str)],
    ) -> impl Fn(&str) -> Option<OsString> {
        move |name| {
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    #[test]
    fn the_language_follows_the_locale_variables_in_order() {
        assert_eq!(language(session(&[])), Language::English);
        assert_eq!(
            language(session(&[("LANG", "fr_FR.UTF-8")])),
            Language::French
        );
        assert_eq!(
            language(session(&[
                ("LANG", "fr_FR.UTF-8"),
                ("LC_MESSAGES", "en_GB.UTF-8")
            ])),
            Language::English
        );
        assert_eq!(
            language(session(&[
                ("LC_ALL", "fr_CA.UTF-8"),
                ("LC_MESSAGES", "en_GB.UTF-8")
            ])),
            Language::French
        );
        assert_eq!(
            language(session(&[("LC_ALL", ""), ("LANG", "fr_FR.UTF-8")])),
            Language::French,
            "an empty value is ignored"
        );
        assert_eq!(language(session(&[("LANG", "C")])), Language::English);
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
    fn no_text_holds_an_em_dash_nor_is_empty() {
        let path = PathBuf::from("/data/x");
        let error = io::Error::from(io::ErrorKind::PermissionDenied);
        let exclusion = ExclusionError::Invalid { path: path.clone() };
        let variables = VariablesError::NoHome;
        let stop = Stop::MinAgeBeyondClock;
        let issue = ScanIssue::Excluded {
            origins: Vec::new(),
            path: path.clone(),
        };
        let messages = [
            Message::Help,
            Message::Version,
            Message::Argument(&ArgumentError::Unknown("--x".to_owned())),
            Message::Argument(&ArgumentError::MissingValue("--exclude")),
            Message::Argument(&ArgumentError::InvalidMinAge("x".to_owned())),
            Message::Argument(&ArgumentError::VerboseAndQuiet),
            Message::Exclusion(&exclusion),
            Message::ExclusionMissing(&path),
            Message::InternalDefect,
            Message::Variables(&variables),
            Message::RulesUnavailable,
            Message::RunningAsRoot,
            Message::Stop(&stop),
            Message::CleaningStopped(&stop),
            Message::JournalRotationFailed(&error),
            Message::Issue(&issue),
            Message::Group {
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
