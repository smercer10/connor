use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufReader, ErrorKind};
use std::path::PathBuf;

use ropey::Rope;

/// Lines examined by convention detection: plenty for any real file, a
/// bound for absurd ones.
const DETECT_LINE_CAP: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineEnding {
    Lf,
    Crlf,
}

impl LineEnding {
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IndentStyle {
    Tabs,
    Spaces(u8),
}

impl IndentStyle {
    /// What the Tab key inserts.
    pub fn as_str(self) -> &'static str {
        match self {
            IndentStyle::Tabs => "\t",
            IndentStyle::Spaces(n) => &"        "[..usize::from(n)],
        }
    }
}

/// The majority terminator among the file's lines; ties and empty files
/// fall back to LF.
fn detect_line_ending(rope: &Rope) -> LineEnding {
    let mut crlf = 0;
    let mut lf = 0;
    // Every line but the last ends in '\n'; a '\r' before it makes CRLF.
    for line in 0..rope.len_lines().saturating_sub(1).min(DETECT_LINE_CAP) {
        let end = rope.line_to_char(line + 1);
        if end >= 2 && rope.char(end - 2) == '\r' {
            crlf += 1;
        } else {
            lf += 1;
        }
    }
    if crlf > lf {
        LineEnding::Crlf
    } else {
        LineEnding::Lf
    }
}

/// Tabs when more lines open with a tab than with spaces; otherwise spaces,
/// their width the most common growth in leading spaces from one
/// space-indented line to the next (how deeper nesting reveals the step).
/// Undetectable width falls back to 4.
fn detect_indent(rope: &Rope) -> IndentStyle {
    let mut tabs = 0;
    let mut spaces = 0;
    let mut steps = [0usize; 9];
    let mut prev_width = 0;
    for line in rope.lines().take(rope.len_lines().min(DETECT_LINE_CAP)) {
        match line.chars().next() {
            Some('\t') => tabs += 1,
            Some(' ') => spaces += 1,
            _ => continue,
        }
        let width = line.chars().take_while(|&ch| ch == ' ').take(64).count();
        if width > 0 {
            let step = width.saturating_sub(prev_width);
            if (1..=8).contains(&step) {
                steps[step] += 1;
            }
            prev_width = width;
        }
    }
    if tabs > spaces {
        return IndentStyle::Tabs;
    }
    // Smallest step wins ties: a two-level jump must not read as one step.
    let mut best = (4, 0);
    for (step, &count) in steps.iter().enumerate().skip(1) {
        if count > best.1 {
            best = (step, count);
        }
    }
    IndentStyle::Spaces(best.0 as u8)
}

/// One open file: the text plus everything that belongs to the file rather
/// than to a view of it (path, dirty state, how it was loaded, its
/// conventions).
pub struct Document {
    rope: Rope,
    path: Option<PathBuf>,
    dirty: bool,
    lossy: bool,
    line_ending: LineEnding,
    indent: IndentStyle,
}

impl Document {
    pub fn empty() -> Self {
        Document {
            rope: Rope::new(),
            path: None,
            dirty: false,
            lossy: false,
            line_ending: LineEnding::Lf,
            indent: IndentStyle::Spaces(4),
        }
    }

