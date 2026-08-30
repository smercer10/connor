//! Renders a document and view into a cleared screen: line-number gutter,
//! text area, status line. Pure — no terminal — so all of it unit-tests.

use std::fmt::Write as _;

use crate::doc::Document;
use crate::grapheme::{self, RopeGraphemes};
use crate::screen::Screen;
use crate::view::View;

/// The screen columns the text area starts at: the gutter holds every line
/// number right-aligned plus one space.
pub fn gutter_width(doc: &Document) -> usize {
    digits(doc.line_count()) + 1
}

/// Rows available to text: everything but the status line.
pub fn text_height(screen_height: u16) -> usize {
    usize::from(screen_height).saturating_sub(1)
}

/// Draws one frame into a cleared screen and returns the cursor's screen
/// cell. O(viewport): only visible lines are walked, and each only to the
/// viewport's right edge. `scratch` is reused across frames so steady-state
/// drawing never heap-allocates.
pub fn draw(screen: &mut Screen, doc: &Document, view: &View, scratch: &mut String) -> (u16, u16) {
    let (width, height) = screen.size();
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let width = usize::from(width);
    let text_h = text_height(height);
    let gutter_w = gutter_width(doc);
    let text_w = width.saturating_sub(gutter_w);

    let sel = view.selection();
    let mut buf = [0; 16];
    for y in 0..text_h {
        let line = view.scroll_line + y;
        if line >= doc.line_count() {
            break;
        }
        scratch.clear();
        let _ = write!(scratch, "{:>w$} ", line + 1, w = gutter_w - 1);
        screen.set_text(0, y as u16, scratch);
        if text_w == 0 {
            continue;
        }

        let start = doc.line_start(line);
        let line_end = doc.line_end(line);
        let slice = doc.rope().slice(start..line_end);
        let right = view.scroll_col + text_w;
        let mut col = 0;
        for range in RopeGraphemes::new(slice) {
            if col >= right {
                break;
            }
            // Cursor and anchor sit on cluster boundaries, so a cluster is
            // always wholly in or out of the selection.
            let selected = sel
                .as_ref()
                .is_some_and(|s| s.start < start + range.end && start + range.start < s.end);
            let cluster = grapheme::grapheme_str(slice, range, &mut buf);
            let cluster_w = grapheme::grapheme_width(cluster, col);
            let end = col + cluster_w;
            if end > view.scroll_col {
                // Tabs and clusters clipped by either viewport edge stay
                // blank — the cleared screen already holds spaces there.
                if cluster != "\t" && col >= view.scroll_col && end <= right {
                    let x = (gutter_w + col - view.scroll_col) as u16;
                    let first = cluster.chars().next().unwrap_or(' ');
                    if first.is_control() {
                        screen.set_grapheme(x, y as u16, "\u{FFFD}", 1);
                    } else {
                        screen.set_grapheme(x, y as u16, cluster, cluster_w as u8);
                    }
                }
                if selected {
                    for c in col.max(view.scroll_col)..end.min(right) {
                        screen.set_reversed(
                            (gutter_w + c - view.scroll_col) as u16,
                            y as u16,
                            true,
                        );
                    }
                }
            }
            col = end;
        }
        // A selection running past the line's end highlights one extra
        // column there — the cue that the terminator is included, and what
        // makes selected empty lines visible at all.
        if sel.as_ref().is_some_and(|s| s.contains(&line_end))
            && (view.scroll_col..right).contains(&col)
        {
            screen.set_reversed((gutter_w + col - view.scroll_col) as u16, y as u16, true);
        }
    }

    let line = view.line(doc);
    let vcol = view.vcol(doc);

    scratch.clear();
    let _ = write!(scratch, "{} · {}:{}", doc.name(), line + 1, vcol + 1);
    if doc.dirty() {
        scratch.push_str(" [+]");
    }
    if doc.lossy() {
        scratch.push_str(" [lossy]");
    }
    screen.set_text(0, height - 1, scratch);

    let cx = (gutter_w + vcol.saturating_sub(view.scroll_col)).min(width - 1) as u16;
    let cy = line
        .saturating_sub(view.scroll_line)
        .min(text_h.saturating_sub(1)) as u16;
    (cx, cy)
}

