//! Incremental syntax highlighting. A `Syntax` owns the tree-sitter state
//! for one document: it drains the document's splice log to shift the tree,
//! reparses in place when the damage is small and hands big parses to a
//! worker thread, and turns the visible viewport into colour spans for the
//! draw pass. Documents with no grammar never construct one, so plain text
//! costs nothing.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, OnceLock};
use std::thread;

use ropey::Rope;
use tree_sitter::{
    InputEdit, Language, Node, ParseOptions, ParseState, Parser, Point, Query, QueryCursor,
    StreamingIterator as _, Tree,
};
use tree_sitter_language::LanguageFn;

use crate::doc::{Document, Splice};
use crate::watch::AppEvent;

// Vendored grammar (grammars/dockerfile), compiled by build.rs; the
// crates.io crate pins an incompatible tree-sitter.
unsafe extern "C" {
    fn tree_sitter_dockerfile() -> *const ();
}
const DOCKERFILE: LanguageFn = unsafe { LanguageFn::from_raw(tree_sitter_dockerfile) };

/// One coloured stretch of the document in char indices; spans are sorted
/// and non-overlapping, so the draw pass consumes them with a single
/// advancing index. `color` uses the `Cell::fg` encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub color: u8,
}

/// Capture-name prefixes to `Cell::fg` codes. Restrained on purpose:
/// variables, properties, punctuation and operators stay default, and only
/// the base ANSI palette is used so the terminal theme keeps both light and
/// dark schemes readable.
const PALETTE: &[(&str, u8)] = &[
    ("keyword", 6), // dark magenta
    ("string", 3),  // dark green
    ("escape", 3),
    ("comment", 9),  // bright black
    ("function", 5), // dark blue
    ("type", 4),     // dark yellow
    ("constructor", 4),
    ("constant", 2), // dark red
    ("number", 2),
    ("boolean", 2),
    ("attribute", 7), // dark cyan
    ("label", 7),
    ("text.title", 6),   // markdown headings
    ("text.literal", 7), // markdown code blocks
    ("text.uri", 5),
    ("text.reference", 7),
    ("punctuation.special", 4), // markdown list and quote markers
    ("none", 0),                // explicitly plain: punches a hole in an outer capture
];

/// A capture with no palette entry — ignored entirely, so an enclosing
/// coloured capture shows through. Distinct from an explicit 0 ("none"),
/// which overrides the enclosing colour with the default.
const UNMAPPED: u8 = u8::MAX;

/// Longest dotted prefix of `name` with a palette entry, so
/// `comment.documentation` follows `comment` until it earns its own row.
fn color_of(name: &str) -> u8 {
    let mut prefix = name;
    loop {
        if let Some(&(_, color)) = PALETTE.iter().find(|(p, _)| *p == prefix) {
            return color;
        }
        match prefix.rfind('.') {
            Some(dot) => prefix = &prefix[..dot],
            None => return UNMAPPED,
        }
    }
}

/// A grammar plus its highlight query, with every capture index resolved to
/// a colour up front. Built once per language and shared by every document.
pub struct Lang {
    language: Language,
    query: Query,
    colors: Vec<u8>,
}

impl Lang {
    fn new(language: Language, query_src: &str) -> Lang {
        let query = Query::new(&language, query_src).expect("grammar ships a valid query");
        let colors = query.capture_names().iter().map(|n| color_of(n)).collect();
        Lang {
            language,
            query,
            colors,
        }
    }
}

/// One bundled language: how to recognise its files and how to build its
/// `Lang`, which the `OnceLock` slot memoizes on first use so unopened
/// languages never compile their query.
struct LangSpec {
    /// Identifies the language in test assertions and audit output.
    #[cfg_attr(not(test), expect(dead_code))]
    name: &'static str,
    files: Files,
    language: LanguageFn,
    /// Query source parts, concatenated at init — C++ prepends C's query
    /// because its grammar inherits C's nodes but its query doesn't.
    query: &'static [&'static str],
    lang: OnceLock<Lang>,
}

/// How a language's files are recognised; table entries spell only the
/// rules they use via `..NO_FILES`.
struct Files {
    exts: &'static [&'static str],
    filenames: &'static [&'static str],
    filename_prefixes: &'static [&'static str],
    shebangs: &'static [&'static str],
}

