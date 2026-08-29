use unicode_width::UnicodeWidthChar;

/// One terminal cell: a grapheme cluster stored inline (no heap) plus its
/// display width. A two-column glyph occupies a leader cell followed by one
/// `CONTINUATION` cell; emission skips continuations because the terminal
/// advances two columns when the leader is written.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    bytes: [u8; 14],
    len: u8,
    width: u8,
}

impl Cell {
    pub const BLANK: Cell = Cell::from_ascii(b' ');
    /// The column occupied by the wide glyph to its left.
    pub const CONTINUATION: Cell = Cell {
        bytes: [0; 14],
        len: 0,
        width: 0,
    };
    /// U+FFFD; stands in for clusters too long to store inline.
    const REPLACEMENT: Cell = Cell {
        bytes: [0xEF, 0xBF, 0xBD, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        len: 3,
        width: 1,
    };

    const fn from_ascii(byte: u8) -> Cell {
        let mut bytes = [0; 14];
        bytes[0] = byte;
        Cell {
            bytes,
            len: 1,
            width: 1,
        }
    }

    pub fn new(grapheme: &str, width: u8) -> Cell {
        if grapheme.len() > 14 {
            return Cell::REPLACEMENT;
        }
        let mut bytes = [0; 14];
        bytes[..grapheme.len()].copy_from_slice(grapheme.as_bytes());
        Cell {
            bytes,
            len: grapheme.len() as u8,
            width: width.max(1),
        }
    }

    pub fn str(&self) -> &str {
        str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("\u{FFFD}")
    }

    #[cfg(test)]
    pub fn width(&self) -> u8 {
        self.width
    }

    pub fn is_continuation(&self) -> bool {
        self.width == 0
    }
}

/// A grid of cells sized to the terminal. Scenes draw into one grid while
/// another holds what is currently on screen; diffing the two yields the
/// minimal set of cells to rewrite.
pub struct Screen {
    width: u16,
    height: u16,
    cells: Vec<Cell>,
}

impl Screen {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::BLANK; usize::from(width) * usize::from(height)],
        }
    }

    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Re-dimensions and blanks the grid. Not called on the render path.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.cells.clear();
        self.cells
            .resize(usize::from(width) * usize::from(height), Cell::BLANK);
    }

    pub fn clear(&mut self) {
        self.cells.fill(Cell::BLANK);
    }

    /// Convenience for single-scalar glyphs (UI chrome, box drawing).
    pub fn set(&mut self, x: u16, y: u16, ch: char) {
        let mut buf = [0; 4];
        let width = ch.width().unwrap_or(1).max(1) as u8;
        self.set_grapheme(x, y, ch.encode_utf8(&mut buf), width);
    }

    /// Writes one grapheme cluster of the given display width. A two-column
    /// glyph also claims the cell to its right as a continuation; one that
    /// doesn't fit (at the grid's right edge) is blanked instead of torn.
    /// Out-of-bounds writes are ignored: a scene drawn against a stale size
    /// (e.g. mid-resize) must clip, not crash.
    pub fn set_grapheme(&mut self, x: u16, y: u16, grapheme: &str, width: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        self.repair(x, y);
        let index = self.index(x, y);
        if width == 2 {
            if x + 1 >= self.width {
                self.cells[index] = Cell::BLANK;
                return;
            }
            self.repair(x + 1, y);
            let index = self.index(x, y);
            self.cells[index] = Cell::new(grapheme, 2);
            self.cells[index + 1] = Cell::CONTINUATION;
        } else {
            self.cells[index] = Cell::new(grapheme, width);
        }
    }

    /// Makes (x, y) safe to overwrite: severing half of a wide glyph blanks
    /// the other half rather than leaving an orphaned leader or continuation.
    fn repair(&mut self, x: u16, y: u16) {
        let index = self.index(x, y);
        if self.cells[index].is_continuation() {
            self.cells[index - 1] = Cell::BLANK;
        } else if self.cells[index].width == 2 {
            self.cells[index + 1] = Cell::BLANK;
        }
    }

    /// Writes scalar-per-glyph text (UI chrome), advancing by display width.
    pub fn set_text(&mut self, x: u16, y: u16, text: &str) {
        let mut x = x;
        for ch in text.chars() {
            if x >= self.width {
                return;
            }
            let width = ch.width().unwrap_or(1).max(1) as u16;
            self.set(x, y, ch);
            let Some(next) = x.checked_add(width) else {
                return;
            };
            x = next;
        }
    }

    #[cfg(test)]
    pub fn get(&self, x: u16, y: u16) -> Option<Cell> {
        (x < self.width && y < self.height).then(|| self.cells[self.index(x, y)])
    }

    /// Copies the contents of an equally-sized grid without allocating.
    pub fn copy_from(&mut self, other: &Screen) {
        debug_assert_eq!(self.size(), other.size());
        self.cells.copy_from_slice(&other.cells);
    }

    /// Calls `f(x, y, run)` once per maximal horizontal run of cells that
    /// differ from `prev`. Both buffers must be the same size. Allocation-free.
    pub fn for_each_changed_run(&self, prev: &Screen, mut f: impl FnMut(u16, u16, &[Cell])) {
        debug_assert_eq!(self.size(), prev.size());
        let width = usize::from(self.width);
        for y in 0..self.height {
            let row_start = usize::from(y) * width;
            let row = &self.cells[row_start..row_start + width];
            let prev_row = &prev.cells[row_start..row_start + width];
            let mut x = 0;
            while x < width {
                if row[x] == prev_row[x] {
                    x += 1;
                    continue;
                }
                let start = x;
                while x < width && row[x] != prev_row[x] {
                    x += 1;
                }
                f(start as u16, y, &row[start..x]);
            }
        }
    }

    fn index(&self, x: u16, y: u16) -> usize {
        usize::from(y) * usize::from(self.width) + usize::from(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runs(next: &Screen, prev: &Screen) -> Vec<(u16, u16, String)> {
        let mut out = Vec::new();
        next.for_each_changed_run(prev, |x, y, run| {
            out.push((x, y, run.iter().map(Cell::str).collect()));
        });
        out
    }

    #[test]
    fn identical_buffers_yield_no_runs() {
        let a = Screen::new(10, 4);
        let b = Screen::new(10, 4);
        assert!(runs(&a, &b).is_empty());
    }

    #[test]
    fn single_changed_cell_yields_one_run() {
        let prev = Screen::new(10, 4);
        let mut next = Screen::new(10, 4);
        next.set(3, 2, 'x');
        assert_eq!(runs(&next, &prev), vec![(3, 2, "x".into())]);
    }

    #[test]
    fn adjacent_changes_coalesce() {
        let prev = Screen::new(10, 4);
        let mut next = Screen::new(10, 4);
        next.set_text(2, 1, "abc");
        assert_eq!(runs(&next, &prev), vec![(2, 1, "abc".into())]);
    }

    #[test]
    fn gap_splits_runs() {
        let prev = Screen::new(10, 4);
        let mut next = Screen::new(10, 4);
        next.set(1, 0, 'a');
        next.set(3, 0, 'b');
        assert_eq!(
            runs(&next, &prev),
            vec![(1, 0, "a".into()), (3, 0, "b".into())]
        );
    }

    #[test]
    fn row_boundary_splits_runs() {
        let prev = Screen::new(3, 2);
        let mut next = Screen::new(3, 2);
        next.set(2, 0, 'a');
        next.set(0, 1, 'b');
        assert_eq!(
            runs(&next, &prev),
            vec![(2, 0, "a".into()), (0, 1, "b".into())]
        );
    }

    #[test]
    fn last_cell_of_grid_is_diffed() {
        let prev = Screen::new(5, 3);
        let mut next = Screen::new(5, 3);
        next.set(4, 2, 'z');
        assert_eq!(runs(&next, &prev), vec![(4, 2, "z".into())]);
    }

    #[test]
    fn clear_against_previous_content_covers_it() {
        let mut prev = Screen::new(6, 1);
        prev.set_text(1, 0, "old");
        let next = Screen::new(6, 1);
        assert_eq!(runs(&next, &prev), vec![(1, 0, "   ".into())]);
    }

    #[test]
    fn resize_blanks_and_redimensions() {
        let mut buf = Screen::new(4, 2);
        buf.set(0, 0, 'x');
        buf.resize(3, 5);
        assert_eq!(buf.size(), (3, 5));
        assert_eq!(buf.get(0, 0), Some(Cell::BLANK));
    }

    #[test]
    fn out_of_bounds_set_is_ignored() {
        let mut buf = Screen::new(4, 2);
        buf.set(4, 0, 'x');
        buf.set(0, 2, 'x');
        buf.set_text(2, 0, "long text past the edge");
        let blank = Screen::new(4, 2);
        assert_eq!(runs(&buf, &blank), vec![(2, 0, "lo".into())]);
    }

    #[test]
    fn wide_glyph_claims_leader_and_continuation() {
        let mut screen = Screen::new(6, 1);
        screen.set_grapheme(1, 0, "日", 2);
        assert_eq!(screen.get(1, 0).unwrap().str(), "日");
        assert_eq!(screen.get(1, 0).unwrap().width(), 2);
        assert!(screen.get(2, 0).unwrap().is_continuation());
    }

    #[test]
    fn wide_glyph_at_last_column_becomes_blank() {
        let mut screen = Screen::new(4, 1);
        screen.set_grapheme(3, 0, "日", 2);
        assert_eq!(screen.get(3, 0), Some(Cell::BLANK));
    }

    #[test]
    fn overwriting_either_half_of_a_wide_glyph_blanks_the_other() {
        let mut screen = Screen::new(6, 1);
        screen.set_grapheme(1, 0, "日", 2);
        screen.set(1, 0, 'a');
        assert_eq!(screen.get(2, 0), Some(Cell::BLANK));

        screen.set_grapheme(1, 0, "日", 2);
        screen.set(2, 0, 'b');
        assert_eq!(screen.get(1, 0), Some(Cell::BLANK));
        assert_eq!(screen.get(2, 0).unwrap().str(), "b");
    }

    #[test]
    fn overlapping_wide_glyphs_repair_each_other() {
        let mut screen = Screen::new(6, 1);
        screen.set_grapheme(1, 0, "日", 2);
        screen.set_grapheme(2, 0, "本", 2);
        assert_eq!(screen.get(1, 0), Some(Cell::BLANK));
        assert_eq!(screen.get(2, 0).unwrap().str(), "本");
        assert!(screen.get(3, 0).unwrap().is_continuation());
    }

    #[test]
    fn combining_cluster_fits_one_cell() {
        let mut screen = Screen::new(4, 1);
        screen.set_grapheme(0, 0, "e\u{301}", 1);
        assert_eq!(screen.get(0, 0).unwrap().str(), "e\u{301}");
        assert_eq!(screen.get(0, 0).unwrap().width(), 1);
    }

    #[test]
    fn oversized_cluster_clips_to_replacement() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        assert!(family.len() > 14);
        let cell = Cell::new(family, 2);
        assert_eq!(cell.str(), "\u{FFFD}");
        assert_eq!(cell.width(), 1);
    }

    #[test]
    fn wide_glyph_change_always_starts_its_run_at_the_leader() {
        let mut prev = Screen::new(6, 1);
        prev.set_grapheme(2, 0, "日", 2);
        let mut next = Screen::new(6, 1);
        next.set_grapheme(2, 0, "本", 2);
        assert_eq!(runs(&next, &prev), vec![(2, 0, "本".into())]);

        let mut narrow = Screen::new(6, 1);
        narrow.set_text(2, 0, "ab");
        assert_eq!(runs(&narrow, &prev), vec![(2, 0, "ab".into())]);
        assert_eq!(runs(&prev, &narrow), vec![(2, 0, "日".into())]);
    }

    #[test]
    fn zero_sized_buffers_diff_without_panicking() {
        let a = Screen::new(0, 3);
        let b = Screen::new(0, 3);
        assert!(runs(&a, &b).is_empty());
        let c = Screen::new(3, 0);
        let d = Screen::new(3, 0);
        assert!(runs(&c, &d).is_empty());
    }
}
