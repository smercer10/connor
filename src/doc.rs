use std::borrow::Cow;
use std::fs::File;
use std::io::{self, BufReader, ErrorKind};
use std::path::PathBuf;

use ropey::Rope;

/// One open file: the text plus everything that belongs to the file rather
/// than to a view of it (path, dirty state, how it was loaded).
pub struct Document {
    rope: Rope,
    path: Option<PathBuf>,
    dirty: bool,
    lossy: bool,
}

impl Document {
    pub fn empty() -> Self {
        Document {
            rope: Rope::new(),
            path: None,
            dirty: false,
            lossy: false,
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
            rope,
            path: Some(path),
            dirty: false,
            lossy,
        })
    }

    #[cfg(test)]
    pub fn from_str(text: &str) -> Self {
        Document {
            rope: Rope::from_str(text),
            ..Document::empty()
        }
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