const NO_FILES: Files = Files {
    exts: &[],
    filenames: &[],
    filename_prefixes: &[],
    shebangs: &[],
};

static LANGS: [LangSpec; 12] = [
    LangSpec {
        name: "rust",
        files: Files {
            exts: &["rs"],
            ..NO_FILES
        },
        language: tree_sitter_rust::LANGUAGE,
        query: &[tree_sitter_rust::HIGHLIGHTS_QUERY],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "go",
        files: Files {
            exts: &["go"],
            ..NO_FILES
        },
        language: tree_sitter_go::LANGUAGE,
        query: &[tree_sitter_go::HIGHLIGHTS_QUERY],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "python",
        files: Files {
            exts: &["py", "pyi"],
            shebangs: &["python"],
            ..NO_FILES
        },
        language: tree_sitter_python::LANGUAGE,
        query: &[tree_sitter_python::HIGHLIGHTS_QUERY],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "bash",
        files: Files {
            exts: &["sh", "bash"],
            filenames: &[".bashrc", ".bash_profile", ".bash_aliases", ".profile"],
            shebangs: &["sh", "bash", "dash", "zsh"],
            ..NO_FILES
        },
        language: tree_sitter_bash::LANGUAGE,
        query: &[tree_sitter_bash::HIGHLIGHT_QUERY],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "c",
        files: Files {
            exts: &["c", "h"],
            ..NO_FILES
        },
        language: tree_sitter_c::LANGUAGE,
        query: &[tree_sitter_c::HIGHLIGHT_QUERY],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "cpp",
        files: Files {
            exts: &["cpp", "cc", "cxx", "hpp", "hh", "hxx"],
            ..NO_FILES
        },
        language: tree_sitter_cpp::LANGUAGE,
        query: &[
            tree_sitter_c::HIGHLIGHT_QUERY,
            "\n",
            tree_sitter_cpp::HIGHLIGHT_QUERY,
        ],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "yaml",
        files: Files {
            exts: &["yml", "yaml"],
            ..NO_FILES
        },
        language: tree_sitter_yaml::LANGUAGE,
        query: &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "hcl",
        files: Files {
            exts: &["hcl", "tf", "tfvars"],
            ..NO_FILES
        },
        language: tree_sitter_hcl::LANGUAGE,
        query: &[include_str!("../grammars/hcl/highlights.scm")],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "dockerfile",
        files: Files {
            exts: &["dockerfile"],
            filenames: &["Dockerfile", "Containerfile"],
            filename_prefixes: &["Dockerfile."],
            ..NO_FILES
        },
        language: DOCKERFILE,
        query: &[include_str!("../grammars/dockerfile/highlights.scm")],
        lang: OnceLock::new(),
    },
    // Block grammar only — headings, lists, quotes, fences. Inline
    // emphasis needs the bundled second grammar parsed over included
    // ranges; that is #34.
    LangSpec {
        name: "markdown",
        files: Files {
            exts: &["md", "markdown"],
            ..NO_FILES
        },
        language: tree_sitter_md::LANGUAGE,
        query: &[tree_sitter_md::HIGHLIGHT_QUERY_BLOCK],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "toml",
        files: Files {
            exts: &["toml"],
            filenames: &["Cargo.lock"],
            ..NO_FILES
        },
        language: tree_sitter_toml_ng::LANGUAGE,
        query: &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
        lang: OnceLock::new(),
    },
    LangSpec {
        name: "json",
        files: Files {
            exts: &["json"],
            ..NO_FILES
        },
        language: tree_sitter_json::LANGUAGE,
        query: &[tree_sitter_json::HIGHLIGHTS_QUERY],
        lang: OnceLock::new(),
    },
];

/// The grammar for a document: well-known filename first (including the
/// `Dockerfile.*` family), then extension, then — only when the path
/// decides nothing — the shebang line.
fn lang_for(path: Option<&Path>, rope: &Rope) -> Option<&'static Lang> {
    let spec = path
        .and_then(spec_for_path)
        .or_else(|| spec_for_shebang(rope))?;
    Some(
        spec.lang
            .get_or_init(|| Lang::new(spec.language.into(), &spec.query.concat())),
    )
}

