use std::io;
use std::path::{Path, PathBuf};

use okapi_rules::Language;

use super::capped::read_capped;

/// Far beyond any `.desktop` file.
const DESKTOP_LIMIT: usize = 1024 * 1024;
/// The group that describes the application (its actions have their own
/// names).
const ENTRY_GROUP: &str = "[Desktop Entry]";

/// The name of the application whose `.desktop` identifier is `identifier`
/// (`google-chrome`), from the first folder of applications whose file
/// gives one, in `language` when it has it, else its plain `Name`. `None`
/// when no file gives one.
///
/// # Errors
///
/// The last error met reading a file that exists, when none gave a name
/// (the system's, or [`io::ErrorKind::InvalidData`]).
pub fn application_name(identifier: &str, language: Language) -> io::Result<Option<String>> {
    application_name_in(&data_directories(), identifier, names_in(language))
}

/// The `Name[...]` keys of `language`, the most precise first.
const fn names_in(language: Language) -> &'static [&'static str] {
    match language {
        Language::French => &["fr_FR", "fr"],
        // The plain `Name` is in English.
        Language::English => &[],
    }
}

fn application_name_in(
    directories: &[PathBuf],
    identifier: &str,
    languages: &[&str],
) -> io::Result<Option<String>> {
    if !super::names_a_file(identifier) {
        return Ok(None);
    }
    let mut failure = None;
    for directory in directories {
        match desktop_name(&directory.join(format!("{identifier}.desktop")), languages) {
            Ok(Some(name)) => return Ok(Some(name)),
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => failure = Some(error),
        }
    }
    failure.map_or(Ok(None), Err)
}

/// The name a `.desktop` file gives its application.
fn desktop_name(path: &Path, languages: &[&str]) -> io::Result<Option<String>> {
    read_capped(path, DESKTOP_LIMIT).map(|text| parse_desktop_name(&text, languages))
}

pub(crate) fn parse_desktop_name(text: &str, languages: &[&str]) -> Option<String> {
    parse_desktop_entry(text, languages).name
}

/// What a `.desktop` file says of its application, read as data: its
/// command (`Exec`) is never read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    /// In the language asked when the file has it, else its plain `Name`.
    pub name: Option<String>,
    /// Its plain `Name`, in no language.
    pub plain_name: Option<String>,
    /// `StartupWMClass`: the class its windows say.
    pub wm_class: Option<String>,
    /// `X-Flatpak-RenamedFrom`: the launchers' names it had before being
    /// renamed (`<old-id>.desktop`), as flatpak writes them.
    pub renamed_from: Vec<String>,
    /// A name in the icon theme, or a path.
    pub icon: Option<String>,
    /// `Type=Application`.
    pub application: bool,
    pub no_display: bool,
    pub hidden: bool,
    pub only_show_in: Vec<String>,
    pub not_show_in: Vec<String>,
}

impl DesktopEntry {
    /// Whether the menu of a session on `desktops` (`XDG_CURRENT_DESKTOP`)
    /// shows it: an application, neither hidden nor kept from the menu,
    /// meant for one of them if it names any, for none it refuses.
    #[must_use]
    pub fn shown_in(&self, desktops: &[String]) -> bool {
        let names = |list: &[String]| list.iter().any(|desktop| desktops.contains(desktop));
        self.application
            && !self.no_display
            && !self.hidden
            && (self.only_show_in.is_empty() || names(&self.only_show_in))
            && !names(&self.not_show_in)
    }
}

/// What the `.desktop` file at `path` says of its application, its name in
/// `language` when it has it.
///
/// # Errors
///
/// The system's error, or [`io::ErrorKind::InvalidData`] when the file is
/// not UTF-8 or far too long for one.
pub fn desktop_entry(path: &Path, language: Language) -> io::Result<DesktopEntry> {
    read_capped(path, DESKTOP_LIMIT).map(|text| parse_desktop_entry(&text, names_in(language)))
}

