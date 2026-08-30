//! One-line status-line prompts: an append-only path field with bash-style
//! Tab completion, and a digits-only line-number field.

use std::fs;
use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

pub enum Outcome {
    Pending,
    Submit,
    Cancel,
}

pub struct PathPrompt {
    label: &'static str,
    buf: String,
}

impl PathPrompt {
    pub fn new(label: &'static str) -> PathPrompt {
        PathPrompt {
            label,
            buf: String::new(),
        }
    }

    /// Feeds one keypress: printable characters append, Backspace trims,
    /// Tab completes against the filesystem. Anything else is ignored so a
    /// stray chord can't dismiss the prompt.
    pub fn key(&mut self, key: &KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Enter => return Outcome::Submit,
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Backspace => {
                self.buf.pop();
            }
            KeyCode::Tab => complete_path(&mut self.buf),
            // Ctrl- and Alt-modified characters are chords, not input.
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.buf.push(ch);
            }
            _ => {}
        }
        Outcome::Pending
    }

    /// Appends a paste flattened to a single line: control characters,
    /// line breaks included, are dropped.
    pub fn paste(&mut self, text: &str) {
        self.buf.extend(text.chars().filter(|ch| !ch.is_control()));
    }

    /// Writes the prompt into the notice; the caret sits at its end.
    pub fn render(&self, notice: &mut String) {
        notice.clear();
        notice.push_str(self.label);
        notice.push_str(&self.buf);
    }

    pub fn into_path(self) -> PathBuf {
        PathBuf::from(self.buf)
    }
}

/// Digits-only line-number field for go-to-line.
pub struct LinePrompt {
    buf: String,
}

impl LinePrompt {
    pub fn new() -> LinePrompt {
        LinePrompt { buf: String::new() }
    }

    /// Feeds one keypress: digits append, Backspace trims. Anything else is
    /// ignored so a stray chord can't dismiss the prompt.
    pub fn key(&mut self, key: &KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Enter => return Outcome::Submit,
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Backspace => {
                self.buf.pop();
            }
            KeyCode::Char(ch)
                if ch.is_ascii_digit()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.buf.push(ch);
            }
            _ => {}
        }
        Outcome::Pending
    }

    /// Appends only the digits of a paste, matching what typing accepts.
    pub fn paste(&mut self, text: &str) {
        self.buf
            .extend(text.chars().filter(|ch| ch.is_ascii_digit()));
    }

    /// Writes the prompt into the notice; the caret sits at its end.
    pub fn render(&self, notice: &mut String) {
        notice.clear();
        notice.push_str("go to line: ");
        notice.push_str(&self.buf);
    }

    /// The typed number; `None` when empty. Overflow saturates — the caller
    /// clamps to the document anyway.
    pub fn line(&self) -> Option<usize> {
        if self.buf.is_empty() {
            return None;
        }
        Some(self.buf.parse().unwrap_or(usize::MAX))
    }
}

/// Completes the name after the last `/` in place: dotfiles stay hidden
/// unless asked for, and a directory that completes uniquely gains a
/// trailing slash, ready for the next segment.
fn complete_path(buf: &mut String) {
    let split = buf.rfind('/').map_or(0, |i| i + 1);
    let (dir, prefix) = buf.split_at(split);
    let dir = if dir.is_empty() { "." } else { dir };
    let Ok(entries) = fs::read_dir(Path::new(dir)) else {
        return;
    };
    let candidates = entries.filter_map(|entry| {
        let entry = entry.ok()?;
        let is_dir = entry.file_type().ok()?.is_dir();
        Some((entry.file_name().to_string_lossy().into_owned(), is_dir))
    });
    if let Some(replacement) = complete(prefix, candidates) {
        buf.truncate(split);
        buf.push_str(&replacement);
    }
}

