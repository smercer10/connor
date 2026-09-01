//! The keymap: one static table drives both key dispatch and the help
//! overlay, so the two can never drift. Every binding's variant is
//! constructed only here, and dispatch matches `Action` without a wildcard —
//! dropping either side is a compile error.

use std::fmt::Write as _;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Everything a bound key can do. Printable characters insert themselves
/// and are a rule, not a binding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    NewTab,
    Open,
    PickFile,
    ToggleTree,
    Save,
    CloseTab,
    Quit,
    PrevTab,
    NextTab,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    Newline,
    InsertTab,
    Backspace,
    Delete,
    Find,
    FindProject,
    GoToLine,
    PrevChange,
    NextChange,
    ToggleDiff,
    ToggleLock,
    Left,
    Right,
    Up,
    Down,
    WordLeft,
    WordRight,
    Home,
    End,
    DocStart,
    DocEnd,
    PageUp,
    PageDown,
    Help,
}

impl Action {
    /// Whether the lock refuses this action. Exhaustive on purpose — a new
    /// action has to say which side it is on rather than defaulting to
    /// allowed, the same bargain dispatch makes by matching without a
    /// wildcard. `Save` is here despite changing no buffer text: it writes
    /// the file, and the file is what the agent has its hands on.
    pub fn writes(self) -> bool {
        match self {
            Action::Save
            | Action::Undo
            | Action::Redo
            | Action::Cut
            | Action::Paste
            | Action::Newline
            | Action::InsertTab
            | Action::Backspace
            | Action::Delete => true,
            Action::NewTab
            | Action::Open
            | Action::PickFile
            | Action::ToggleTree
            | Action::CloseTab
            | Action::Quit
            | Action::PrevTab
            | Action::NextTab
            | Action::Copy
            | Action::Find
            | Action::FindProject
            | Action::GoToLine
            | Action::PrevChange
            | Action::NextChange
            | Action::ToggleDiff
            | Action::ToggleLock
            | Action::Left
            | Action::Right
            | Action::Up
            | Action::Down
            | Action::WordLeft
            | Action::WordRight
            | Action::Home
            | Action::End
            | Action::DocStart
            | Action::DocEnd
            | Action::PageUp
            | Action::PageDown
            | Action::Help => false,
        }
    }

    /// Movement shares a prelude in dispatch: shift extends the selection,
    /// plain movement clears it, and the undo group breaks.
    pub fn is_movement(self) -> bool {
        matches!(
            self,
            Action::Left
                | Action::Right
                | Action::Up
                | Action::Down
                | Action::WordLeft
                | Action::WordRight
                | Action::Home
                | Action::End
                | Action::DocStart
                | Action::DocEnd
                | Action::PageUp
                | Action::PageDown
        )
    }
}

/// One chord: the modifiers that must be held (shift never counts) and the
/// key they're held on.
pub struct Key {
    pub mods: KeyModifiers,
    pub code: KeyCode,
}

/// One help row: every chord that triggers the action, and what it does.
pub struct Binding {
    pub keys: &'static [Key],
    pub action: Action,
    pub what: &'static str,
}

pub struct Section {
    pub title: &'static str,
    pub bindings: &'static [Binding],
}

const fn ctrl(code: KeyCode) -> Key {
    Key {
        mods: KeyModifiers::CONTROL,
        code,
    }
}

const fn alt(code: KeyCode) -> Key {
    Key {
        mods: KeyModifiers::ALT,
        code,
    }
}

const fn plain(code: KeyCode) -> Key {
    Key {
        mods: KeyModifiers::NONE,
        code,
    }
}

const fn bind(keys: &'static [Key], action: Action, what: &'static str) -> Binding {
    Binding { keys, action, what }
}

