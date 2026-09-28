use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::tui::app::{Action, Prompt};
use crate::tui::view::Grouping;

/// Map a key to an action for the active prompt. `App` decides what the
/// resulting actions do. Confirmation keeps letters as `Char` for `App`.
pub fn action_for_key(key: KeyEvent, prompt: &Prompt) -> Option<Action> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        return match key.code {
            KeyCode::Char('c') => Some(Action::Quit),
            // `Ctrl-L` is the readline redraw reflex and vim's clear-screen;
            // `x` is the primary spelling.
            KeyCode::Char('l') => Some(Action::ClearFilter),
            KeyCode::Char('r') => Some(Action::Refresh),
            // Vim's own window-command prefix: `Ctrl-W` then arrows.
            KeyCode::Char('w') => Some(Action::StartResize),
            // Vim's page scroll, spelled the way a vim user reaches for it.
            KeyCode::Char('f') => Some(Action::PageDown),
            KeyCode::Char('b') => Some(Action::PageUp),
            // Alias for `v`, kept until the re-home has settled.
            KeyCode::Char('a') => Some(Action::MarkAll),
            _ => None,
        };
    }

    match prompt {
        Prompt::Filter(_) | Prompt::Search(_) | Prompt::Insert { .. } => match key.code {
            KeyCode::Esc => Some(Action::Cancel),
            KeyCode::Enter => Some(Action::Confirm),
            KeyCode::Backspace => Some(Action::Backspace),
            KeyCode::Char(c) => Some(Action::Char(c)),
            _ => None,
        },
        // Enter means "never mind".
        Prompt::Bookmark { .. } => match key.code {
            KeyCode::Esc | KeyCode::Enter => Some(Action::Cancel),
            KeyCode::Char(c) => Some(Action::Char(c)),
            _ => None,
        },
        Prompt::Fold => match key.code {
            KeyCode::Char('a') => Some(Action::ToggleFold),
            KeyCode::Char('c') => Some(Action::CloseFold),
            KeyCode::Char('o') => Some(Action::OpenFold),
            KeyCode::Char('M') => Some(Action::CloseAllFolds),
            KeyCode::Char('R') => Some(Action::OpenAllFolds),
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(_) => Some(Action::Cancel),
            _ => None,
        },
        Prompt::Resize => match key.code {
            KeyCode::Right | KeyCode::Char('l') => Some(Action::Widen),
            KeyCode::Left | KeyCode::Char('h') => Some(Action::Narrow),
            KeyCode::Char('o') => Some(Action::ToggleZen),
            KeyCode::Esc => Some(Action::Cancel),
            KeyCode::Enter => Some(Action::Confirm),
            KeyCode::Char(c) => Some(Action::Char(c)),
            _ => None,
        },
        Prompt::ConfirmArchive { .. } => match key.code {
            KeyCode::Esc => Some(Action::Cancel),
            KeyCode::Enter => Some(Action::Confirm),
            KeyCode::Char(c) => Some(Action::Char(c)),
            _ => None,
        },
        Prompt::None => base(key.code),
    }
}