pub(crate) fn parse_desktop_entry(text: &str, languages: &[&str]) -> DesktopEntry {
    let mut in_entry = false;
    let mut plain = None;
    let mut localized: Vec<Option<&str>> = vec![None; languages.len()];
    let mut entry = DesktopEntry::default();
    let list = |value: &str| {
        value
            .split(';')
            .filter(|desktop| !desktop.is_empty())
            .map(str::to_owned)
            .collect()
    };
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            in_entry = line == ENTRY_GROUP;
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" => plain = Some(value),
            "StartupWMClass" => {
                entry.wm_class = Some(value.to_owned()).filter(|class| !class.is_empty());
            }
            "X-Flatpak-RenamedFrom" => entry.renamed_from = list(value),
            "Icon" => entry.icon = Some(value.to_owned()).filter(|icon| !icon.is_empty()),
            "Type" => entry.application = value == "Application",
            "NoDisplay" => entry.no_display = value == "true",
            "Hidden" => entry.hidden = value == "true",
            "OnlyShowIn" => entry.only_show_in = list(value),
            "NotShowIn" => entry.not_show_in = list(value),
            _ => {
                if let Some(language) = key
                    .strip_prefix("Name[")
                    .and_then(|rest| rest.strip_suffix(']'))
                    && let Some(rank) = languages.iter().position(|wanted| *wanted == language)
                {
                    localized[rank] = Some(value);
                }
            }
        }
    }
    entry.name = localized
        .into_iter()
        .flatten()
        .chain(plain)
        .find(|name| !name.is_empty())
        .map(str::to_owned);
    entry.plain_name = plain.filter(|name| !name.is_empty()).map(str::to_owned);
    entry
}

/// Where `.desktop` files are looked for, most personal first: the
/// `applications` folder of `$XDG_DATA_HOME` (`~/.local/share`), then of each
/// of `$XDG_DATA_DIRS` (`/usr/local/share:/usr/share`). Empty where unknown.
fn data_directories() -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        crate::variables::data_directories()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The start of `google-chrome.desktop`: its actions name themselves in
    /// French, the application does not.
    const CHROME: &str = "\
[Desktop Entry]
Version=1.0
Name=Google Chrome
GenericName=Web Browser
GenericName[fr]=Navigateur Web
Exec=/usr/bin/google-chrome-stable %U

[Desktop Action new-window]
Name=New Window
Name[fr]=Nouvelle fenêtre

