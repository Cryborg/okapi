//! The applications Okapi saw installed, one card each: who left a folder
//! once its application is gone, and what the screen names it. It is state
//! Okapi writes after a whole reading of what is installed; the user never
//! edits it. Its size is bounded by what it holds, the oldest cards going
//! first, so that it always stays readable.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{StoreError, file};

const SEEN_FILE: &str = "seen-apps.json";
/// The format this Okapi reads.
const SEEN_VERSION: u32 = 1;
/// The longest name kept, in bytes: a longer one names no folder.
pub const MOST_NAME_BYTES: usize = 128;
/// The most names kept for one application.
pub const MOST_NAMES: usize = 8;
/// What the file's envelope (its version) takes beside the cards.
const ENVELOPE_ROOM: usize = 1024;

/// One application seen installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SeenApp {
    /// `deb`, `snap` or `flatpak`.
    pub source: String,
    /// Its identifier in its source.
    pub id: String,
    /// As the menu showed it.
    pub shown: String,
    /// The names it went by: its package, its id, its launcher's name and
    /// file, its windows' class.
    pub names: Vec<String>,
    /// The day it was last seen, in days since the clock's origin.
    pub day: i64,
}

/// The applications seen. A missing file means none yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SeenApps {
    apps: Vec<SeenApp>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSeenApps {
    #[expect(dead_code, reason = "checked by `file::check_version` first")]
    version: u32,
    apps: Vec<SeenApp>,
}

/// `text`, if it may be kept as a name: not empty, not too long.
fn kept(text: &str) -> Option<String> {
    (!text.is_empty() && text.len() <= MOST_NAME_BYTES).then(|| text.to_owned())
}

impl SeenApps {
    /// The file the applications seen of `directory` live in.
    #[must_use]
    pub fn path(directory: &Path) -> PathBuf {
        directory.join(SEEN_FILE)
    }

    #[must_use]
    pub fn apps(&self) -> &[SeenApp] {
        &self.apps
    }

    /// The card of the application `id` of `source`, if it was seen.
    #[must_use]
    pub fn find(&self, source: &str, id: &str) -> Option<&SeenApp> {
        self.apps
            .iter()
            .find(|app| app.source == source && app.id == id)
    }

    /// Reads the applications seen kept in `directory`; none when there is
    /// no file.
    ///
    /// # Errors
    ///
    /// [`StoreError::Unreadable`], [`StoreError::BrokenLink`],
    /// [`StoreError::NotAFile`], [`StoreError::TooLarge`],
    /// [`StoreError::UnsupportedVersion`] or [`StoreError::Invalid`].
    pub fn load(directory: &Path) -> Result<Self, StoreError> {
        let path = Self::path(directory);
        let Some(raw) = file::read_versioned::<RawSeenApps>(&path, SEEN_VERSION)? else {
            tracing::info!(path = %path.display(), "no application seen yet");
            return Ok(Self::default());
        };
        tracing::info!(path = %path.display(), apps = raw.apps.len(), "applications seen loaded");
        Ok(Self { apps: raw.apps })
    }

    /// Adds the applications `seen`, or refreshes their cards: their names
    /// joined (each kept if short enough, [`MOST_NAMES`] at most), the day
    /// moved forward. Whether anything changed.
    pub fn remember(&mut self, seen: impl IntoIterator<Item = SeenApp>) -> bool {
        let mut changed = false;
        for app in seen {
            let names: Vec<String> = app.names.iter().filter_map(|name| kept(name)).collect();
            let shown = kept(&app.shown).unwrap_or_default();
            let known = self
                .apps
                .iter()
                .position(|card| card.source == app.source && card.id == app.id);
            let index = known.unwrap_or_else(|| {
                self.apps.push(SeenApp {
                    names: Vec::new(),
                    shown: String::new(),
                    ..app.clone()
                });
                changed = true;
                self.apps.len() - 1
            });
            let card = &mut self.apps[index];
            for name in names {
                if card.names.len() < MOST_NAMES && !card.names.contains(&name) {
                    card.names.push(name);
                    changed = true;
                }
            }
            if !shown.is_empty() && card.shown != shown {
                card.shown = shown;
                changed = true;
            }
            if app.day > card.day {
                card.day = app.day;
                changed = true;
            }
        }
        changed
    }

    /// Keeps the applications seen in `directory`, replacing those there
    /// whole unless those cannot be read (D-10). The oldest cards go first
    /// when they would not fit in a file Okapi reads back.
    ///
    /// # Errors
    ///
    /// Those of [`Self::load`] for a file there that cannot be read, and
    /// those of an atomic write.
    pub fn save(&self, directory: &Path) -> Result<(), StoreError> {
        file::replaceable(Self::load(directory))?;
        let most = usize::try_from(file::MAX_FILE_SIZE).unwrap_or(usize::MAX);
        let fitted = self.fitted(most.saturating_sub(ENVELOPE_ROOM));
        if fitted.apps.len() < self.apps.len() {
            tracing::info!(
                kept = fitted.apps.len(),
                dropped = self.apps.len() - fitted.apps.len(),
                "the oldest applications seen forgotten: the file is full"
            );
        }
        file::write_versioned(directory, SEEN_FILE, SEEN_VERSION, &fitted)?;
        tracing::info!(path = %Self::path(directory).display(), apps = fitted.apps.len(), "applications seen kept");
        Ok(())
    }