fn digits(mut n: usize) -> usize {
    let mut count = 1;
    while n >= 10 {
        n /= 10;
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(text: &str, width: u16, height: u16, view: &View) -> (Screen, (u16, u16)) {
        let doc = Document::from_str(text);
        let mut screen = Screen::new(width, height);
        let mut scratch = String::new();
        let cursor = draw(&mut screen, &doc, view, &mut scratch);
        (screen, cursor)
    }

    fn row(screen: &Screen, y: u16) -> String {
        let (width, _) = screen.size();
        (0..width)
            .map(|x| screen.get(x, y).unwrap().str().to_owned())
            .collect()
    }

    #[test]
    fn gutter_text_status_and_blank_rows() {
        let (screen, cursor) = render("hello\nworld", 14, 4, &View::default());
        assert_eq!(row(&screen, 0), "1 hello       ");
        assert_eq!(row(&screen, 1), "2 world       ");
        assert_eq!(row(&screen, 2), "              ");
        assert_eq!(row(&screen, 3), "[No Name] · 1:");
        assert_eq!(cursor, (2, 0));
    }

    #[test]
    fn gutter_widens_with_the_line_count() {
        let text = "a\n".repeat(9) + "j";
        let (screen, cursor) = render(&text, 8, 12, &View::default());
        assert_eq!(row(&screen, 0), " 1 a    ");
        assert_eq!(row(&screen, 9), "10 j    ");
        assert_eq!(cursor, (3, 0));
    }

    #[test]
    fn wide_glyphs_render_and_tabs_stay_blank() {
        let (screen, _) = render("日本\n\tx", 13, 3, &View::default());
        assert_eq!(row(&screen, 0), "1 日本       ");
        assert_eq!(row(&screen, 1), "2         x  ");
    }

    #[test]
    fn control_clusters_render_as_replacement() {
        let (screen, _) = render("a\u{7}b", 8, 2, &View::default());
        assert_eq!(row(&screen, 0), "1 a\u{FFFD}b   ");
    }

    #[test]
    fn horizontal_scroll_clips_wide_glyphs_at_both_edges() {
        // "ab日cd": 日 spans columns 2-3.
        let left_clipped = View::test_at(0, 0, 3);
        let (screen, _) = render("ab日cd", 6, 2, &left_clipped);
        assert_eq!(row(&screen, 0), "1  cd ");

        // Text area is 3 columns; 本 (columns 2-3) crosses the right edge.
        let (screen, _) = render("日本", 5, 2, &View::default());
        assert_eq!(row(&screen, 0), "1 日 ");
    }

    #[test]
    fn status_reports_cursor_position_and_lossy() {
        let mut doc = Document::from_str("日x\nab");
        doc.set_lossy(true);
        let view = View::test_at(4, 0, 0); // 'b' on line 2
        let mut screen = Screen::new(24, 3);
        let mut scratch = String::new();
        let cursor = draw(&mut screen, &doc, &view, &mut scratch);
        assert_eq!(row(&screen, 2), "[No Name] · 2:2 [lossy] ");
        assert_eq!(cursor, (3, 1));
    }

    #[test]
    fn cursor_cell_accounts_for_widths_and_scroll() {
        let view = View::test_at(1, 0, 0); // after 日: visual column 2
        let (_, cursor) = render("日本x", 10, 2, &view);
        assert_eq!(cursor, (4, 0));

        // 11 lines (three-column gutter); cursor on line 6, two rows below
        // the scroll top.
        let scrolled = View::test_at(10, 3, 0);
        let text = "a\n".repeat(10);
        let (_, cursor) = render(&text, 10, 6, &scrolled);
        assert_eq!(cursor, (3, 2));
    }

    /// The reverse-video flags of a row: `#` where set, space where not.
    fn sel_row(screen: &Screen, y: u16) -> String {
        let (width, _) = screen.size();
        (0..width)
            .map(|x| {
                if screen.get(x, y).unwrap().reversed() {
                    '#'
                } else {
                    ' '
                }
            })
            .collect()
    }

    #[test]
    fn selection_highlights_its_extent_and_nothing_else() {
        let view = View::test_at(7, 0, 0).with_anchor(2);
        let (screen, _) = render("hello world", 14, 2, &view);
        assert_eq!(row(&screen, 0), "1 hello world ");
        assert_eq!(sel_row(&screen, 0), "    #####     ");
    }

    #[test]
    fn no_selection_or_empty_selection_highlights_nothing() {
        let (screen, _) = render("hello", 9, 2, &View::default());
        assert_eq!(sel_row(&screen, 0), "         ");

        let collapsed = View::test_at(2, 0, 0).with_anchor(2);
        let (screen, _) = render("hello", 9, 2, &collapsed);
        assert_eq!(sel_row(&screen, 0), "         ");
    }

    #[test]
    fn selection_across_lines_cues_the_terminator() {
        let view = View::test_at(4, 0, 0).with_anchor(1);
        let (screen, _) = render("ab\ncd", 7, 3, &view);
        assert_eq!(sel_row(&screen, 0), "   ##  "); // b plus the terminator
        assert_eq!(sel_row(&screen, 1), "  #    "); // c
    }

    #[test]
    fn selected_empty_line_shows_its_terminator_cue() {
        let view = View::test_at(4, 0, 0).with_anchor(0);
        let (screen, _) = render("a\n\nb", 6, 4, &view);
        assert_eq!(sel_row(&screen, 0), "  ##  ");
        assert_eq!(sel_row(&screen, 1), "  #   ");
        assert_eq!(sel_row(&screen, 2), "  #   ");
    }

    #[test]
    fn selected_tabs_and_wide_glyphs_highlight_their_whole_span() {
        let view = View::test_at(3, 0, 0).with_anchor(0);
        let (screen, _) = render("a\tb", 12, 2, &view);
        assert_eq!(row(&screen, 0), "1 a       b ");
        assert_eq!(sel_row(&screen, 0), "  ######### ");

        let view = View::test_at(1, 0, 0).with_anchor(0);
        let (screen, _) = render("日x", 7, 2, &view);
        assert_eq!(sel_row(&screen, 0), "  ##   ");
    }

    #[test]
    fn selection_clips_to_the_viewport() {
        // "abcdef" with columns 0-2 scrolled off and a 2-column text area:
        // only d and e are visible of the d-f selection.
        let view = View::test_at(6, 0, 3).with_anchor(3);
        let (screen, _) = render("abcdef", 4, 2, &view);
        assert_eq!(row(&screen, 0), "1 de");
        assert_eq!(sel_row(&screen, 0), "  ##");
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(0, 0), (0, 5), (5, 0), (1, 1), (2, 2), (3, 1), (1, 3)] {
            render("日本\ntext", w, h, &View::default());
        }
    }
}