pub static KEYMAP: &[Section] = &[
    Section {
        title: "files",
        bindings: &[
            bind(&[ctrl(KeyCode::Char('n'))], Action::NewTab, "new tab"),
            bind(&[ctrl(KeyCode::Char('o'))], Action::Open, "open file"),
            bind(
                &[ctrl(KeyCode::Char('p'))],
                Action::PickFile,
                "open by name",
            ),
            bind(
                &[ctrl(KeyCode::Char('b'))],
                Action::ToggleTree,
                "project tree",
            ),
            bind(&[ctrl(KeyCode::Char('s'))], Action::Save, "save"),
            bind(&[ctrl(KeyCode::Char('w'))], Action::CloseTab, "close tab"),
            bind(&[ctrl(KeyCode::Char('q'))], Action::Quit, "quit"),
        ],
    },
    Section {
        title: "tabs",
        bindings: &[
            bind(
                &[ctrl(KeyCode::PageUp), alt(KeyCode::Left)],
                Action::PrevTab,
                "previous tab",
            ),
            bind(
                &[ctrl(KeyCode::PageDown), alt(KeyCode::Right)],
                Action::NextTab,
                "next tab",
            ),
        ],
    },
    Section {
        title: "edit",
        bindings: &[
            bind(&[ctrl(KeyCode::Char('z'))], Action::Undo, "undo"),
            bind(&[ctrl(KeyCode::Char('y'))], Action::Redo, "redo"),
            bind(&[ctrl(KeyCode::Char('x'))], Action::Cut, "cut"),
            bind(&[ctrl(KeyCode::Char('c'))], Action::Copy, "copy"),
            bind(&[ctrl(KeyCode::Char('v'))], Action::Paste, "paste"),
            bind(&[plain(KeyCode::Enter)], Action::Newline, "new line"),
            bind(&[plain(KeyCode::Tab)], Action::InsertTab, "insert tab"),
            bind(
                &[plain(KeyCode::Backspace)],
                Action::Backspace,
                "delete left",
            ),
            bind(&[plain(KeyCode::Delete)], Action::Delete, "delete right"),
        ],
    },
    Section {
        title: "search",
        bindings: &[
            bind(&[ctrl(KeyCode::Char('f'))], Action::Find, "find / replace"),
            bind(
                &[alt(KeyCode::Char('f'))],
                Action::FindProject,
                "find in project",
            ),
            bind(&[ctrl(KeyCode::Char('g'))], Action::GoToLine, "go to line"),
        ],
    },
    Section {
        title: "changes",
        bindings: &[
            bind(&[alt(KeyCode::Up)], Action::PrevChange, "previous change"),
            bind(&[alt(KeyCode::Down)], Action::NextChange, "next change"),
            bind(
                &[alt(KeyCode::Char('d'))],
                Action::ToggleDiff,
                "diff against HEAD or disk",
            ),
            bind(
                &[alt(KeyCode::Char('l'))],
                Action::ToggleLock,
                "lock editing",
            ),
        ],
    },
    Section {
        title: "move",
        bindings: &[
            bind(&[plain(KeyCode::Left)], Action::Left, "left"),
            bind(&[plain(KeyCode::Right)], Action::Right, "right"),
            bind(&[plain(KeyCode::Up)], Action::Up, "up"),
            bind(&[plain(KeyCode::Down)], Action::Down, "down"),
            bind(&[ctrl(KeyCode::Left)], Action::WordLeft, "previous word"),
            bind(&[ctrl(KeyCode::Right)], Action::WordRight, "next word"),
            bind(&[plain(KeyCode::Home)], Action::Home, "line start"),
            bind(&[plain(KeyCode::End)], Action::End, "line end"),
            bind(&[ctrl(KeyCode::Home)], Action::DocStart, "document start"),
            bind(&[ctrl(KeyCode::End)], Action::DocEnd, "document end"),
            bind(&[plain(KeyCode::PageUp)], Action::PageUp, "page up"),
            bind(&[plain(KeyCode::PageDown)], Action::PageDown, "page down"),
        ],
    },
    Section {
        title: "help",
        bindings: &[bind(
            &[ctrl(KeyCode::Char('/')), plain(KeyCode::F(1))],
            Action::Help,
            "this help",
        )],
    },
];

/// The action a keypress triggers, if any. Shift is deliberately ignored:
/// dispatch reads it itself to extend selections through movement keys.
pub fn lookup(key: &KeyEvent) -> Option<Action> {
    let mods = key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT);
    let code = normalize(key.code, mods);
    for section in KEYMAP {
        for binding in section.bindings {
            for k in binding.keys {
                if k.code == code && k.mods == mods {
                    return Some(binding.action);
                }
            }
        }
    }
    None
}

/// Legacy terminals send Ctrl+/ as byte 0x1F, which crossterm decodes as
/// Ctrl+7; terminals speaking the kitty protocol send Ctrl+/ itself.
fn normalize(code: KeyCode, mods: KeyModifiers) -> KeyCode {
    if mods == KeyModifiers::CONTROL && code == KeyCode::Char('7') {
        return KeyCode::Char('/');
    }
    code
}

impl Binding {
    /// Appends the derived label, e.g. "Ctrl+PgUp·Alt+←" — the table's key
    /// data is the only source, so the overlay can't misquote a chord.
    pub fn write_label(&self, out: &mut String) {
        for (i, key) in self.keys.iter().enumerate() {
            if i > 0 {
                out.push('·');
            }
            push_key(out, key);
        }
    }
}

