/// One terminal cell. Assumes every glyph occupies exactly one column; wide
/// characters are not yet handled and will misrender rather than crash.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub ch: char,
}

impl Cell {
    pub const BLANK: Cell = Cell { ch: ' ' };
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

    /// Out-of-bounds writes are ignored: a scene drawn against a stale size
    /// (e.g. mid-resize) must clip, not crash.
    pub fn set(&mut self, x: u16, y: u16, ch: char) {
        if x < self.width && y < self.height {
            let index = self.index(x, y);
            self.cells[index] = Cell { ch };
        }
    }

    pub fn set_text(&mut self, x: u16, y: u16, text: &str) {
        for (offset, ch) in text.chars().enumerate() {
            let Ok(offset) = u16::try_from(offset) else {
                return;
            };
            let Some(x) = x.checked_add(offset) else {
                return;
            };
            self.set(x, y, ch);
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
            out.push((x, y, run.iter().map(|c| c.ch).collect()));
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
    fn zero_sized_buffers_diff_without_panicking() {
        let a = Screen::new(0, 3);
        let b = Screen::new(0, 3);
        assert!(runs(&a, &b).is_empty());
        let c = Screen::new(3, 0);
        let d = Screen::new(3, 0);
        assert!(runs(&c, &d).is_empty());
    }
}
