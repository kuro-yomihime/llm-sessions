//! Named session shortcuts: one letter each, restored to the exact
//! view/pane/row that was selected when the letter was saved.
//!
//! Vim's mark grammar, because it is the one people already know: `m<letter>`
//! sets a bookmark, `'<letter>` jumps to it. Twenty-six slots is more
//! favourites than anyone has; the file is a map under `$XDG_STATE_HOME`, so
//! it survives rescans and restarts and never touches agent stores.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::SourceId;

/// Everything needed to put the user back where they stood.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bookmark {
    /// Host name from `hosts.json`; `None` is this machine. Ids collide
    /// across machines, so the bookmark without a host is ambiguous.
    #[serde(default)]
    pub host: Option<String>,
    pub source: SourceId,
    pub id: String,
    /// Which left-pane axis was active.
    pub view: String,
    /// Which pane held focus, letters `g`/`s`/`t`.
    pub focus: char,
}

/// Letter → bookmark. `BTreeMap` so the file diffs stably.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub marks: BTreeMap<char, Bookmark>,
}

impl Store {
    pub fn default_path(home: &Path) -> PathBuf {
        std::env::var_os("XDG_STATE_HOME")
            .map_or_else(|| home.join(".local/state"), PathBuf::from)
            .join("llm-sessions/aliases.json")
    }

    /// Absent file means no bookmarks, not an error; an unreadable one is
    /// reported once and otherwise ignored, exactly like `hosts.json`.
    pub fn load(home: &Path) -> (Self, Option<String>) {
        Self::load_at(&Self::default_path(home))
    }

    pub(crate) fn load_at(path: &Path) -> (Self, Option<String>) {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Self::default(), None),
            Err(e) => return (Self::default(), Some(format!("aliases.json: {e}"))),
        };
        match serde_json::from_str(&text) {
            Ok(store) => (store, None),
            Err(e) => (Self::default(), Some(format!("aliases.json: {e}"))),
        }
    }

    pub fn save_at(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            path,
            serde_json::to_string_pretty(self).unwrap_or_default() + "\n",
        )
    }

    pub fn get(&self, letter: char) -> Option<&Bookmark> {
        self.marks.get(&letter)
    }
}

/// The letters a bookmark may use. Kept to `a`–`z`: digits already jump to
/// views, and one alphabet is enough favourites for anyone.
pub fn valid_letter(c: char) -> bool {
    c.is_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::view::{Grouping, Pane};

    #[test]
    fn a_store_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliases.json");
        let store = Store {
            marks: BTreeMap::from([(
                'a',
                Bookmark {
                    host: Some("nx-1".into()),
                    source: SourceId::Omp,
                    id: "01a0".into(),
                    view: "host".into(),
                    focus: 't',
                },
            )]),
        };
        store.save_at(&path).unwrap();
        let (loaded, failure) = Store::load_at(&path);
        assert_eq!(failure, None);
        assert_eq!(loaded.marks.len(), 1);
        assert_eq!(loaded.get('a'), store.get('a'));
    }

    #[test]
    fn a_broken_file_is_one_reported_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliases.json");
        std::fs::write(&path, "not json").unwrap();
        let (store, failure) = Store::load_at(&path);
        assert!(store.marks.is_empty());
        assert!(failure.is_some());
    }

    #[test]
    fn view_and_pane_spellings_invert() {
        for view in [
            Grouping::Workspace,
            Grouping::Agent,
            Grouping::Branch,
            Grouping::Fleet,
            Grouping::Host,
            Grouping::Timeline,
        ] {
            assert_eq!(Grouping::from_label(view.label()), view);
        }
        assert_eq!(Grouping::from_label("from the future"), Grouping::Workspace);
        for pane in [Pane::Groups, Pane::Sessions, Pane::Transcript] {
            assert_eq!(Pane::from_letter(pane.letter()), pane);
        }
    }
}