/// The unprompted table: navigation, views, marks, folds' prefixes.
fn base(code: KeyCode) -> Option<Action> {
    match code {
        // `q` reads like `Esc`, and it used to kill the program from any
        // pane. Quitting is deliberate: `Q` or `Ctrl-C`.
        KeyCode::Char('Q') => Some(Action::Quit),
        KeyCode::Char('q') | KeyCode::Esc => Some(Action::Cancel),
        // Pre-re-home spellings, accepted silently for muscle memory and
        // deliberately absent from the help: `A`/`U`/`L`/`M`/`b`.
        KeyCode::Char('A') => Some(Action::ArchiveMarked),
        KeyCode::Char('U') => Some(Action::UnarchiveMarked),
        KeyCode::Char('L') => Some(Action::ToggleLiveOnly),
        KeyCode::Char('M') => Some(Action::ToggleGroupMark),
        KeyCode::Char('b') => Some(Action::StartBookmark),
        KeyCode::Char('j') | KeyCode::Down => Some(Action::Down),
        KeyCode::Char('k') | KeyCode::Up => Some(Action::Up),
        KeyCode::PageDown => Some(Action::PageDown),
        KeyCode::PageUp => Some(Action::PageUp),
        KeyCode::Char('g') | KeyCode::Home => Some(Action::Top),
        KeyCode::Char('G') | KeyCode::End => Some(Action::Bottom),
        KeyCode::Enter => Some(Action::Confirm),
        KeyCode::Right | KeyCode::Char('l') => Some(Action::FocusNext),
        KeyCode::Left | KeyCode::Char('h') => Some(Action::FocusPrev),
        KeyCode::Tab => Some(Action::CycleFocusNext),
        KeyCode::BackTab => Some(Action::CycleFocusPrev),
        KeyCode::Char(']') => Some(Action::CycleGroupingNext),
        KeyCode::Char('[') => Some(Action::CycleGroupingPrev),
        // A digit jumps straight to a view; `]`/`[` step through them.
        KeyCode::Char(digit) if Grouping::for_key(digit).is_some() => {
            Grouping::for_key(digit).map(Action::SetGrouping)
        }
        KeyCode::Char('J') => Some(Action::JumpLive),
        KeyCode::Char('/') => Some(Action::StartFilter),
        KeyCode::Char('s') => Some(Action::StartSearch),
        KeyCode::Char('a') => Some(Action::ToggleArchived),
        KeyCode::Char('e') => Some(Action::ToggleLiveOnly),
        KeyCode::Char('x') => Some(Action::ClearFilter),
        // Vim's visual keys select: `v` takes every visible session, `V` the
        // whole group, and Space toggles the row under the cursor.
        KeyCode::Char(' ') => Some(Action::ToggleMark),
        KeyCode::Char('v') => Some(Action::MarkAll),
        KeyCode::Char('V') => Some(Action::ToggleGroupMark),
        KeyCode::Char('f') => Some(Action::ToggleFollow),
        KeyCode::Char('z') => Some(Action::StartFold),
        // Vim-mark grammar: `m<letter>` sets, `'<letter>` jumps. Both are
        // prefixes, so the letters underneath stay free for their actions.
        KeyCode::Char('m') => Some(Action::StartBookmark),
        // Vim takes both spellings of a mark jump; so does this.
        KeyCode::Char('\'' | '`') => Some(Action::StartJump),
        // `d`/`u` is vim's do/undo pair, and keeps both actions off the `a`
        // shelf: one missed shift used to turn "show archived" into
        // "archive everything marked".
        KeyCode::Char('d') => Some(Action::ArchiveMarked),
        KeyCode::Char('u') => Some(Action::UnarchiveMarked),
        KeyCode::Char('t') => Some(Action::ToggleThinking),
        KeyCode::Char('o') => Some(Action::ToggleTools),
        KeyCode::Char('c') => Some(Action::ToggleFiles),
        KeyCode::F(11) => Some(Action::ToggleZen),
        KeyCode::Char('r') => Some(Action::Resume),
        KeyCode::Char('i') => Some(Action::StartInsert),
        KeyCode::Char('?') | KeyCode::F(1) => Some(Action::ToggleHelp),
        KeyCode::Char(c) => Some(Action::Char(c)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(c: char) -> Option<Action> {
        action_for_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &Prompt::None,
        )
    }

    fn ctrl(c: char) -> Option<Action> {
        action_for_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL),
            &Prompt::None,
        )
    }

    fn fold(c: char) -> Option<Action> {
        action_for_key(
            KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
            &Prompt::Fold,
        )
    }

    /// The re-home in one place: the vim-conventional letters, and the
    /// do/undo pair that keeps archiving off the `a` shelf.
    #[test]
    fn the_rehomed_letters_map_to_their_actions() {
        assert_eq!(press('d'), Some(Action::ArchiveMarked));
        assert_eq!(press('u'), Some(Action::UnarchiveMarked));
        assert_eq!(press('v'), Some(Action::MarkAll));
        assert_eq!(press('V'), Some(Action::ToggleGroupMark));
        assert_eq!(press(' '), Some(Action::ToggleMark));
        assert_eq!(press('e'), Some(Action::ToggleLiveOnly));
        assert_eq!(press('x'), Some(Action::ClearFilter));
        assert_eq!(ctrl('w'), Some(Action::StartResize));
        assert_eq!(press(']'), Some(Action::CycleGroupingNext));
        assert_eq!(press('['), Some(Action::CycleGroupingPrev));
        // The letters that did not move still work.
        assert_eq!(press('a'), Some(Action::ToggleArchived));
        assert_eq!(press('l'), Some(Action::FocusNext));
        // Bookmark prefixes: vim-mark grammar, one letter then done.
        assert_eq!(press('m'), Some(Action::StartBookmark));
        assert_eq!(press('\''), Some(Action::StartJump));
        assert_eq!(press('`'), Some(Action::StartJump));
        assert_eq!(ctrl('f'), Some(Action::PageDown));
        assert_eq!(ctrl('b'), Some(Action::PageUp));
        assert_eq!(press('f'), Some(Action::ToggleFollow));
        assert_eq!(press('z'), Some(Action::StartFold));
        assert_eq!(
            action_for_key(
                KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
                &Prompt::Resize,
            ),
            Some(Action::ToggleZen)
        );
        assert_eq!(fold('a'), Some(Action::ToggleFold));
        assert_eq!(fold('c'), Some(Action::CloseFold));
        assert_eq!(fold('o'), Some(Action::OpenFold));
        assert_eq!(fold('M'), Some(Action::CloseAllFolds));
        assert_eq!(fold('R'), Some(Action::OpenAllFolds));
    }

    /// Old spellings stay accepted for one release of muscle memory. When
    /// these are removed, delete this test with them.
    #[test]
    fn the_pre_rehome_spellings_still_answer() {
        assert_eq!(press('A'), Some(Action::ArchiveMarked));
        assert_eq!(press('U'), Some(Action::UnarchiveMarked));
        assert_eq!(press('L'), Some(Action::ToggleLiveOnly));
        assert_eq!(press('M'), Some(Action::ToggleGroupMark));
        assert_eq!(press('b'), Some(Action::StartBookmark));
        assert_eq!(ctrl('a'), Some(Action::MarkAll));
        assert_eq!(ctrl('l'), Some(Action::ClearFilter));
    }

    /// While text is being typed, printable keys are literal input and
    /// nothing navigates.
    #[test]
    fn a_text_prompt_swallows_navigation() {
        let prompt = Prompt::Filter(String::new());
        let action = |code| action_for_key(KeyEvent::new(code, KeyModifiers::NONE), &prompt);
        assert_eq!(action(KeyCode::Char('q')), Some(Action::Char('q')));
        assert_eq!(action(KeyCode::Char('j')), Some(Action::Char('j')));
        assert_eq!(action(KeyCode::Esc), Some(Action::Cancel));
        assert_eq!(action(KeyCode::Enter), Some(Action::Confirm));
        assert_eq!(action(KeyCode::Down), None);
    }
}
