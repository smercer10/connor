//! Renders the tabs into a cleared screen: tab bar, line-number gutter,
//! text area, status line. Pure — no terminal — so all of it unit-tests.

use std::fmt::Write as _;

use unicode_width::UnicodeWidthChar;

use crate::doc::Document;
use crate::grapheme::{self, RopeGraphemes};
use crate::screen::Screen;
use crate::search::Highlights;
use crate::tabs::{Tab, Tabs};

/// The screen columns the text area starts at: the gutter holds every line
/// number right-aligned plus one space.
pub fn gutter_width(doc: &Document) -> usize {
    digits(doc.line_count()) + 1
}

/// Rows available to text: everything but the tab bar and the status line.
pub fn text_height(screen_height: u16) -> usize {
    usize::from(screen_height).saturating_sub(2)
}

/// Draws one frame into a cleared screen and returns the cursor's screen
/// cell. O(viewport): only visible lines are walked, and each only to the
/// viewport's right edge. `scratch` is reused across frames so steady-state
/// drawing never heap-allocates. A non-empty `notice` — a message or a
/// mini-prompt — takes over the status line until the next keypress;
/// `status_caret` parks the cursor after that many of the notice's chars
/// while a prompt is editing there. `search` underlines every match and
/// reverses the current one.
pub fn draw(
    screen: &mut Screen,
    tabs: &Tabs,
    scratch: &mut String,
    notice: &str,
    status_caret: Option<usize>,
    search: Option<Highlights>,
) -> (u16, u16) {
    let (width, height) = screen.size();
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let width = usize::from(width);
    let text_h = text_height(height);
    let Tab { doc, view } = tabs.active();
    let gutter_w = gutter_width(doc);
    let text_w = width.saturating_sub(gutter_w);

    if height > 1 {
        draw_tab_bar(screen, tabs, scratch, width);
    }

    let sel = view.selection();
    let mut buf = [0; 16];
    for y in 0..text_h {
        let line = view.scroll_line + y;
        if line >= doc.line_count() {
            break;
        }
        let row = (y + 1) as u16;
        scratch.clear();
        let _ = write!(scratch, "{:>w$} ", line + 1, w = gutter_w - 1);
        screen.set_text(0, row, scratch);
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
            // The cluster's span in document char indices. Cursor and anchor
            // sit on cluster boundaries, so a cluster is always wholly in or
            // out of the selection.
            let (c_start, c_end) = (start + range.start, start + range.end);
            let selected = sel
                .as_ref()
                .is_some_and(|s| s.start < c_end && c_start < s.end);
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
                        screen.set_grapheme(x, row, "\u{FFFD}", 1);
                    } else {
                        screen.set_grapheme(x, row, cluster, cluster_w as u8);
                    }
                }
                if selected {
                    for c in col.max(view.scroll_col)..end.min(right) {
                        screen.set_reversed((gutter_w + c - view.scroll_col) as u16, row, true);
                    }
                }
                // Match starts sit on char indices, so like the selection a
                // cluster is wholly in or out; binary search keeps the pass
                // O(log matches) per visible cluster. A stale out-of-range
                // start simply intersects nothing.
                if let Some(h) = &search
                    && h.len > 0
                {
                    let i = h
                        .starts
                        .partition_point(|&s| s.saturating_add(h.len) <= c_start);
                    if h.starts.get(i).is_some_and(|&s| s < c_end) {
                        for c in col.max(view.scroll_col)..end.min(right) {
                            let x = (gutter_w + c - view.scroll_col) as u16;
                            if h.current == Some(i) {
                                screen.set_reversed(x, row, true);
                            } else {
                                screen.set_underlined(x, row, true);
                            }
                        }
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
            screen.set_reversed((gutter_w + col - view.scroll_col) as u16, row, true);
        }
    }

    let line = view.line(doc);
    let vcol = view.vcol(doc);

    if notice.is_empty() {
        scratch.clear();
        let _ = write!(scratch, "{} · {}:{}", doc.name(), line + 1, vcol + 1);
        if doc.dirty() {
            scratch.push_str(" [+]");
        }
        if doc.lossy() {
            scratch.push_str(" [lossy]");
        }
        if doc.conflict() {
            scratch.push_str(" [disk changed]");
        }
        if doc.recovered() {
            scratch.push_str(" [recovered]");
        }
        screen.set_text(0, height - 1, scratch);
    } else {
        screen.set_text(0, height - 1, notice);
    }

    if let Some(chars) = status_caret {
        let col: usize = notice.chars().take(chars).map(char_cols).sum();
        return (col.min(width - 1) as u16, height - 1);
    }
    let cx = (gutter_w + vcol.saturating_sub(view.scroll_col)).min(width - 1) as u16;
    let cy = (1 + line
        .saturating_sub(view.scroll_line)
        .min(text_h.saturating_sub(1)))
    .min(usize::from(height) - 1) as u16;
    (cx, cy)
}