fn spec_for_path(path: &Path) -> Option<&'static LangSpec> {
    let name = path.file_name()?.to_str()?;
    let by_name = LANGS.iter().find(|s| {
        s.files.filenames.contains(&name)
            || s.files
                .filename_prefixes
                .iter()
                .any(|p| name.starts_with(p))
    });
    if by_name.is_some() {
        return by_name;
    }
    let ext = path.extension()?.to_str()?;
    LANGS.iter().find(|s| s.files.exts.contains(&ext))
}

fn spec_for_shebang(rope: &Rope) -> Option<&'static LangSpec> {
    let line: String = rope.line(0).chars().take(128).collect();
    let mut words = line.strip_prefix("#!")?.split_whitespace();
    let mut interp = words.next()?;
    if interp.rsplit('/').next() == Some("env") {
        interp = words.find(|w| !w.starts_with('-'))?;
    }
    // The basename, minus any version suffix: python3.12 detects as python.
    let interp = interp
        .rsplit('/')
        .next()?
        .trim_end_matches(|c: char| c.is_ascii_digit() || c == '.');
    LANGS.iter().find(|s| s.files.shebangs.contains(&interp))
}

/// Edits whose combined byte span stays under this reparse synchronously on
/// the event path; anything bigger — including a first parse of a larger
/// file — goes to a worker thread so no frame blocks on it. Sized so the
/// worst sync parse stays well under a frame's budget at tree-sitter's
/// measured ~8 MB/s.
const SYNC_PARSE_LIMIT: usize = 128 * 1024;

/// A finished background parse on its way back to the main loop.
pub struct ParseDone {
    pub doc_id: u64,
    generation: u64,
    tree: Tree,
}

/// A background parse in progress. Splices applied to the document after
/// its rope snapshot queue here and replay onto the arriving tree.
struct Inflight {
    generation: u64,
    cancel: Arc<AtomicBool>,
    since: Vec<Splice>,
}

impl Drop for Inflight {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub struct Syntax {
    lang: &'static Lang,
    parser: Parser,
    tree: Option<Tree>,
    /// Bytes of edits applied to `tree` since it last reparsed — the cost
    /// an incremental reparse is proportional to.
    stale_damage: usize,
    /// Bumped whenever `tree` changes; part of the span-cache key.
    parse_seq: u64,
    /// Tags background parses so a superseded worker's result is dropped.
    generation: u64,
    inflight: Option<Inflight>,
    cursor: QueryCursor,
    spans: Vec<Span>,
    span_key: Option<(u64, u64, usize, usize)>,
}

impl Syntax {
    /// A highlighter for the document, or `None` when no grammar covers its
    /// path or shebang. Starts the document's splice log; the first `pump`
    /// parses.
    pub fn new(doc: &mut Document) -> Option<Syntax> {
        let lang = lang_for(doc.path(), doc.rope())?;
        let mut parser = Parser::new();
        parser.set_language(&lang.language).ok()?;
        doc.track_splices();
        Some(Syntax {
            lang,
            parser,
            tree: None,
            stale_damage: 0,
            parse_seq: 0,
            generation: 0,
            inflight: None,
            cursor: QueryCursor::new(),
            spans: Vec::new(),
            span_key: None,
        })
    }

    /// Catches the tree up with the document: applies every recorded splice,
    /// then reparses — in place when cheap, on a worker when not. Called
    /// before each frame of the active tab; a quiet document is a no-op.
    pub fn pump(&mut self, doc: &mut Document, tx: &Sender<AppEvent>) {
        let (splices, overflowed) = doc.take_splices();
        if overflowed {
            // The log is incomplete: neither the tree nor an in-flight
            // snapshot can be caught up by replay. Rebuild from the rope.
            self.tree = None;
            self.stale_damage = 0;
            self.inflight = None;
        }
        for s in splices {
            if let Some(tree) = &mut self.tree {
                tree.edit(&input_edit(&s));
            }
            if let Some(inflight) = &mut self.inflight {
                inflight.since.push(s);
            }
            self.stale_damage += s.old_end_byte.max(s.new_end_byte) - s.start_byte;
        }
        self.schedule(doc, tx);
    }