/// What should replace `prefix`: the whole name on a unique match (plus a
/// slash for a directory), the longest common extension when several
/// match, nothing when none do or the prefix already is that extension.
fn complete(prefix: &str, candidates: impl Iterator<Item = (String, bool)>) -> Option<String> {
    let show_hidden = prefix.starts_with('.');
    let mut common: Option<(String, bool)> = None;
    let mut matches = 0;
    for (name, is_dir) in candidates {
        if !name.starts_with(prefix) || (!show_hidden && name.starts_with('.')) {
            continue;
        }
        matches += 1;
        common = Some(match common {
            None => (name, is_dir),
            Some((mut held, _)) => {
                held.truncate(common_prefix_len(&held, &name));
                (held, false)
            }
        });
    }
    let (mut name, is_dir) = common?;
    if matches == 1 {
        if is_dir {
            name.push('/');
        }
        Some(name)
    } else if name == prefix {
        None
    } else {
        Some(name)
    }
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = 0;
    for (ca, cb) in a.chars().zip(b.chars()) {
        if ca != cb {
            break;
        }
        len += ca.len_utf8();
    }
    len
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn cands(list: &[(&str, bool)]) -> impl Iterator<Item = (String, bool)> {
        list.iter()
            .map(|(name, is_dir)| (name.to_string(), *is_dir))
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn typing_appends_and_backspace_trims() {
        let mut prompt = PathPrompt::new("open: ");
        for ch in "srz".chars() {
            prompt.key(&press(KeyCode::Char(ch)));
        }
        prompt.key(&press(KeyCode::Backspace));
        let mut notice = String::new();
        prompt.render(&mut notice);
        assert_eq!(notice, "open: sr");

        // Backspacing past empty stays quiet.
        let mut prompt = PathPrompt::new("open: ");
        prompt.key(&press(KeyCode::Backspace));
        assert_eq!(prompt.into_path(), PathBuf::from(""));
    }

    #[test]
    fn chorded_characters_and_stray_keys_are_ignored() {
        let mut prompt = PathPrompt::new("open: ");
        prompt.key(&KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
        prompt.key(&KeyEvent::new(KeyCode::Char('x'), KeyModifiers::ALT));
        assert!(matches!(prompt.key(&press(KeyCode::Up)), Outcome::Pending));
        assert_eq!(prompt.into_path(), PathBuf::from(""));
    }

    #[test]
    fn enter_submits_and_esc_cancels() {
        let mut prompt = PathPrompt::new("open: ");
        assert!(matches!(
            prompt.key(&press(KeyCode::Enter)),
            Outcome::Submit
        ));
        assert!(matches!(prompt.key(&press(KeyCode::Esc)), Outcome::Cancel));
    }

    #[test]
    fn line_prompt_takes_digits_only() {
        let mut prompt = LinePrompt::new();
        for code in [
            KeyCode::Char('4'),
            KeyCode::Char('x'),
            KeyCode::Char('2'),
            KeyCode::Up,
        ] {
            assert!(matches!(prompt.key(&press(code)), Outcome::Pending));
        }
        prompt.key(&KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL));
        let mut notice = String::new();
        prompt.render(&mut notice);
        assert_eq!(notice, "go to line: 42");
        assert_eq!(prompt.line(), Some(42));

        prompt.key(&press(KeyCode::Backspace));
        prompt.key(&press(KeyCode::Backspace));
        assert_eq!(prompt.line(), None);
    }

    #[test]
    fn line_prompt_submits_cancels_and_saturates() {
        let mut prompt = LinePrompt::new();
        assert!(matches!(
            prompt.key(&press(KeyCode::Enter)),
            Outcome::Submit
        ));
        assert!(matches!(prompt.key(&press(KeyCode::Esc)), Outcome::Cancel));

        for _ in 0..25 {
            prompt.key(&press(KeyCode::Char('9')));
        }
        assert_eq!(prompt.line(), Some(usize::MAX));
    }

    #[test]
    fn paste_flattens_to_a_single_line() {
        let mut prompt = PathPrompt::new("open: ");
        prompt.paste("src/\nmain.rs\r\n");
        assert_eq!(prompt.into_path(), PathBuf::from("src/main.rs"));

        let mut prompt = LinePrompt::new();
        prompt.paste(" 4\n2 ");
        assert_eq!(prompt.line(), Some(42));
    }

    #[test]
    fn unique_match_completes_fully_and_dirs_gain_a_slash() {
        let list = [("main.rs", false), ("src", true)];
        assert_eq!(complete("ma", cands(&list)), Some("main.rs".into()));
        assert_eq!(complete("s", cands(&list)), Some("src/".into()));
    }

    #[test]
    fn several_matches_extend_to_the_common_prefix_then_hold() {
        let list = [("draw.rs", false), ("drop.rs", false), ("main.rs", false)];
        assert_eq!(complete("d", cands(&list)), Some("dr".into()));
        // No progress to make: a second Tab changes nothing.
        assert_eq!(complete("dr", cands(&list)), None);
    }

    #[test]
    fn no_match_completes_nothing() {
        assert_eq!(complete("zz", cands(&[("main.rs", false)])), None);
    }

    #[test]
    fn dotfiles_stay_hidden_unless_asked_for() {
        let list = [(".gitignore", false), ("git.rs", false)];
        assert_eq!(complete("", cands(&list)), Some("git.rs".into()));
        assert_eq!(complete(".", cands(&list)), Some(".gitignore".into()));
    }

    #[test]
    fn tab_completes_against_the_filesystem() {
        let dir = std::env::temp_dir().join(format!("connor-complete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a_long.txt"), "").unwrap();
        std::fs::write(dir.join("a_lot.txt"), "").unwrap();

        let mut prompt = PathPrompt::new("open: ");
        for ch in format!("{}/a_l", dir.display()).chars() {
            prompt.key(&press(KeyCode::Char(ch)));
        }
        prompt.key(&press(KeyCode::Tab));
        assert_eq!(prompt.into_path(), dir.join("a_lo"));

        let mut prompt = PathPrompt::new("open: ");
        for ch in format!("{}/su", dir.display()).chars() {
            prompt.key(&press(KeyCode::Char(ch)));
        }
        prompt.key(&press(KeyCode::Tab));
        assert_eq!(prompt.into_path(), dir.join("sub/"));

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