    /// The most recent cards whose writing takes `limit` bytes at most.
    fn fitted(&self, limit: usize) -> Self {
        let mut apps = self.apps.clone();
        // The most recent first: the oldest are the ones to drop.
        apps.sort_by_key(|app| std::cmp::Reverse(app.day));
        let size = |apps: &[SeenApp]| {
            serde_json::to_vec_pretty(&Self {
                apps: apps.to_vec(),
            })
            .map_or(usize::MAX, |bytes| bytes.len())
        };
        let mut kept = apps.len();
        while kept > 0 && size(&apps[..kept]) > limit {
            // Each step drops a tenth, at least one: few writings, even far
            // over the limit.
            kept -= (kept / 10).max(1);
        }
        apps.truncate(kept);
        Self { apps }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn app(id: &str, names: &[&str], day: i64) -> SeenApp {
        SeenApp {
            source: "snap".to_owned(),
            id: id.to_owned(),
            shown: id.to_uppercase(),
            names: names.iter().map(|name| (*name).to_owned()).collect(),
            day,
        }
    }

    #[test]
    fn an_application_seen_again_joins_its_names_and_moves_its_day() {
        // Catches a card added twice, names lost, a day moved back, and a
        // change said when nothing changed (a needless writing).
        let mut seen = SeenApps::default();
        assert!(seen.remember([app("spotify", &["spotify", "Spotify"], 10)]));
        assert!(seen.remember([app("spotify", &["spotify", "spotify-client"], 12)]));
        assert!(!seen.remember([app("spotify", &["Spotify"], 11)]));
        assert_eq!(seen.apps().len(), 1);
        let card = seen.find("snap", "spotify").unwrap();
        assert_eq!(card.names, ["spotify", "Spotify", "spotify-client"]);
        assert_eq!(card.day, 12);
        assert_eq!(card.shown, "SPOTIFY");
        assert_eq!(seen.find("flatpak", "spotify"), None, "by its source too");
    }

    #[test]
    fn a_name_too_long_or_too_many_are_never_kept() {
        let long = "x".repeat(MOST_NAME_BYTES + 1);
        let many: Vec<String> = (0..20).map(|index| format!("name{index}")).collect();
        let mut names: Vec<&str> = many.iter().map(String::as_str).collect();
        names.insert(0, &long);
        let mut seen = SeenApps::default();
        seen.remember([app("x", &names, 1)]);
        let card = seen.find("snap", "x").unwrap();
        assert_eq!(card.names.len(), MOST_NAMES);
        assert!(!card.names.contains(&long));
    }

    #[test]
    fn the_file_always_stays_small_enough_to_be_read_back() {
        // Catches a memory that grows past what Okapi reads: never replaced
        // (D-10), it would freeze for good.
        let directory = tempfile::tempdir().unwrap();
        let longest = "n".repeat(MOST_NAME_BYTES);
        let mut seen = SeenApps::default();
        seen.remember((0..6_000).map(|index| {
            SeenApp {
                source: "flatpak".to_owned(),
                id: format!("{index:0>120}"),
                shown: longest.clone(),
                names: (0..MOST_NAMES)
                    .map(|rank| format!("{rank}{}", &longest[1..]))
                    .collect(),
                day: index,
            }
        }));
        seen.save(directory.path()).unwrap();
        let written = fs::metadata(SeenApps::path(directory.path()))
            .unwrap()
            .len();
        assert!(written <= file::MAX_FILE_SIZE, "{written}");
        let back = SeenApps::load(directory.path()).unwrap();
        assert!(back.apps().len() < 6_000);
        assert!(
            back.find("flatpak", &format!("{:0>120}", 5_999)).is_some(),
            "the most recent kept"
        );
        assert_eq!(
            back.find("flatpak", &format!("{:0>120}", 0)),
            None,
            "the oldest gone"
        );
    }

    #[test]
    fn a_file_that_cannot_be_read_is_never_replaced() {
        let directory = tempfile::tempdir().unwrap();
        let path = SeenApps::path(directory.path());
        fs::write(&path, "{").unwrap();
        assert!(matches!(
            SeenApps::load(directory.path()),
            Err(StoreError::Invalid { .. })
        ));
        let mut seen = SeenApps::default();
        seen.remember([app("x", &["x"], 1)]);
        assert!(seen.save(directory.path()).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{");
    }
}