/// Appends the status line's pointer at the overlay, e.g. "F1 help":
/// the Help binding's unmodified chord (its tersest spelling), sourced
/// from the table so a rebinding follows.
pub fn write_help_hint(out: &mut String) {
    let binding = KEYMAP
        .iter()
        .flat_map(|s| s.bindings)
        .find(|b| b.action == Action::Help);
    let Some(binding) = binding else { return };
    let key = binding
        .keys
        .iter()
        .find(|k| k.mods.is_empty())
        .or(binding.keys.first());
    if let Some(key) = key {
        push_key(out, key);
        out.push_str(" help");
    }
}

/// Appends the lock toggle's chord, e.g. "Alt+L" — the status line's
/// indicator and every refusal are built from it, so a rebinding follows
/// into both rather than leaving a message quoting a dead key.
pub fn write_lock_chord(out: &mut String) {
    let binding = KEYMAP
        .iter()
        .flat_map(|s| s.bindings)
        .find(|b| b.action == Action::ToggleLock);
    if let Some(key) = binding.and_then(|b| b.keys.first()) {
        push_key(out, key);
    }
}

fn push_key(out: &mut String, key: &Key) {
    if key.mods.contains(KeyModifiers::CONTROL) {
        out.push_str("Ctrl+");
    }
    if key.mods.contains(KeyModifiers::ALT) {
        out.push_str("Alt+");
    }
    match key.code {
        KeyCode::Char(c) => out.extend(c.to_uppercase()),
        KeyCode::F(n) => {
            let _ = write!(out, "F{n}");
        }
        KeyCode::Left => out.push('←'),
        KeyCode::Right => out.push('→'),
        KeyCode::Up => out.push('↑'),
        KeyCode::Down => out.push('↓'),
        KeyCode::PageUp => out.push_str("PgUp"),
        KeyCode::PageDown => out.push_str("PgDn"),
        KeyCode::Home => out.push_str("Home"),
        KeyCode::End => out.push_str("End"),
        KeyCode::Enter => out.push_str("Enter"),
        KeyCode::Tab => out.push_str("Tab"),
        KeyCode::Backspace => out.push_str("Bksp"),
        KeyCode::Delete => out.push_str("Del"),
        _ => out.push('?'),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(action: Action) -> &'static Binding {
        KEYMAP
            .iter()
            .flat_map(|s| s.bindings)
            .find(|b| b.action == action)
            .unwrap()
    }

    fn label(action: Action) -> String {
        let mut out = String::new();
        find(action).write_label(&mut out);
        out
    }

    #[test]
    fn every_listed_key_resolves_to_its_own_action() {
        for section in KEYMAP {
            for binding in section.bindings {
                assert!(
                    !binding.keys.is_empty(),
                    "{:?} lists no keys",
                    binding.action
                );
                for key in binding.keys {
                    let event = KeyEvent::new(key.code, key.mods);
                    assert_eq!(
                        lookup(&event),
                        Some(binding.action),
                        "{:?} does not resolve to {:?}",
                        key.code,
                        binding.action
                    );
                }
            }
        }
    }

    #[test]
    fn the_change_jumps_read_as_alt_arrows_and_are_not_movement() {
        assert_eq!(label(Action::NextChange), "Alt+↓");
        assert_eq!(label(Action::PrevChange), "Alt+↑");
        assert_eq!(find(Action::NextChange).what, "next change");
        // Movement means shift extends a selection through it; jumping
        // between hunks has no such reading.
        assert!(!Action::NextChange.is_movement());
        assert!(!Action::PrevChange.is_movement());
    }

    #[test]
    fn the_diff_toggle_joins_the_change_family() {
        assert_eq!(label(Action::ToggleDiff), "Alt+D");
        assert_eq!(find(Action::ToggleDiff).what, "diff against HEAD or disk");
        // Its own section, beside the jumps that walk what it shows.
        let section = KEYMAP
            .iter()
            .find(|s| s.bindings.iter().any(|b| b.action == Action::ToggleDiff))
            .unwrap();
        assert_eq!(section.title, "changes");
        // Nothing to extend with shift, and no cursor to extend from.
        assert!(!Action::ToggleDiff.is_movement());
    }

    #[test]
    fn the_lock_toggle_joins_the_change_family() {
        assert_eq!(label(Action::ToggleLock), "Alt+L");
        assert_eq!(find(Action::ToggleLock).what, "lock editing");
        // Browsing while the agent writes is the review loop, so it sits
        // with the jumps and the diff rather than in a section of its own.
        let section = KEYMAP
            .iter()
            .find(|s| s.bindings.iter().any(|b| b.action == Action::ToggleLock))
            .unwrap();
        assert_eq!(section.title, "changes");
        assert!(!Action::ToggleLock.is_movement());
        // The way out of the lock can never be locked away.
        assert!(!Action::ToggleLock.writes());
    }

    #[test]
    fn the_lock_chord_names_the_toggle() {
        let mut out = String::new();
        write_lock_chord(&mut out);
        assert_eq!(out, "Alt+L");
    }

    #[test]
    fn writing_classification_matches_the_table() {
        // The edit section is what the lock exists to stop, save for Copy,
        // which only reads. Deriving the expectation from the table means a
        // binding added there has to be classified rather than defaulting.
        for section in KEYMAP {
            for binding in section.bindings {
                let expected = match section.title {
                    "edit" => binding.action != Action::Copy,
                    _ => binding.action == Action::Save,
                };
                assert_eq!(
                    binding.action.writes(),
                    expected,
                    "{:?} in section {}",
                    binding.action,
                    section.title
                );
            }
        }
    }

    #[test]
    fn browsing_survives_the_lock() {
        // The bullet the feature is judged on: navigation, search,
        // selection, copy, tabs, the tree and the diff all still work.
        for action in [
            Action::Copy,
            Action::Find,
            Action::FindProject,
            Action::GoToLine,
            Action::PrevChange,
            Action::NextChange,
            Action::ToggleDiff,
            Action::ToggleTree,
            Action::PickFile,
            Action::Open,
            Action::NewTab,
            Action::PrevTab,
            Action::NextTab,
            Action::CloseTab,
            Action::Quit,
            Action::Help,
        ] {
            assert!(!action.writes(), "{action:?} is blocked by the lock");
        }
        for section in KEYMAP {
            if section.title == "move" {
                for binding in section.bindings {
                    assert!(!binding.action.writes(), "{:?}", binding.action);
                }
            }
        }
    }

    #[test]
    fn shift_never_blocks_a_binding() {
        let shifted =
            |code, extra: KeyModifiers| lookup(&KeyEvent::new(code, extra | KeyModifiers::SHIFT));
        assert_eq!(
            shifted(KeyCode::Left, KeyModifiers::NONE),
            Some(Action::Left)
        );
        assert_eq!(
            shifted(KeyCode::Right, KeyModifiers::CONTROL),
            Some(Action::WordRight)
        );
        assert_eq!(
            shifted(KeyCode::PageUp, KeyModifiers::CONTROL),
            Some(Action::PrevTab)
        );
    }

    #[test]
    fn both_help_chords_and_the_legacy_ctrl_slash_byte_open_help() {
        for (code, mods) in [
            (KeyCode::Char('/'), KeyModifiers::CONTROL),
            (KeyCode::Char('7'), KeyModifiers::CONTROL),
            (KeyCode::F(1), KeyModifiers::NONE),
        ] {
            assert_eq!(lookup(&KeyEvent::new(code, mods)), Some(Action::Help));
        }
    }

    #[test]
    fn unbound_keys_and_plain_characters_stay_free() {
        for (code, mods) in [
            (KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Char('7'), KeyModifiers::NONE),
            (KeyCode::Char('x'), KeyModifiers::ALT),
            (KeyCode::F(5), KeyModifiers::NONE),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Enter, KeyModifiers::CONTROL),
            (KeyCode::Delete, KeyModifiers::CONTROL),
        ] {
            assert_eq!(lookup(&KeyEvent::new(code, mods)), None, "{code:?} bound");
        }
    }

    #[test]
    fn labels_derive_from_key_specs() {
        assert_eq!(label(Action::Save), "Ctrl+S");
        assert_eq!(label(Action::PrevTab), "Ctrl+PgUp·Alt+←");
        assert_eq!(label(Action::Help), "Ctrl+/·F1");
        assert_eq!(label(Action::DocStart), "Ctrl+Home");
        assert_eq!(label(Action::Backspace), "Bksp");
    }

    #[test]
    fn the_help_hint_names_the_unmodified_chord() {
        let mut out = String::new();
        write_help_hint(&mut out);
        assert_eq!(out, "F1 help");
    }

    #[test]
    fn movement_classification_matches_the_table() {
        for section in KEYMAP {
            for binding in section.bindings {
                assert_eq!(
                    binding.action.is_movement(),
                    section.title == "move",
                    "{:?} in section {}",
                    binding.action,
                    section.title
                );
            }
        }
    }
}
