//! Incremental syntax highlighting: tree-sitter parsing, viewport span
//! production, and the capture-name palette. `Document` stays parser-agnostic
//! by recording plain `Splice`s that this module drains.

/// One coloured stretch of the document in char indices; spans are sorted
/// and non-overlapping, so the draw pass consumes them with a single
/// advancing index. `color` uses the `Cell::fg` encoding.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Span {
    pub start: usize,
    pub end: usize,
    pub color: u8,
}