    /// Installs a background parse's tree if it is still the awaited one,
    /// replaying the splices that landed while it ran. Returns whether the
    /// highlighting changed.
    pub fn absorb(&mut self, done: ParseDone, doc: &Document, tx: &Sender<AppEvent>) -> bool {
        if !self
            .inflight
            .as_ref()
            .is_some_and(|i| i.generation == done.generation)
        {
            return false;
        }
        let inflight = self.inflight.take().unwrap();
        let mut tree = done.tree;
        let mut damage = 0;
        for s in &inflight.since {
            tree.edit(&input_edit(s));
            damage += s.old_end_byte.max(s.new_end_byte) - s.start_byte;
        }
        self.tree = Some(tree);
        self.stale_damage = damage;
        self.parse_seq += 1;
        // Catch-up reparse for the replayed splices; in place when small.
        self.schedule(doc, tx);
        true
    }

    /// Reparses if the tree lags the document. Damage-proportional work runs
    /// in place; a first parse of a large file or a huge splice goes to a
    /// worker, and the edited old tree keeps yielding correctly shifted
    /// spans until the result lands. One worker at a time: splices queue on
    /// it and `absorb` reschedules, so a burst can't stack threads.
    fn schedule(&mut self, doc: &Document, tx: &Sender<AppEvent>) {
        if self.inflight.is_some() || (self.tree.is_some() && self.stale_damage == 0) {
            return;
        }
        let rope = doc.rope();
        let cost = if self.tree.is_some() {
            self.stale_damage
        } else {
            rope.len_bytes()
        };
        if cost <= SYNC_PARSE_LIMIT {
            let tree = self.parser.parse_with_options(
                &mut |byte, _| chunk_at(rope, byte),
                self.tree.as_ref(),
                None,
            );
            if let Some(tree) = tree {
                self.tree = Some(tree);
                self.stale_damage = 0;
                self.parse_seq += 1;
            }
        } else {
            self.generation += 1;
            let cancel = spawn_parse(
                rope.clone(),
                self.tree.clone(),
                self.lang,
                doc.id(),
                self.generation,
                tx.clone(),
            );
            self.inflight = Some(Inflight {
                generation: self.generation,
                cancel,
                since: Vec::new(),
            });
        }
    }