/// Longest name a tab label shows before truncating with an ellipsis.
const NAME_COLS: usize = 20;

/// Draws the labels on row 0, starting from the leftmost tab that still
/// leaves the active label fully on screen; trailing labels clip.
fn draw_tab_bar(screen: &mut Screen, tabs: &Tabs, scratch: &mut String, width: usize) {
    let all = tabs.all();
    let active = tabs.active_index();
    let mut x = 0;
    for (i, tab) in all.iter().enumerate().skip(first_shown(tabs, width)) {
        scratch.clear();
        scratch.push(' ');
        push_shown_name(&tab.doc.name(), scratch);
        if tab.doc.dirty() {
            scratch.push('+');
        }
        if tab.doc.conflict() {
            scratch.push('!');
        }
        scratch.push(' ');
        screen.set_text(x as u16, 0, scratch);
        let label_w = label_width(tab);
        if i == active {
            for col in x..(x + label_w).min(width) {
                screen.set_reversed(col as u16, 0, true);
            }
        }
        x += label_w;
        if x >= width {
            break;
        }
    }
}

/// The leftmost tab the bar shows: labels scroll off the left until the
/// active one fits fully on screen.
fn first_shown(tabs: &Tabs, width: usize) -> usize {
    let all = tabs.all();
    let mut first = tabs.active_index();
    let mut used = label_width(&all[first]);
    while first > 0 && used + label_width(&all[first - 1]) <= width {
        first -= 1;
        used += label_width(&all[first]);
    }
    first
}

/// The tab whose label covers column `x` of the bar as drawn, if any.
pub fn tab_at(tabs: &Tabs, width: usize, x: u16) -> Option<usize> {
    let all = tabs.all();
    let mut left = 0;
    for (i, tab) in all.iter().enumerate().skip(first_shown(tabs, width)) {
        let right = left + label_width(tab);
        if (left..right).contains(&usize::from(x)) {
            return Some(i);
        }
        left = right;
        if left >= width {
            break;
        }
    }
    None
}

/// The screen columns one tab's label occupies: padding, the shown name,
/// and the dirty and conflict marks.
fn label_width(tab: &Tab) -> usize {
    2 + shown_name_cols(&tab.doc.name())
        + usize::from(tab.doc.dirty())
        + usize::from(tab.doc.conflict())
}

fn shown_name_cols(name: &str) -> usize {
    let full: usize = name.chars().map(char_cols).sum();
    if full <= NAME_COLS {
        return full;
    }
    let mut cols = 0;
    for ch in name.chars() {
        let w = char_cols(ch);
        if cols + w > NAME_COLS - 1 {
            break;
        }
        cols += w;
    }
    cols + 1
}

/// Appends the name capped at `NAME_COLS` columns, a trailing ellipsis
/// standing in for the cut; must stay in step with `shown_name_cols`.
fn push_shown_name(name: &str, out: &mut String) {
    let full: usize = name.chars().map(char_cols).sum();
    if full <= NAME_COLS {
        out.push_str(name);
        return;
    }
    let mut cols = 0;
    for ch in name.chars() {
        let w = char_cols(ch);
        if cols + w > NAME_COLS - 1 {
            break;
        }
        cols += w;
        out.push(ch);
    }
    out.push('…');
}