    /// Opens `path`. A missing file yields an empty document carrying that
    /// path, so saving can create it. Invalid UTF-8 loads lossily (bad bytes
    /// become U+FFFD) and is flagged so the status line can say so.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        let (rope, lossy) = match File::open(&path) {
            Ok(file) => match Rope::from_reader(BufReader::new(file)) {
                Ok(rope) => (rope, false),
                Err(e) if e.kind() == ErrorKind::InvalidData => {
                    let bytes = std::fs::read(&path)?;
                    (Rope::from_str(&String::from_utf8_lossy(&bytes)), true)
                }
                Err(e) => return Err(e),
            },
            Err(e) if e.kind() == ErrorKind::NotFound => (Rope::new(), false),
            Err(e) => return Err(e),
        };
        Ok(Document {
            line_ending: detect_line_ending(&rope),
            indent: detect_indent(&rope),
            rope,
            path: Some(path),
            dirty: false,
            lossy,
        })
    }

    #[cfg(test)]
    pub fn from_str(text: &str) -> Self {
        let rope = Rope::from_str(text);
        Document {
            line_ending: detect_line_ending(&rope),
            indent: detect_indent(&rope),
            rope,
            ..Document::empty()
        }
    }

    #[cfg(test)]
    pub fn set_lossy(&mut self, lossy: bool) {
        self.lossy = lossy;
    }

    pub fn rope(&self) -> &Rope {
        &self.rope
    }

    pub fn name(&self) -> Cow<'_, str> {
        match &self.path {
            Some(path) => path
                .file_name()
                .unwrap_or(path.as_os_str())
                .to_string_lossy(),
            None => Cow::Borrowed("[No Name]"),
        }
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    pub fn lossy(&self) -> bool {
        self.lossy
    }

    pub fn line_ending(&self) -> LineEnding {
        self.line_ending
    }

    pub fn indent(&self) -> IndentStyle {
        self.indent
    }

    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.rope.line_to_char(line)
    }

    /// The char index just past the line's text — before its `\n` or `\r\n`
    /// terminator, which the cursor never enters and the screen never shows.
    pub fn line_end(&self, line: usize) -> usize {
        let start = self.rope.line_to_char(line);
        let mut end = self.rope.line_to_char(line + 1);
        if end > start && self.rope.char(end - 1) == '\n' {
            end -= 1;
            if end > start && self.rope.char(end - 1) == '\r' {
                end -= 1;
            }
        }
        end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_has_one_empty_line() {
        let doc = Document::empty();
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.line_start(0), 0);
        assert_eq!(doc.line_end(0), 0);
        assert_eq!(doc.name(), "[No Name]");
        assert!(!doc.dirty());
    }

    #[test]
    fn line_end_stops_before_lf_and_crlf() {
        let doc = Document::from_str("ab\ncd\r\nef");
        assert_eq!(doc.line_count(), 3);
        assert_eq!(doc.line_end(0), 2);
        assert_eq!(doc.line_start(1), 3);
        assert_eq!(doc.line_end(1), 5);
        assert_eq!(doc.line_start(2), 7);
        assert_eq!(doc.line_end(2), 9);
    }

    #[test]
    fn trailing_newline_yields_an_empty_last_line() {
        let doc = Document::from_str("ab\n");
        assert_eq!(doc.line_count(), 2);
        assert_eq!(doc.line_start(1), 3);
        assert_eq!(doc.line_end(1), 3);
    }

    #[test]
    fn empty_lines_terminate_correctly() {
        let doc = Document::from_str("\n\r\n");
        assert_eq!(doc.line_count(), 3);
        assert_eq!(doc.line_end(0), 0);
        assert_eq!(doc.line_start(1), 1);
        assert_eq!(doc.line_end(1), 1);
    }

    #[test]
    fn line_ending_detection_takes_the_majority() {
        assert_eq!(
            Document::from_str("a\nb\nc\n").line_ending(),
            LineEnding::Lf
        );
        let crlf = Document::from_str("a\r\nb\r\nc");
        assert_eq!(crlf.line_ending(), LineEnding::Crlf);
        let mixed = Document::from_str("a\r\nb\nc\r\nd\r\n");
        assert_eq!(mixed.line_ending(), LineEnding::Crlf);
        let tie = Document::from_str("a\r\nb\nc");
        assert_eq!(tie.line_ending(), LineEnding::Lf);
        assert_eq!(Document::empty().line_ending(), LineEnding::Lf);
        assert_eq!(LineEnding::Crlf.as_str(), "\r\n");
    }

    #[test]
    fn indent_detection_votes_tabs_versus_spaces() {
        let tabs = Document::from_str("fn x\n\ta\n\tb\n");
        assert_eq!(tabs.indent(), IndentStyle::Tabs);
        let spaces = Document::from_str("fn x\n  a\n  b\n");
        assert_eq!(spaces.indent(), IndentStyle::Spaces(2));
        let majority = Document::from_str("\ta\n\tb\n  c\n");
        assert_eq!(majority.indent(), IndentStyle::Tabs);
        assert_eq!(IndentStyle::Tabs.as_str(), "\t");
        assert_eq!(IndentStyle::Spaces(3).as_str(), "   ");
    }

    #[test]
    fn indent_width_follows_nesting_steps() {
        let four = Document::from_str("a\n    b\n        c\n    d\n");
        assert_eq!(four.indent(), IndentStyle::Spaces(4));
        let two = Document::from_str("x\n  a\n    b\n      c\n");
        assert_eq!(two.indent(), IndentStyle::Spaces(2));
    }

    #[test]
    fn undetectable_indent_falls_back_to_four_spaces() {
        assert_eq!(Document::empty().indent(), IndentStyle::Spaces(4));
        let flat = Document::from_str("flat\nlines\nonly\n");
        assert_eq!(flat.indent(), IndentStyle::Spaces(4));
    }

    #[test]
    fn open_reads_missing_and_lossy_files() {
        let dir = std::env::temp_dir().join(format!("connor-doc-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let valid = dir.join("valid.txt");
        std::fs::write(&valid, "hello\nworld\n").unwrap();
        let doc = Document::open(valid).unwrap();
        assert_eq!(doc.rope().to_string(), "hello\nworld\n");
        assert!(!doc.lossy());
        assert_eq!(doc.name(), "valid.txt");

        let missing = dir.join("missing.txt");
        let doc = Document::open(missing.clone()).unwrap();
        assert_eq!(doc.line_count(), 1);
        assert_eq!(doc.name(), "missing.txt");
        assert!(!missing.exists());

        let garbage = dir.join("garbage.bin");
        std::fs::write(&garbage, b"ok\xFF\xFEbad\n").unwrap();
        let doc = Document::open(garbage).unwrap();
        assert!(doc.lossy());
        assert_eq!(doc.rope().to_string(), "ok\u{FFFD}\u{FFFD}bad\n");
        assert!(!doc.dirty());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