[Desktop Action new-private-window]
Name=New Incognito Window
Name[fr]=Nouvelle fenêtre de navigation privée
";

    #[test]
    fn the_name_is_the_application_s_never_an_action_s() {
        assert_eq!(
            parse_desktop_name(CHROME, &["fr_FR", "fr"]).as_deref(),
            Some("Google Chrome")
        );
    }

    #[test]
    fn the_first_language_asked_wins_then_the_plain_name() {
        let text =
            "[Desktop Entry]\nName=Files\nName[fr]=Fichiers\nName[fr_FR]=Fichiers (France)\n";
        assert_eq!(
            parse_desktop_name(text, &["fr_FR", "fr"]).as_deref(),
            Some("Fichiers (France)")
        );
        assert_eq!(
            parse_desktop_name(text, &["fr"]).as_deref(),
            Some("Fichiers")
        );
        assert_eq!(parse_desktop_name(text, &["de"]).as_deref(), Some("Files"));
        assert_eq!(
            parse_desktop_name("[Desktop Entry]\nType=Application\n", &[]),
            None
        );
    }

    #[test]
    fn an_entry_says_its_icon_and_whether_the_menu_shows_it() {
        let gnome = ["ubuntu".to_owned(), "GNOME".to_owned()];
        let entry = parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nName=Files\nIcon=org.gnome.Nautilus\n\
             Exec=nautilus\n\n[Desktop Action new]\nIcon=other\nNoDisplay=true\n",
            &[],
        );
        assert_eq!(entry.name.as_deref(), Some("Files"));
        assert_eq!(
            entry.icon.as_deref(),
            Some("org.gnome.Nautilus"),
            "never an action's"
        );
        assert!(
            entry.shown_in(&gnome),
            "an action's NoDisplay is not its own"
        );
        for hiding in [
            "Type=Application\nNoDisplay=true",
            "Type=Application\nHidden=true",
            "Type=Link",
            "Name=No type",
            "Type=Application\nOnlyShowIn=KDE;XFCE;",
            "Type=Application\nNotShowIn=GNOME;",
        ] {
            let entry = parse_desktop_entry(&format!("[Desktop Entry]\n{hiding}\n"), &[]);
            assert!(!entry.shown_in(&gnome), "{hiding}");
        }
        let only_gnome = parse_desktop_entry(
            "[Desktop Entry]\nType=Application\nOnlyShowIn=GNOME;\n",
            &[],
        );
        assert!(only_gnome.shown_in(&gnome));
        assert!(!only_gnome.shown_in(&[]), "a session that names no desktop");
        let empty_icon = parse_desktop_entry("[Desktop Entry]\nIcon=\n", &[]);
        assert_eq!(empty_icon.icon, None);
    }

    #[test]
    fn an_entry_says_its_plain_name_its_window_class_and_its_former_names() {
        // Catches an action's keys taken for the application's, and a
        // translated name taken for the plain one.
        let entry = parse_desktop_entry(
            "[Desktop Entry]\nName=Text Editor\nName[fr]=Éditeur de texte\n\
             StartupWMClass=gnome-text-editor\n\
             X-Flatpak-RenamedFrom=org.gnome.gedit.desktop;gedit.desktop;\n\n\
             [Desktop Action new]\nName=New\nStartupWMClass=other\nX-Flatpak-RenamedFrom=x.desktop;\n",
            &["fr"],
        );
        assert_eq!(entry.name.as_deref(), Some("Éditeur de texte"));
        assert_eq!(entry.plain_name.as_deref(), Some("Text Editor"));
        assert_eq!(entry.wm_class.as_deref(), Some("gnome-text-editor"));
        assert_eq!(
            entry.renamed_from,
            ["org.gnome.gedit.desktop", "gedit.desktop"]
        );
    }

    #[test]
    fn the_first_folder_whose_file_names_the_application_wins() {
        let root = tempfile::tempdir().unwrap();
        let personal = root.path().join("personal");
        let shared = root.path().join("shared");
        let system = root.path().join("system");
        for folder in [&personal, &shared, &system] {
            std::fs::create_dir(folder).unwrap();
        }
        std::fs::write(
            shared.join("app.desktop"),
            "[Desktop Entry]\nType=Application\n",
        )
        .unwrap();
        std::fs::write(system.join("app.desktop"), "[Desktop Entry]\nName=App\n").unwrap();
        std::fs::write(
            personal.join("other.desktop"),
            "[Desktop Entry]\nName=Mine\n",
        )
        .unwrap();
        let folders = [personal.clone(), shared, system.clone()];
        assert_eq!(
            application_name_in(&folders, "app", &[])
                .unwrap()
                .as_deref(),
            Some("App"),
            "none in the first, no name in the second"
        );
        assert_eq!(
            application_name_in(&folders, "other", &[])
                .unwrap()
                .as_deref(),
            Some("Mine")
        );
        assert_eq!(application_name_in(&folders, "none", &[]).unwrap(), None);
        std::fs::create_dir(personal.join("broken.desktop")).unwrap();
        assert!(application_name_in(&folders, "broken", &[]).is_err());
        std::fs::write(
            system.join("broken.desktop"),
            "[Desktop Entry]\nName=Fixed\n",
        )
        .unwrap();
        assert_eq!(
            application_name_in(&folders, "broken", &[])
                .unwrap()
                .as_deref(),
            Some("Fixed"),
            "an error before a name is no error"
        );
        std::fs::write(
            personal.join("both.desktop"),
            "[Desktop Entry]\nName=Mine\n",
        )
        .unwrap();
        std::fs::write(
            system.join("both.desktop"),
            "[Desktop Entry]\nName=System\n",
        )
        .unwrap();
        assert_eq!(
            application_name_in(&folders, "both", &[])
                .unwrap()
                .as_deref(),
            Some("Mine"),
            "the user's own file overrides the system's"
        );
        std::fs::write(
            root.path().join("outside.desktop"),
            "[Desktop Entry]\nName=Out\n",
        )
        .unwrap();
        assert_eq!(
            application_name_in(&folders, "../outside", &[]).unwrap(),
            None
        );
    }
}