/// Matches `Screen::set_text`'s advance so measured and drawn widths agree.
fn char_cols(ch: char) -> usize {
    ch.width().unwrap_or(1).max(1)
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
    use std::path::PathBuf;

    use super::*;
    use crate::doc::{Caret, EditKind};
    use crate::view::View;

    fn tabs_of(doc: Document, view: View) -> Tabs {
        let mut tabs = Tabs::new(vec![doc]);
        tabs.active_mut().view = view;
        tabs
    }

    fn render_tabs(tabs: &Tabs, width: u16, height: u16) -> (Screen, (u16, u16)) {
        let mut screen = Screen::new(width, height);
        let mut scratch = String::new();
        let cursor = draw(&mut screen, tabs, &mut scratch, "", None, None);
        (screen, cursor)
    }

    fn render(text: &str, width: u16, height: u16, view: View) -> (Screen, (u16, u16)) {
        render_tabs(&tabs_of(Document::from_str(text), view), width, height)
    }

    fn named(name: &str, text: &str) -> Document {
        let mut doc = Document::from_str(text);
        doc.set_path(PathBuf::from(name));
        doc
    }

    fn dirtied(mut doc: Document) -> Document {
        doc.edit(
            0..0,
            "x",
            Caret {
                cursor: 0,
                anchor: None,
            },
            EditKind::Insert,
        );
        doc
    }

    fn row(screen: &Screen, y: u16) -> String {
        let (width, _) = screen.size();
        (0..width)
            .map(|x| screen.get(x, y).unwrap().str().to_owned())
            .collect()
    }

    #[test]
    fn gutter_text_status_and_blank_rows() {
        let (screen, cursor) = render("hello\nworld", 14, 5, View::default());
        assert_eq!(row(&screen, 0), " [No Name]    ");
        assert_eq!(row(&screen, 1), "1 hello       ");
        assert_eq!(row(&screen, 2), "2 world       ");
        assert_eq!(row(&screen, 3), "              ");
        assert_eq!(row(&screen, 4), "[No Name] · 1:");
        assert_eq!(cursor, (2, 1));
    }

    #[test]
    fn gutter_widens_with_the_line_count() {
        let text = "a\n".repeat(9) + "j";
        let (screen, cursor) = render(&text, 8, 13, View::default());
        assert_eq!(row(&screen, 1), " 1 a    ");
        assert_eq!(row(&screen, 10), "10 j    ");
        assert_eq!(cursor, (3, 1));
    }

    #[test]
    fn wide_glyphs_render_and_tabs_stay_blank() {
        let (screen, _) = render("日本\n\tx", 13, 4, View::default());
        assert_eq!(row(&screen, 1), "1 日本       ");
        assert_eq!(row(&screen, 2), "2         x  ");
    }

    #[test]
    fn control_clusters_render_as_replacement() {
        let (screen, _) = render("a\u{7}b", 8, 3, View::default());
        assert_eq!(row(&screen, 1), "1 a\u{FFFD}b   ");
    }

    #[test]
    fn horizontal_scroll_clips_wide_glyphs_at_both_edges() {
        // "ab日cd": 日 spans columns 2-3.
        let left_clipped = View::test_at(0, 0, 3);
        let (screen, _) = render("ab日cd", 6, 3, left_clipped);
        assert_eq!(row(&screen, 1), "1  cd ");

        // Text area is 3 columns; 本 (columns 2-3) crosses the right edge.
        let (screen, _) = render("日本", 5, 3, View::default());
        assert_eq!(row(&screen, 1), "1 日 ");
    }

    #[test]
    fn status_reports_cursor_position_and_lossy() {
        let mut doc = Document::from_str("日x\nab");
        doc.set_lossy(true);
        let view = View::test_at(4, 0, 0); // 'b' on line 2
        let (screen, cursor) = render_tabs(&tabs_of(doc, view), 24, 4);
        assert_eq!(row(&screen, 3), "[No Name] · 2:2 [lossy] ");
        assert_eq!(cursor, (3, 2));
    }

    #[test]
    fn status_and_tab_bar_mark_a_disk_conflict() {
        let mut doc = dirtied(named("a.rs", "x"));
        doc.set_conflict(true);
        let tabs = tabs_of(doc, View::default());
        let (screen, _) = render_tabs(&tabs, 32, 4);
        assert_eq!(row(&screen, 0), " a.rs+!                         ");
        assert_eq!(row(&screen, 3), "a.rs · 1:1 [+] [disk changed]   ");
    }

    #[test]
    fn a_notice_takes_over_the_status_line() {
        let tabs = tabs_of(Document::from_str("ab"), View::default());
        let mut screen = Screen::new(12, 3);
        let mut scratch = String::new();
        draw(&mut screen, &tabs, &mut scratch, "saved ab", None, None);
        assert_eq!(row(&screen, 2), "saved ab    ");
    }

    #[test]
    fn prompt_caret_parks_at_the_notice_end_in_the_status_line() {
        let tabs = tabs_of(Document::from_str("ab"), View::default());
        let mut screen = Screen::new(12, 3);
        let mut scratch = String::new();
        let cursor = draw(&mut screen, &tabs, &mut scratch, "open: sr", Some(8), None);
        assert_eq!(cursor, (8, 2));

        // The caret clips at the right edge rather than leaving the screen.
        let cursor = draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "open: src/main.rs",
            Some(17),
            None,
        );
        assert_eq!(cursor, (11, 2));
    }

    #[test]
    fn prompt_caret_can_park_inside_the_notice() {
        let tabs = tabs_of(Document::from_str("ab"), View::default());
        let mut screen = Screen::new(24, 3);
        let mut scratch = String::new();
        // Caret after "find: 日" — hint text follows the edited field.
        let cursor = draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "find: 日 · esc",
            Some(7),
            None,
        );
        assert_eq!(cursor, (8, 2));
    }

    #[test]
    fn cursor_cell_accounts_for_widths_and_scroll() {
        let view = View::test_at(1, 0, 0); // after 日: visual column 2
        let (_, cursor) = render("日本x", 10, 3, view);
        assert_eq!(cursor, (4, 1));

        // 11 lines (three-column gutter); cursor on line 6, two rows below
        // the scroll top.
        let scrolled = View::test_at(10, 3, 0);
        let text = "a\n".repeat(10);
        let (_, cursor) = render(&text, 10, 7, scrolled);
        assert_eq!(cursor, (3, 3));
    }

    #[test]
    fn tab_bar_lists_tabs_marking_active_and_dirty() {
        let tabs = Tabs::new(vec![named("a.rs", ""), dirtied(named("b.rs", ""))]);
        let (screen, _) = render_tabs(&tabs, 16, 3);
        assert_eq!(row(&screen, 0), " a.rs  b.rs+    ");
        assert_eq!(sel_row(&screen, 0), "######          ");
    }

    #[test]
    fn tab_bar_truncates_long_names_with_an_ellipsis() {
        let tabs = Tabs::new(vec![named("a_very_long_file_name.rs", "")]);
        let (screen, _) = render_tabs(&tabs, 24, 3);
        assert_eq!(row(&screen, 0), " a_very_long_file_na…   ");
    }

    #[test]
    fn tab_bar_overflow_keeps_the_active_tab_fully_visible() {
        let mut tabs = Tabs::new(vec![
            named("aa.rs", ""),
            named("bb.rs", ""),
            named("cc.rs", ""),
        ]);
        tabs.activate(2);
        let (screen, _) = render_tabs(&tabs, 15, 3);
        assert_eq!(row(&screen, 0), " bb.rs  cc.rs  ");
        assert_eq!(sel_row(&screen, 0), "       ####### ");
    }

    #[test]
    fn tab_at_maps_label_spans_and_misses_the_gap() {
        let tabs = Tabs::new(vec![named("a.rs", ""), dirtied(named("b.rs", ""))]);
        // Drawn as " a.rs  b.rs+    ": labels span 0..6 and 6..13.
        assert_eq!(tab_at(&tabs, 16, 0), Some(0));
        assert_eq!(tab_at(&tabs, 16, 5), Some(0));
        assert_eq!(tab_at(&tabs, 16, 6), Some(1));
        assert_eq!(tab_at(&tabs, 16, 12), Some(1));
        assert_eq!(tab_at(&tabs, 16, 13), None);
    }

    #[test]
    fn tab_at_matches_an_overflowed_bar() {
        let mut tabs = Tabs::new(vec![
            named("aa.rs", ""),
            named("bb.rs", ""),
            named("cc.rs", ""),
        ]);
        tabs.activate(2);
        // Drawn as " bb.rs  cc.rs  ": aa.rs scrolled off the left.
        assert_eq!(tab_at(&tabs, 15, 0), Some(1));
        assert_eq!(tab_at(&tabs, 15, 7), Some(2));
        assert_eq!(tab_at(&tabs, 15, 13), Some(2));
        assert_eq!(tab_at(&tabs, 15, 14), None);
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

    /// The underline flags of a row: `_` where set, space where not.
    fn ul_row(screen: &Screen, y: u16) -> String {
        let (width, _) = screen.size();
        (0..width)
            .map(|x| {
                if screen.get(x, y).unwrap().underlined() {
                    '_'
                } else {
                    ' '
                }
            })
            .collect()
    }

    fn render_search(
        text: &str,
        width: u16,
        height: u16,
        view: View,
        search: Highlights,
    ) -> Screen {
        let tabs = tabs_of(Document::from_str(text), view);
        let mut screen = Screen::new(width, height);
        let mut scratch = String::new();
        draw(&mut screen, &tabs, &mut scratch, "", None, Some(search));
        screen
    }

    #[test]
    fn matches_underline_and_the_current_one_reverses() {
        let starts = [0, 3, 6];
        let search = Highlights {
            starts: &starts,
            len: 2,
            current: Some(1),
        };
        let screen = render_search("ab ab ab", 11, 3, View::default(), search);
        assert_eq!(ul_row(&screen, 1), "  __    __ ");
        assert_eq!(sel_row(&screen, 1), "     ##    ");
    }

    #[test]
    fn match_highlights_clip_to_the_viewport_and_span_wide_glyphs() {
        // "abcdef" with columns 0-2 scrolled off: only the "de" of the
        // "def" match is visible.
        let starts = [3];
        let search = Highlights {
            starts: &starts,
            len: 3,
            current: None,
        };
        let screen = render_search("abcdef", 4, 3, View::test_at(0, 0, 3), search);
        assert_eq!(ul_row(&screen, 1), "  __");

        let starts = [1];
        let search = Highlights {
            starts: &starts,
            len: 1,
            current: None,
        };
        let screen = render_search("a日b", 8, 3, View::default(), search);
        assert_eq!(ul_row(&screen, 1), "   __   ");
    }

    #[test]
    fn stale_match_starts_past_the_text_highlight_nothing() {
        let starts = [2, 90, 500];
        let search = Highlights {
            starts: &starts,
            len: 2,
            current: Some(2),
        };
        let screen = render_search("abcd", 8, 3, View::default(), search);
        assert_eq!(ul_row(&screen, 1), "    __  ");
        assert_eq!(sel_row(&screen, 1), "        ");
    }

    #[test]
    fn selection_highlights_its_extent_and_nothing_else() {
        let view = View::test_at(7, 0, 0).with_anchor(2);
        let (screen, _) = render("hello world", 14, 3, view);
        assert_eq!(row(&screen, 1), "1 hello world ");
        assert_eq!(sel_row(&screen, 1), "    #####     ");
    }

    #[test]
    fn no_selection_or_empty_selection_highlights_nothing() {
        let (screen, _) = render("hello", 9, 3, View::default());
        assert_eq!(sel_row(&screen, 1), "         ");

        let collapsed = View::test_at(2, 0, 0).with_anchor(2);
        let (screen, _) = render("hello", 9, 3, collapsed);
        assert_eq!(sel_row(&screen, 1), "         ");
    }

    #[test]
    fn selection_across_lines_cues_the_terminator() {
        let view = View::test_at(4, 0, 0).with_anchor(1);
        let (screen, _) = render("ab\ncd", 7, 4, view);
        assert_eq!(sel_row(&screen, 1), "   ##  "); // b plus the terminator
        assert_eq!(sel_row(&screen, 2), "  #    "); // c
    }

    #[test]
    fn selected_empty_line_shows_its_terminator_cue() {
        let view = View::test_at(4, 0, 0).with_anchor(0);
        let (screen, _) = render("a\n\nb", 6, 5, view);
        assert_eq!(sel_row(&screen, 1), "  ##  ");
        assert_eq!(sel_row(&screen, 2), "  #   ");
        assert_eq!(sel_row(&screen, 3), "  #   ");
    }

    #[test]
    fn selected_tabs_and_wide_glyphs_highlight_their_whole_span() {
        let view = View::test_at(3, 0, 0).with_anchor(0);
        let (screen, _) = render("a\tb", 12, 3, view);
        assert_eq!(row(&screen, 1), "1 a       b ");
        assert_eq!(sel_row(&screen, 1), "  ######### ");

        let view = View::test_at(1, 0, 0).with_anchor(0);
        let (screen, _) = render("日x", 7, 3, view);
        assert_eq!(sel_row(&screen, 1), "  ##   ");
    }

    #[test]
    fn selection_clips_to_the_viewport() {
        // "abcdef" with columns 0-2 scrolled off and a 2-column text area:
        // only d and e are visible of the d-f selection.
        let view = View::test_at(6, 0, 3).with_anchor(3);
        let (screen, _) = render("abcdef", 4, 3, view);
        assert_eq!(row(&screen, 1), "1 de");
        assert_eq!(sel_row(&screen, 1), "  ##");
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(0, 0), (0, 5), (5, 0), (1, 1), (2, 2), (3, 1), (1, 3)] {
            render("日本\ntext", w, h, View::default());
            let mut tabs = Tabs::new(vec![
                named("aa.rs", "日本"),
                dirtied(named("bb.rs", "text")),
                named("a_very_long_file_name.rs", ""),
            ]);
            tabs.activate(2);
            render_tabs(&tabs, w, h);

            let starts = [0, 7, usize::MAX - 1];
            let search = Highlights {
                starts: &starts,
                len: 2,
                current: Some(9),
            };
            render_search("日本\ntext", w, h, View::default(), search);
        }
    }
}