    /// Rebuilds the span cache for the visible lines unless document, tree
    /// and viewport are all unchanged. Runs the highlight query over just
    /// the viewport's byte range and flattens nested captures into sorted,
    /// non-overlapping spans; never called from the draw pass itself.
    pub fn refresh(&mut self, doc: &Document, scroll_line: usize, text_h: usize) {
        let key = (doc.revision(), self.parse_seq, scroll_line, text_h);
        if self.span_key == Some(key) {
            return;
        }
        self.span_key = Some(key);
        self.spans.clear();
        let Some(tree) = &self.tree else {
            return;
        };
        let rope = doc.rope();
        let start_byte = line_byte(rope, scroll_line);
        let end_byte = line_byte(rope, scroll_line + text_h);
        if start_byte >= end_byte {
            return;
        }
        self.cursor.set_byte_range(start_byte..end_byte);
        let provider = |node: Node| {
            rope.byte_slice(node.byte_range())
                .chunks()
                .map(str::as_bytes)
        };
        let mut captures = self
            .cursor
            .captures(&self.lang.query, tree.root_node(), provider);

        let spans = &mut self.spans;
        let mut emit = |from: usize, to: usize, color: u8| {
            let to = to.min(end_byte);
            if color != 0 && from < to {
                spans.push(Span {
                    start: rope.byte_to_char(from),
                    end: rope.byte_to_char(to),
                    color,
                });
            }
        };
        // Captures arrive sorted by start and properly nested (they are
        // tree nodes), enclosing ones first. A small stack flattens them,
        // splitting an outer capture around an inner one — plain last-wins
        // would swallow a string's tail after its escape sequence — with
        // the innermost capture owning each byte.
        let mut stack: Vec<(usize, u8)> = Vec::new();
        let mut pos = start_byte;
        while let Some((m, ci)) = captures.next() {
            let capture = m.captures[*ci];
            let color = self.lang.colors[capture.index as usize];
            if color == UNMAPPED {
                continue;
            }
            let range = capture.node.byte_range();
            while stack.last().is_some_and(|&(end, _)| end <= range.start) {
                let (end, color) = stack.pop().unwrap();
                emit(pos, end, color);
                pos = pos.max(end);
            }
            if let Some(&(_, outer)) = stack.last() {
                emit(pos, range.start, outer);
            }
            pos = pos.max(range.start);
            stack.push((range.end, color));
        }
        while let Some((end, color)) = stack.pop() {
            emit(pos, end, color);
            pos = pos.max(end);
        }
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }
}

fn input_edit(s: &Splice) -> InputEdit {
    InputEdit {
        start_byte: s.start_byte,
        old_end_byte: s.old_end_byte,
        new_end_byte: s.new_end_byte,
        start_position: point(s.start_point),
        old_end_position: point(s.old_end_point),
        new_end_position: point(s.new_end_point),
    }
}

fn point((row, column): (usize, usize)) -> Point {
    Point { row, column }
}

/// The rope's text from `byte` onward, one chunk at a time — the shape
/// tree-sitter's streaming parse wants.
fn chunk_at(rope: &Rope, byte: usize) -> &[u8] {
    if byte >= rope.len_bytes() {
        return &[];
    }
    let (chunk, start, _, _) = rope.chunk_at_byte(byte);
    &chunk.as_bytes()[byte - start..]
}

/// The byte where `line` starts; lines past the end clamp to the rope's end.
fn line_byte(rope: &Rope, line: usize) -> usize {
    if line >= rope.len_lines() {
        rope.len_bytes()
    } else {
        rope.line_to_byte(line)
    }
}

/// Parses on a worker thread — rope and tree clones are cheap shared
/// handles — and sends the result home tagged with its generation. The
/// cancel flag stops a superseded parse at its next progress check.
fn spawn_parse(
    rope: Rope,
    old_tree: Option<Tree>,
    lang: &'static Lang,
    doc_id: u64,
    generation: u64,
    tx: Sender<AppEvent>,
) -> Arc<AtomicBool> {
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    thread::spawn(move || {
        let mut parser = Parser::new();
        if parser.set_language(&lang.language).is_err() {
            return;
        }
        let mut progress = |_: &ParseState| {
            if flag.load(Ordering::Relaxed) {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let options = ParseOptions::new().progress_callback(&mut progress);
        let tree = parser.parse_with_options(
            &mut |byte, _| chunk_at(&rope, byte),
            old_tree.as_ref(),
            Some(options),
        );
        if let Some(tree) = tree {
            let _ = tx.send(AppEvent::Parsed(ParseDone {
                doc_id,
                generation,
                tree,
            }));
        }
    });
    cancel
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use crate::doc::{Caret, EditKind};

    fn caret(cursor: usize) -> Caret {
        Caret {
            cursor,
            anchor: None,
        }
    }

    fn doc_named(name: &str, text: &str) -> (Document, Syntax) {
        let mut doc = Document::from_str(text);
        doc.set_path(PathBuf::from(name));
        let syntax = Syntax::new(&mut doc).unwrap();
        (doc, syntax)
    }

    /// Pumps, refreshes the first `lines` lines, and returns the spans.
    fn spans_for(doc: &mut Document, syntax: &mut Syntax, lines: usize) -> Vec<Span> {
        let (tx, _rx) = mpsc::channel();
        syntax.pump(doc, &tx);
        syntax.refresh(doc, 0, lines);
        syntax.spans().to_vec()
    }

    fn span(start: usize, end: usize, color: u8) -> Span {
        Span { start, end, color }
    }

    #[test]
    fn unknown_extensions_and_pathless_buffers_get_no_highlighter() {
        let mut doc = Document::from_str("fn main() {}");
        assert!(Syntax::new(&mut doc).is_none());
        doc.set_path(PathBuf::from("t.txt"));
        assert!(Syntax::new(&mut doc).is_none());
    }

    #[test]
    fn capture_names_resolve_by_longest_dotted_prefix() {
        assert_eq!(color_of("keyword"), 6);
        assert_eq!(color_of("comment.documentation"), 9);
        assert_eq!(color_of("function.macro"), 5);
        assert_eq!(color_of("none"), 0);
        assert_eq!(color_of("variable.builtin"), UNMAPPED);
        assert_eq!(color_of("operator"), UNMAPPED);
    }

    #[test]
    fn rust_code_yields_the_expected_spans() {
        let (mut doc, mut syntax) = doc_named("t.rs", "fn main() {}");
        let spans = spans_for(&mut doc, &mut syntax, 10);
        assert_eq!(spans, vec![span(0, 2, 6), span(3, 7, 5)]);
    }

    #[test]
    fn an_inner_capture_splits_its_enclosing_span() {
        let (mut doc, mut syntax) = doc_named("t.rs", r#"let s = "a\nb";"#);
        let spans = spans_for(&mut doc, &mut syntax, 10);
        // The escape sequence splits the string span in three; here inner
        // and outer share a colour, but the split is what guarantees a
        // string's tail keeps its colour after any escape.
        assert_eq!(
            spans,
            vec![
                span(0, 3, 6),
                span(8, 10, 3),
                span(10, 12, 3),
                span(12, 14, 3),
            ]
        );
    }

    #[test]
    fn markdown_headings_colour_and_fence_content_stays_plain() {
        let (mut doc, mut syntax) = doc_named("t.md", "# Title\n```rust\nlet x;\n```\n");
        let spans = spans_for(&mut doc, &mut syntax, 10);
        // The heading marker and text colour; the fenced block is literal
        // except its content, whose explicit `none` capture punches a hole.
        assert!(spans.iter().any(|s| s.color == 6 && s.start < 7));
        assert!(
            !spans
                .iter()
                .any(|s| s.start <= 16 && 22 <= s.end && s.color != 0),
            "fence content must stay plain: {spans:?}"
        );
    }

    #[test]
    fn edits_shift_spans_incrementally() {
        let (mut doc, mut syntax) = doc_named("t.rs", "fn main() {}");
        spans_for(&mut doc, &mut syntax, 10);
        // A comment line inserted at the front pushes everything down; the
        // old tree is edited and reparsed, not rebuilt blind.
        doc.edit(0..0, "// c\n", caret(0), EditKind::Other);
        let spans = spans_for(&mut doc, &mut syntax, 10);
        assert_eq!(spans, vec![span(0, 4, 9), span(5, 7, 6), span(8, 12, 5)]);
    }

    #[test]
    fn undo_and_redo_keep_spans_correct() {
        let (mut doc, mut syntax) = doc_named("t.rs", "fn main() {}");
        let before = spans_for(&mut doc, &mut syntax, 10);
        doc.edit(0..0, "// c\n", caret(0), EditKind::Other);
        let edited = spans_for(&mut doc, &mut syntax, 10);
        doc.undo();
        assert_eq!(spans_for(&mut doc, &mut syntax, 10), before);
        doc.redo();
        assert_eq!(spans_for(&mut doc, &mut syntax, 10), edited);
    }

    #[test]
    fn external_reload_keeps_spans_correct() {
        let dir = std::env::temp_dir().join(format!("connor-syn-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let mut doc = Document::open(path.clone()).unwrap();
        let mut syntax = Syntax::new(&mut doc).unwrap();
        spans_for(&mut doc, &mut syntax, 10);

        std::fs::write(&path, "const N: u8 = 1;\nfn main() {}\n").unwrap();
        assert!(matches!(
            doc.check_disk(caret(0)),
            crate::doc::DiskCheck::Reloaded { .. }
        ));
        let spans = spans_for(&mut doc, &mut syntax, 10);
        // The reload's single splice reparsed cleanly: the new first line
        // colours and main's spans shifted one line down.
        assert!(spans.contains(&span(0, 5, 6))); // const
        assert!(spans.contains(&span(17, 19, 6))); // fn
        assert!(spans.contains(&span(20, 24, 5))); // main
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_the_viewport_gets_spans() {
        let text = "fn a() {}\n".repeat(50);
        let (mut doc, mut syntax) = doc_named("t.rs", &text);
        let (tx, _rx) = mpsc::channel();
        syntax.pump(&mut doc, &tx);
        syntax.refresh(&doc, 10, 5);
        let spans = syntax.spans().to_vec();
        assert!(!spans.is_empty());
        let (start, end) = (doc.line_start(10), doc.line_start(15));
        assert!(spans.iter().all(|s| start <= s.start && s.end <= end));

        // Past the end of the file there is nothing to colour.
        syntax.refresh(&doc, 100, 5);
        assert!(syntax.spans().is_empty());
    }

    #[test]
    fn parses_across_rope_chunk_seams() {
        let mut text = String::new();
        for i in 0..2000 {
            text.push_str(&format!("fn f{i}() {{}}\n"));
        }
        let (mut doc, mut syntax) = doc_named("t.rs", &text);
        assert!(doc.rope().chunks().count() > 1);
        let spans = spans_for(&mut doc, &mut syntax, 3);
        assert_eq!(spans[0], span(0, 2, 6));
        assert_eq!(spans[1].color, 5);
    }

    #[test]
    fn a_big_file_parses_in_the_background_and_absorbs_with_catch_up() {
        let mut text = "fn main() {}\n".repeat(SYNC_PARSE_LIMIT / 13 + 1);
        assert!(text.len() > SYNC_PARSE_LIMIT);
        text.insert_str(0, "// lead\n");
        let (mut doc, mut syntax) = doc_named("t.rs", &text);
        let (tx, rx) = mpsc::channel();
        syntax.pump(&mut doc, &tx);
        syntax.refresh(&doc, 0, 5);
        // No tree yet: the parse is on the worker and the file stays plain.
        assert!(syntax.spans().is_empty());

        // An edit while the parse runs queues and replays on the result.
        doc.edit(0..0, "// c\n", caret(0), EditKind::Other);
        syntax.pump(&mut doc, &tx);

        let AppEvent::Parsed(done) = rx.recv().unwrap() else {
            panic!("expected a parse result");
        };
        assert_eq!(done.doc_id, doc.id());
        assert!(syntax.absorb(done, &doc, &tx));
        syntax.refresh(&doc, 0, 5);
        assert_eq!(syntax.spans()[0], span(0, 4, 9)); // the queued comment
        assert_eq!(syntax.spans()[1], span(5, 12, 9)); // the original lead
    }

    #[test]
    fn a_superseded_background_parse_is_dropped() {
        let text = "fn main() {}\n".repeat(SYNC_PARSE_LIMIT / 13 + 1);
        let (mut doc, mut syntax) = doc_named("t.rs", &text);
        let (tx, rx) = mpsc::channel();
        syntax.pump(&mut doc, &tx);
        // Overflowing the splice log invalidates the running parse and
        // spawns a fresh one.
        for _ in 0..=crate::doc::SPLICE_CAP {
            doc.edit(0..0, "x", caret(0), EditKind::Other);
            doc.break_undo_group();
        }
        syntax.pump(&mut doc, &tx);
        // The first worker may finish before its cancel lands (its stale
        // result must be dropped) or never send at all; only the fresh
        // parse installs either way.
        let mut received = 0;
        loop {
            let AppEvent::Parsed(done) = rx.recv().unwrap() else {
                panic!("expected a parse result");
            };
            received += 1;
            assert!(received <= 2);
            if syntax.absorb(done, &doc, &tx) {
                break;
            }
        }
        assert!(syntax.tree.is_some());
    }

    fn detected(path: &str) -> Option<&'static str> {
        spec_for_path(Path::new(path)).map(|s| s.name)
    }

    fn shebang(text: &str) -> Option<&'static str> {
        spec_for_shebang(&Rope::from_str(text)).map(|s| s.name)
    }

    /// Doubles as the palette audit: `-- --nocapture` lists every capture
    /// a bundled query defines that the palette leaves unmapped.
    #[test]
    fn every_language_query_compiles_and_reports_unmapped_captures() {
        for spec in &LANGS {
            let lang = spec
                .lang
                .get_or_init(|| Lang::new(spec.language.into(), &spec.query.concat()));
            for name in lang.query.capture_names() {
                if color_of(name) == UNMAPPED {
                    println!("{}: unmapped @{name}", spec.name);
                }
            }
        }
    }

    #[test]
    fn detection_by_extension_covers_every_language() {
        let cases = [
            ("t.rs", "rust"),
            ("t.go", "go"),
            ("t.py", "python"),
            ("t.sh", "bash"),
            ("t.c", "c"),
            ("t.h", "c"),
            ("t.cpp", "cpp"),
            ("t.yaml", "yaml"),
            ("t.tf", "hcl"),
            ("t.dockerfile", "dockerfile"),
            ("t.md", "markdown"),
            ("t.toml", "toml"),
            ("t.json", "json"),
        ];
        for (path, want) in cases {
            assert_eq!(detected(path), Some(want), "{path}");
        }
        assert_eq!(detected("t.txt"), None);
        assert_eq!(detected("noext"), None);
    }

    #[test]
    fn detection_by_well_known_filename() {
        assert_eq!(detected("Dockerfile"), Some("dockerfile"));
        assert_eq!(detected("/proj/sub/Dockerfile"), Some("dockerfile"));
        assert_eq!(detected("Containerfile"), Some("dockerfile"));
        assert_eq!(detected("Dockerfile.dev"), Some("dockerfile"));
        assert_eq!(detected("Dockerfilex"), None);
        assert_eq!(detected(".bashrc"), Some("bash"));
        assert_eq!(detected(".profile"), Some("bash"));
        assert_eq!(detected("Cargo.lock"), Some("toml"));
    }

    #[test]
    fn detection_by_shebang() {
        assert_eq!(shebang("#!/bin/bash\necho hi\n"), Some("bash"));
        assert_eq!(shebang("#!/bin/sh\n"), Some("bash"));
        assert_eq!(shebang("#!/usr/bin/env python3.12\n"), Some("python"));
        assert_eq!(shebang("#!/usr/bin/env -S bash -x\n"), Some("bash"));
        assert_eq!(shebang("echo no shebang\n"), None);
        assert_eq!(shebang(""), None);
    }

    #[test]
    fn a_path_match_beats_the_shebang() {
        let rope = Rope::from_str("#!/bin/bash\nx = 1\n");
        let lang = lang_for(Some(Path::new("t.py")), &rope).unwrap();
        let python = spec_for_path(Path::new("t.py")).unwrap();
        assert!(std::ptr::eq(lang, python.lang.get().unwrap()));
    }

    #[test]
    fn pathless_doc_with_shebang_gets_a_highlighter() {
        let mut doc = Document::from_str("#!/bin/sh\necho hi\n");
        assert!(Syntax::new(&mut doc).is_some());
    }

    #[test]
    fn every_new_language_colours_its_basics() {
        // (file name, snippet, colours that must appear); loose on purpose
        // so grammar version bumps don't shuffle exact spans out from
        // under the assertions.
        let cases: &[(&str, &str, &[u8])] = &[
            ("t.go", "package main // c\n", &[6, 9]),
            ("t.py", "def f():\n    return \"s\"  # c\n", &[6, 3, 9]),
            ("t.sh", "if true; then echo hi; fi # c\n", &[6, 9]),
            ("t.yaml", "key: \"value\" # c\n", &[3, 9]),
            (
                "t.tf",
                "resource \"a\" \"b\" {\n  x = \"s\" # c\n}\n",
                &[6, 3, 9],
            ),
            ("Dockerfile", "FROM alpine\n# c\nRUN echo hi\n", &[6, 9]),
            ("t.c", "int x = 1; // c\n", &[4, 9]),
            ("t.cpp", "class A {}; // c\n", &[6, 9]),
            ("t.toml", "# c\nkey = \"value\"\n", &[9, 3]),
            ("t.json", "{\"k\": \"v\", \"n\": 1}\n", &[3, 2]),
        ];
        for (name, text, expect) in cases {
            let (mut doc, mut syntax) = doc_named(name, text);
            let spans = spans_for(&mut doc, &mut syntax, 20);
            for want in *expect {
                assert!(
                    spans.iter().any(|s| s.color == *want),
                    "{name}: colour {want} missing in {spans:?}"
                );
            }
        }
    }
}
