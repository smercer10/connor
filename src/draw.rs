//! Renders the tabs into a cleared screen: tab bar, line-number gutter,
//! text area, status line. Pure — no terminal — so all of it unit-tests.

use std::fmt::Write as _;

use ropey::Rope;
use unicode_width::UnicodeWidthChar;

use crate::compare::Compare;
use crate::diff;
use crate::doc::Document;
use crate::grapheme::{self, RopeGraphemes};
use crate::grep::{Grep, Row};
use crate::keymap;
use crate::picker::Picker;
use crate::screen::Screen;
use crate::search::Highlights;
use crate::status::{self, Status};
use crate::syntax::Span;
use crate::tabs::{Tab, Tabs};
use crate::tree::Tree;

/// The screen columns the text area starts at: the gutter holds every line
/// number right-aligned plus one space, and — for a document inside a git
/// project — one more column for its change mark. Every coordinate that
/// crosses the gutter goes through here, so mouse mapping and cursor
/// scrolling follow the mark column without knowing about it.
pub fn gutter_width(tab: &Tab) -> usize {
    digits(tab.doc.line_count()) + 1 + usize::from(tab.diff.in_repo())
}

/// Rows available to text: everything but the tab bar and the status line.
pub fn text_height(screen_height: u16) -> usize {
    usize::from(screen_height).saturating_sub(2)
}

/// Widest the tree sidebar grows.
const TREE_MAX_W: usize = 30;

/// Columns the editor keeps when the sidebar squeezes it.
const EDITOR_MIN_W: usize = 30;

/// Below this the sidebar disappears entirely rather than shrink to junk.
const TREE_HIDE_W: usize = 45;

/// The sidebar's columns, border included: fixed, shrinking to keep the
/// editor at least `EDITOR_MIN_W`, gone on a terminal too narrow for both.
pub fn tree_width(open: bool, screen_w: u16) -> usize {
    let width = usize::from(screen_w);
    if !open || width < TREE_HIDE_W {
        return 0;
    }
    TREE_MAX_W.min(width - EDITOR_MIN_W)
}

/// Per-char decorations painted over the text area — search matches and
/// syntax colour spans — borrowed from their owners for the frame.
#[derive(Default)]
pub struct Marks<'a> {
    pub search: Option<Highlights<'a>>,
    pub syntax: &'a [Span],
}

/// The sidebar as one frame sees it: the tree, whether it holds focus, and
/// the project's standing against HEAD that marks its rows.
pub struct Sidebar<'a> {
    pub tree: &'a Tree,
    pub focused: bool,
    pub status: &'a Status,
}

/// Draws one frame into a cleared screen and returns the cursor's screen
/// cell. O(viewport): only visible lines are walked, and each only to the
/// viewport's right edge. `scratch` is reused across frames so steady-state
/// drawing never heap-allocates. A non-empty `notice` — a message or a
/// mini-prompt — takes over the status line until the next keypress,
/// hiding its right-aligned help hint;
/// `status_caret` parks the cursor after that many of the notice's chars
/// while a prompt is editing there. `marks.search` underlines every match
/// and reverses the current one; `marks.syntax` colours the sorted,
/// non-overlapping spans it holds — empty means plain text. An open
/// `sidebar` shifts the gutter and text right; its focus is the selection
/// bar — the caller hides the terminal cursor while the tree holds it — and
/// its rows carry the project's change marks, as the tab labels do.
pub fn draw(
    screen: &mut Screen,
    tabs: &Tabs,
    scratch: &mut String,
    notice: &str,
    status_caret: Option<usize>,
    marks: Marks,
    sidebar: Option<Sidebar>,
) -> (u16, u16) {
    let (width, height) = screen.size();
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let tree_w = tree_width(sidebar.is_some(), width);
    let width = usize::from(width);
    let text_h = text_height(height);
    let tab = tabs.active();
    let Tab { doc, view, .. } = tab;
    let gutter_w = gutter_width(tab);
    let mark_w = usize::from(tab.diff.in_repo());
    let text_w = width.saturating_sub(tree_w + gutter_w);

    if height > 1 {
        draw_tab_bar(screen, tabs, scratch, width);
    }

    let sel = view.selection();
    let mut buf = [0; 16];
    // Spans are sorted and non-overlapping, and the viewport walk below
    // ascends through char indices, so one advancing index serves every
    // visible line — O(1) amortised per cluster, no search.
    let mut span_i = 0;
    // Hunks are sorted and non-overlapping too, but they span the whole
    // document rather than the viewport, so the walk starts where the
    // viewport does instead of scanning up to it.
    let changes = tab.diff.hunks();
    let mut hunk_i = changes.partition_point(|h| h.end <= view.scroll_line);
    for y in 0..text_h {
        let line = view.scroll_line + y;
        if line >= doc.line_count() {
            break;
        }
        let row = (y + 1) as u16;
        scratch.clear();
        let _ = write!(scratch, "{:>w$} ", line + 1, w = gutter_w - 1 - mark_w);
        screen.set_text(tree_w as u16, row, scratch);
        while changes.get(hunk_i).is_some_and(|h| h.end <= line) {
            hunk_i += 1;
        }
        if let Some(h) = changes.get(hunk_i)
            && h.start <= line
        {
            let x = (tree_w + gutter_w - 1) as u16;
            screen.set(x, row, h.kind.glyph());
            screen.set_fg(x, row, h.kind.color());
        }
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
                    let x = (tree_w + gutter_w + col - view.scroll_col) as u16;
                    let first = cluster.chars().next().unwrap_or(' ');
                    if first.is_control() {
                        screen.set_grapheme(x, row, "\u{FFFD}", 1);
                    } else {
                        screen.set_grapheme(x, row, cluster, cluster_w as u8);
                    }
                }
                while marks.syntax.get(span_i).is_some_and(|s| s.end <= c_start) {
                    span_i += 1;
                }
                if let Some(s) = marks.syntax.get(span_i)
                    && s.start < c_end
                    && c_start < s.end
                {
                    for c in col.max(view.scroll_col)..end.min(right) {
                        let x = (tree_w + gutter_w + c - view.scroll_col) as u16;
                        screen.set_fg(x, row, s.color);
                    }
                }
                if selected {
                    for c in col.max(view.scroll_col)..end.min(right) {
                        let x = (tree_w + gutter_w + c - view.scroll_col) as u16;
                        screen.set_reversed(x, row, true);
                    }
                }
                // Match starts sit on char indices, so like the selection a
                // cluster is wholly in or out; binary search keeps the pass
                // O(log matches) per visible cluster. A stale out-of-range
                // start simply intersects nothing.
                if let Some(h) = &marks.search
                    && h.len > 0
                {
                    let i = h
                        .starts
                        .partition_point(|&s| s.saturating_add(h.len) <= c_start);
                    if h.starts.get(i).is_some_and(|&s| s < c_end) {
                        for c in col.max(view.scroll_col)..end.min(right) {
                            let x = (tree_w + gutter_w + c - view.scroll_col) as u16;
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
            let x = (tree_w + gutter_w + col - view.scroll_col) as u16;
            screen.set_reversed(x, row, true);
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
        // The keymap's discoverability bootstrap: a right-aligned pointer
        // at the overlay, yielding whenever the left content needs the room.
        let left: usize = scratch.chars().map(char_cols).sum();
        scratch.clear();
        keymap::write_help_hint(scratch);
        let hint = scratch.chars().count();
        if hint > 0 && left + 2 + hint <= width {
            screen.set_text((width - hint) as u16, height - 1, scratch);
        }
    } else {
        screen.set_text(0, height - 1, notice);
    }

    if tree_w > 0
        && let Some(sidebar) = sidebar
    {
        draw_tree(screen, &sidebar, tree_w);
    }
    if let Some(chars) = status_caret {
        let col: usize = notice.chars().take(chars).map(char_cols).sum();
        return (col.min(width - 1) as u16, height - 1);
    }
    let cx = (tree_w + gutter_w + vcol.saturating_sub(view.scroll_col)).min(width - 1) as u16;
    let cy = (1 + line
        .saturating_sub(view.scroll_line)
        .min(text_h.saturating_sub(1)))
    .min(usize::from(height) - 1) as u16;
    (cx, cy)
}

/// Draws the tree sidebar over rows 1..=text_h: a border column on its
/// right edge, one indented row per visible entry with `▸`/`▾` marking
/// collapsed and expanded directories, the selection in reverse video
/// while the tree holds focus, and the edited file's name underlined.
/// Every cell write bounds-checks, so degenerate sizes clip, not panic.
///
/// Inside a repository the last inner column is the change mark's, held for
/// every row rather than only marked ones so a name never shifts sideways
/// as marks come and go — the gutter's rule. Outside one the column is not
/// reserved at all and the sidebar is exactly what it was before.
fn draw_tree(screen: &mut Screen, sidebar: &Sidebar, tree_w: usize) {
    let Sidebar {
        tree,
        focused,
        status,
    } = *sidebar;
    let (_, height) = screen.size();
    let text_h = text_height(height);
    let inner = tree_w - 1;
    let mark_w = usize::from(status.in_repo());
    for k in 0..text_h {
        screen.set(inner as u16, (1 + k) as u16, '│');
    }
    // A resize may have shrunk the window since the last state change;
    // drawing clamps read-only rather than mutating the scroll.
    let scroll = tree.scroll().min(tree.visible_len().saturating_sub(1));
    for k in 0..text_h {
        let i = scroll + k;
        let y = (1 + k) as u16;
        if i >= tree.visible_len() {
            // The ellipsis row says the walk is still feeding entries in.
            if tree.walking() && i == tree.visible_len() {
                screen.set(0, y, '…');
            }
            break;
        }
        let r = tree.row(i);
        // Deep nesting caps the indent so the name keeps some columns.
        let indent = (r.depth * 2).min(inner.saturating_sub(8));
        if r.dir {
            screen.set(indent as u16, y, if r.expanded { '▾' } else { '▸' });
        }
        let name_x = indent + 2;
        let drawn = draw_head(
            screen,
            name_x,
            y,
            r.name,
            inner.saturating_sub(name_x + mark_w),
        );
        // A directory carries the mark of everything beneath it, so a
        // change under a collapsed row is never invisible.
        if mark_w > 0
            && let Some(mark) = status.mark(r.path)
        {
            let x = (inner - mark_w) as u16;
            screen.set(x, y, status::DOT);
            screen.set_fg(x, y, mark.color());
        }
        if r.active {
            for x in name_x..name_x + drawn {
                screen.set_underlined(x as u16, y, true);
            }
        }
        if focused && i == tree.selected() {
            for x in 0..inner {
                screen.set_reversed(x as u16, y, true);
            }
        }
    }
}

/// Draws `text` within `avail` columns starting at `x`. One that doesn't
/// fit keeps its head — for a name, the stem — before a trailing ellipsis.
/// Returns the columns drawn.
fn draw_head(screen: &mut Screen, x: usize, y: u16, text: &str, avail: usize) -> usize {
    let total: usize = text.chars().map(char_cols).sum();
    if total > avail {
        if avail == 0 {
            return 0;
        }
        let mut drawn = 0;
        for ch in text.chars() {
            let w = char_cols(ch);
            if drawn + w > avail - 1 {
                break;
            }
            screen.set((x + drawn) as u16, y, ch);
            drawn += w;
        }
        screen.set((x + drawn) as u16, y, '…');
        return drawn + 1;
    }
    let mut drawn = 0;
    for ch in text.chars() {
        screen.set((x + drawn) as u16, y, ch);
        drawn += char_cols(ch);
    }
    drawn
}

/// The rules that aren't bindings, shown inside the help box.
const HELP_FOOTER: &str = "type to insert · shift+move selects";

/// Columns between the help box's columns of bindings.
const HELP_GAP: usize = 3;

/// Rows the side-by-side view has for content: the body less its header.
pub fn compare_rows(text_h: usize) -> usize {
    text_h.saturating_sub(1)
}

/// Draws the side-by-side view over the body — the tab bar and status line
/// stay — and returns whether any visible line ran past a pane's right
/// edge. Nothing here knows a line's width without walking it, so that
/// answer is what bounds the view's horizontal scroll.
///
/// The panes take the whole width, sidebar included: two of them want the
/// room, and the sidebar comes back untouched when the view closes.
pub fn draw_compare(
    screen: &mut Screen,
    compare: &Compare,
    doc: &Document,
    scratch: &mut String,
) -> bool {
    let (screen_w, height) = screen.size();
    let text_h = text_height(height);
    // Two gutters and a divider need somewhere to be.
    if usize::from(screen_w) < 3 || text_h == 0 {
        return false;
    }
    let width = usize::from(screen_w);
    // The body is drawn already and none of it belongs here: a fresh cell
    // per position also clears the reverse and underline flags of whatever
    // it covers, the way `overlay_box` clears its footprint.
    for y in 1..=text_h {
        for x in 0..width {
            screen.set(x as u16, y as u16, ' ');
        }
    }
    // The divider belongs to neither pane, the way the sidebar's does.
    let left_w = (width - 1) / 2;
    let right_x = left_w + 1;
    let right_w = width - right_x;
    for y in 1..=text_h {
        screen.set(left_w as u16, y as u16, '│');
    }

    // A disk view names the file itself, and its read is already done —
    // only a HEAD view can be standing beside an empty pane it has no
    // answer for yet, and only once a lookup has answered is an empty one
    // HEAD's answer rather than the absence of one.
    let left_label = if compare.resolving().is_some() {
        "disk"
    } else if compare.tracked() || compare.note().is_some() {
        "HEAD"
    } else {
        "not in HEAD"
    };
    draw_head(screen, 0, 1, left_label, left_w);
    scratch.clear();
    let _ = write!(scratch, "{}", doc.name());
    if doc.dirty() {
        scratch.push_str(" [+]");
    }
    let name_w = draw_head(screen, right_x, 1, scratch, right_w);
    scratch.clear();
    match (compare.at(), compare.changes()) {
        (_, 0) => scratch.push_str("no changes"),
        (0, 1) => scratch.push_str("1 change"),
        (0, n) => {
            let _ = write!(scratch, "{n} changes");
        }
        (at, n) => {
            let _ = write!(scratch, "{at}/{n} changes");
        }
    }
    // The count yields to the name rather than overwrite it, like the
    // status line's help hint.
    let count_w: usize = scratch.chars().map(char_cols).sum();
    if compare.note().is_none() && name_w + 2 + count_w <= right_w {
        screen.set_text((right_x + right_w - count_w) as u16, 1, scratch);
    }
    // Underlined edge to edge: the rule that separates header from panes.
    for x in 0..width {
        screen.set_underlined(x as u16, 1, true);
    }

    let rows = compare_rows(text_h);
    if rows == 0 {
        return false;
    }
    if let Some(note) = compare.note() {
        let note_w: usize = note.chars().map(char_cols).sum();
        draw_head(
            screen,
            width.saturating_sub(note_w) / 2,
            (2 + rows / 2) as u16,
            note,
            width,
        );
        return false;
    }

    let bands = compare.bands();
    let (top, scroll_col) = (compare.top(), compare.scroll_col());
    // Bands tile the whole view rather than the viewport, so the walk
    // starts where the viewport does instead of scanning up to it — the
    // gutter's rule for hunks.
    let mut band_i = bands.partition_point(|b| b.row + b.height() <= top);
    let baseline = compare.baseline();
    let left_g = digits(baseline.len_lines()) + 2;
    let right_g = digits(doc.line_count()) + 2;
    let mut clipped = false;
    for k in 0..rows {
        let row = top + k;
        while bands.get(band_i).is_some_and(|b| b.row + b.height() <= row) {
            band_i += 1;
        }
        let Some(band) = bands.get(band_i) else {
            break;
        };
        let y = (2 + k) as u16;
        let offset = row - band.row;
        // The side with no line here leaves its half blank, line number
        // and all: an absent row is what a padded band has to say.
        if band.head.start + offset < band.head.end {
            clipped |= draw_compare_line(
                screen,
                Pane {
                    x: 0,
                    width: left_w,
                    gutter: left_g,
                    // Left is what HEAD had; red is what a reader of any
                    // diff already reads that as.
                    fg: if band.same { 0 } else { 2 },
                },
                y,
                baseline,
                band.head.start + offset,
                scroll_col,
                scratch,
            );
        }
        let line = band.buffer.start + offset;
        // An edit can outrun the diff it will be aligned by; until the new
        // bands land, a line the buffer no longer has simply isn't drawn.
        if line < band.buffer.end && line < doc.line_count() {
            clipped |= draw_compare_line(
                screen,
                Pane {
                    x: right_x,
                    width: right_w,
                    gutter: right_g,
                    fg: if band.same { 0 } else { 3 },
                },
                y,
                doc.rope(),
                line,
                scroll_col,
                scratch,
            );
        }
    }
    clipped
}

/// One pane of the side-by-side view: where it sits, how wide its own line
/// numbers make its gutter, and the colour its differing rows take.
struct Pane {
    x: usize,
    width: usize,
    gutter: usize,
    fg: u8,
}

/// One row of one pane: its line number, its change bar, and the line
/// itself clipped at the pane's right edge. Returns whether the line ran
/// past that edge. The main text area's walk without its decorations —
/// there is no selection, no search and no cursor in here, and colour is
/// spent saying what changed rather than what the grammar is.
fn draw_compare_line(
    screen: &mut Screen,
    pane: Pane,
    y: u16,
    rope: &Rope,
    line: usize,
    scroll_col: usize,
    scratch: &mut String,
) -> bool {
    scratch.clear();
    let _ = write!(scratch, "{:>w$} ", line + 1, w = pane.gutter - 2);
    // A pane narrower than its own gutter must not spill over the divider
    // into its neighbour: digits and a space, so bytes are columns here.
    scratch.truncate(pane.width);
    screen.set_text(pane.x as u16, y, scratch);
    if pane.fg != 0 && pane.gutter <= pane.width {
        let x = (pane.x + pane.gutter - 1) as u16;
        screen.set(x, y, diff::BAR);
        screen.set_fg(x, y, pane.fg);
    }
    let avail = pane.width.saturating_sub(pane.gutter);
    if avail == 0 {
        return false;
    }
    let x0 = pane.x + pane.gutter;
    let slice = diff::strip_terminator(rope.line(line));
    let right = scroll_col + avail;
    let mut buf = [0; 16];
    let mut col = 0;
    let mut more = false;
    for range in RopeGraphemes::new(slice) {
        if col >= right {
            more = true;
            break;
        }
        let cluster = grapheme::grapheme_str(slice, range, &mut buf);
        let cluster_w = grapheme::grapheme_width(cluster, col);
        let end = col + cluster_w;
        if end > scroll_col {
            // Tabs and clusters clipped by either edge stay blank, exactly
            // as they do in the text area.
            if cluster != "\t" && col >= scroll_col && end <= right {
                let x = (x0 + col - scroll_col) as u16;
                let first = cluster.chars().next().unwrap_or(' ');
                if first.is_control() {
                    screen.set_grapheme(x, y, "\u{FFFD}", 1);
                } else {
                    screen.set_grapheme(x, y, cluster, cluster_w as u8);
                }
            }
            if pane.fg != 0 {
                for c in col.max(scroll_col)..end.min(right) {
                    screen.set_fg((x0 + c - scroll_col) as u16, y, pane.fg);
                }
            }
        }
        col = end;
    }
    more || col > right
}

/// Draws the keymap overlay: a centered box over the text area, one row per
/// binding under underlined section titles, flowed into as few columns as
/// fit. Drawn after `draw`, straight over the text; the tab bar and status
/// line stay visible. Every cell write bounds-checks, so a screen too small
/// for the box clips it instead of panicking.
pub fn draw_help(screen: &mut Screen, scratch: &mut String) {
    let (width, height) = screen.size();
    let width = usize::from(width);
    let text_h = text_height(height);

    let mut label_w = 0;
    let mut desc_w = 0;
    let mut rows = 0;
    for section in keymap::KEYMAP {
        rows += 1 + section.bindings.len();
        desc_w = desc_w.max(section.title.chars().count());
        for binding in section.bindings {
            scratch.clear();
            binding.write_label(scratch);
            label_w = label_w.max(scratch.chars().count());
            desc_w = desc_w.max(binding.what.chars().count());
        }
    }

    // Borders and the footer take three rows beyond the binding rows.
    let cols = if rows + 3 <= text_h { 1 } else { 2 };
    let per_col = rows.div_ceil(cols);
    let col_w = label_w + 2 + desc_w;
    let box_w = (cols * col_w + (cols - 1) * HELP_GAP + 4)
        .max(HELP_FOOTER.chars().count() + 4)
        .min(width);
    let box_h = (per_col + 3).min(text_h);
    let x0 = (width - box_w) / 2;
    let y0 = 1 + (text_h - box_h) / 2;

    overlay_box(screen, x0, y0, box_w, box_h);

    let geom = HelpGeom {
        x0,
        y0,
        box_w,
        col_w,
        per_col,
        content_h: box_h.saturating_sub(3),
    };
    let mut i = 0;
    for section in keymap::KEYMAP {
        help_row(screen, &geom, i, section.title, true);
        i += 1;
        for binding in section.bindings {
            scratch.clear();
            binding.write_label(scratch);
            while scratch.chars().count() < label_w + 2 {
                scratch.push(' ');
            }
            scratch.push_str(binding.what);
            help_row(screen, &geom, i, scratch, false);
            i += 1;
        }
    }

    if geom.content_h > 0 && HELP_FOOTER.chars().count() + 4 <= box_w {
        let y = (y0 + box_h - 2) as u16;
        screen.set_text((x0 + 2) as u16, y, HELP_FOOTER);
    }
}

/// Where the help box sits and how its content rows flow into columns.
struct HelpGeom {
    x0: usize,
    y0: usize,
    box_w: usize,
    col_w: usize,
    per_col: usize,
    content_h: usize,
}

/// Places content row `i` in its flowed column, underlined for a section
/// title. Every help glyph is one column wide, so clipping by char count
/// keeps a narrow box's right border intact.
fn help_row(screen: &mut Screen, g: &HelpGeom, i: usize, text: &str, underline: bool) {
    let (col, row) = (i / g.per_col, i % g.per_col);
    if row >= g.content_h {
        return;
    }
    let x = g.x0 + 2 + col * (g.col_w + HELP_GAP);
    let y = (g.y0 + 1 + row) as u16;
    let avail = (g.x0 + g.box_w).saturating_sub(1).saturating_sub(x);
    for (c, ch) in text.chars().take(avail).enumerate() {
        let x = (x + c) as u16;
        screen.set(x, y, ch);
        if underline {
            screen.set_underlined(x, y, true);
        }
    }
}

/// Paints a centered overlay's border and blank interior. A fresh cell per
/// footprint position also clears the reverse and underline flags of
/// whatever the box covers.
fn overlay_box(screen: &mut Screen, x0: usize, y0: usize, box_w: usize, box_h: usize) {
    for y in y0..y0 + box_h {
        for x in x0..x0 + box_w {
            let edge_x = x == x0 || x == x0 + box_w - 1;
            let edge_y = y == y0 || y == y0 + box_h - 1;
            let ch = match (edge_x, edge_y) {
                (true, true) => match (x == x0, y == y0) {
                    (true, true) => '┌',
                    (false, true) => '┐',
                    (true, false) => '└',
                    (false, false) => '┘',
                },
                (true, false) => '│',
                (false, true) => '─',
                (false, false) => ' ',
            };
            screen.set(x as u16, y as u16, ch);
        }
    }
}

/// Widest the picker box grows: deep paths still fit, and a wide terminal
/// keeps its margins.
const PICK_MAX_W: usize = 80;

/// Result rows the picker shows at most. A fixed row count keeps the box
/// still while the match count churns underneath it.
const PICK_ROWS: usize = 16;

/// Draws the file-picker overlay: a centered box over the text area, the
/// query on its top row beside the match count, ranked results below with
/// the selection in reverse video. Drawn after `draw`, straight over the
/// text; the tab bar and status line stay visible. Returns the cursor cell
/// at the query's caret. Every cell write bounds-checks, so a screen too
/// small for the box clips it instead of panicking.
pub fn draw_picker(screen: &mut Screen, picker: &Picker, scratch: &mut String) -> (u16, u16) {
    let (width, height) = screen.size();
    let width = usize::from(width);
    let text_h = text_height(height);

    let box_w = if width > PICK_MAX_W + 4 {
        PICK_MAX_W
    } else {
        width
    };
    let visible = PICK_ROWS.min(text_h.saturating_sub(3));
    let box_h = (visible + 3).min(text_h);
    let x0 = (width - box_w) / 2;
    let y0 = 1 + (text_h - box_h) / 2;

    overlay_box(screen, x0, y0, box_w, box_h);

    let avail = box_w.saturating_sub(4);
    let query_y = (y0 + 1) as u16;

    // The match count sits at the interior's right edge, with a trailing
    // ellipsis while the walk is still feeding files in.
    scratch.clear();
    let _ = write!(scratch, "{}/{}", picker.matched_len(), picker.total());
    if picker.walking() {
        scratch.push('…');
    }
    let count_w = scratch.chars().count();
    let counted = box_h >= 3 && avail >= count_w + 5;
    if counted {
        screen.set_text((x0 + 2 + avail - count_w) as u16, query_y, scratch);
    }

    let mut caret = x0 + 2;
    if box_h >= 3 {
        screen.set_text(caret as u16, query_y, "> ");
        caret += 2;
        let budget = avail
            .saturating_sub(2)
            .saturating_sub(if counted { count_w + 1 } else { 0 });
        caret += draw_tail(screen, caret, query_y, picker.query(), budget);
    }

    // Stateless scroll: the selection stays visible, pinned to the bottom
    // edge once it runs past the box.
    let top = if visible > 0 {
        picker.selected().saturating_sub(visible - 1)
    } else {
        0
    };
    for k in 0..visible.min(picker.matched_len().saturating_sub(top)) {
        let rank = top + k;
        let y = (y0 + 2 + k) as u16;
        draw_tail(screen, x0 + 2, y, picker.shown(rank), avail);
        if rank == picker.selected() {
            for x in x0 + 1..x0 + box_w.saturating_sub(1) {
                screen.set_reversed(x as u16, y, true);
            }
        }
    }

    (caret.min(width.saturating_sub(1)) as u16, query_y)
}

/// Draws the project-search overlay: the picker's box with the query on its
/// top row beside the hit count, hits below grouped under underlined file
/// headers, the selected hit in reverse video. Drawn after `draw`, straight
/// over the text; the tab bar and status line stay visible. Returns the
/// cursor cell at the query's caret. Every cell write bounds-checks, so a
/// screen too small for the box clips it instead of panicking.
pub fn draw_grep(screen: &mut Screen, grep: &Grep, scratch: &mut String) -> (u16, u16) {
    let (width, height) = screen.size();
    let width = usize::from(width);
    let text_h = text_height(height);

    let box_w = if width > PICK_MAX_W + 4 {
        PICK_MAX_W
    } else {
        width
    };
    let visible = PICK_ROWS.min(text_h.saturating_sub(3));
    let box_h = (visible + 3).min(text_h);
    let x0 = (width - box_w) / 2;
    let y0 = 1 + (text_h - box_h) / 2;

    overlay_box(screen, x0, y0, box_w, box_h);

    let avail = box_w.saturating_sub(4);
    let query_y = (y0 + 1) as u16;

    // The hit count sits at the interior's right edge: `+` marks the cap
    // ending the search early, a trailing ellipsis one still running.
    scratch.clear();
    let _ = write!(scratch, "{}", grep.hit_count());
    if grep.truncated() {
        scratch.push('+');
    }
    scratch.push_str(" hits");
    if grep.searching() {
        scratch.push('…');
    }
    let count_w = scratch.chars().count();
    let counted = box_h >= 3 && avail >= count_w + 5;
    if counted {
        screen.set_text((x0 + 2 + avail - count_w) as u16, query_y, scratch);
    }

    let mut caret = x0 + 2;
    if box_h >= 3 {
        screen.set_text(caret as u16, query_y, "> ");
        caret += 2;
        let budget = avail
            .saturating_sub(2)
            .saturating_sub(if counted { count_w + 1 } else { 0 });
        caret += draw_tail(screen, caret, query_y, grep.query(), budget);
    }

    // Stateless scroll: the selected hit stays visible, pinned to the
    // bottom edge once it runs past the box.
    let top = if visible > 0 && grep.hit_count() > 0 {
        grep.selected_display_row().saturating_sub(visible - 1)
    } else {
        0
    };
    for k in 0..visible.min(grep.display_len().saturating_sub(top)) {
        let y = (y0 + 2 + k) as u16;
        match grep.row(top + k) {
            Row::File(path) => {
                let drawn = draw_tail(screen, x0 + 2, y, path, avail);
                for c in 0..drawn {
                    screen.set_underlined((x0 + 2 + c) as u16, y, true);
                }
            }
            Row::Hit {
                line,
                preview,
                selected,
            } => {
                scratch.clear();
                let _ = write!(scratch, "  {line}: ");
                // Every prefix glyph is one column wide.
                let head = scratch.chars().count();
                if head < avail {
                    screen.set_text((x0 + 2) as u16, y, scratch);
                    draw_head(screen, x0 + 2 + head, y, preview, avail - head);
                }
                if selected {
                    for x in x0 + 1..x0 + box_w.saturating_sub(1) {
                        screen.set_reversed(x as u16, y, true);
                    }
                }
            }
        }
    }

    (caret.min(width.saturating_sub(1)) as u16, query_y)
}

/// Draws `text` within `avail` columns starting at `x`. One that doesn't
/// fit keeps its tail — for a path, the filename — behind a leading
/// ellipsis. Returns the columns drawn.
fn draw_tail(screen: &mut Screen, x: usize, y: u16, text: &str, avail: usize) -> usize {
    let total: usize = text.chars().map(char_cols).sum();
    let mut drawn = 0;
    let mut start = 0;
    if total > avail {
        if avail == 0 {
            return 0;
        }
        screen.set(x as u16, y, '…');
        drawn = 1;
        let budget = avail - 1;
        let mut cols = 0;
        start = text.len();
        for (i, ch) in text.char_indices().rev() {
            let w = char_cols(ch);
            if cols + w > budget {
                break;
            }
            cols += w;
            start = i;
        }
    }
    for ch in text[start..].chars() {
        screen.set((x + drawn) as u16, y, ch);
        drawn += char_cols(ch);
    }
    drawn
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
        if tab.mark.is_some() {
            scratch.push(status::DOT);
        }
        push_shown_name(&tab.doc.name(), scratch);
        if tab.doc.dirty() {
            scratch.push('+');
        }
        if tab.doc.conflict() {
            scratch.push('!');
        }
        scratch.push(' ');
        screen.set_text(x as u16, 0, scratch);
        if let Some(mark) = tab.mark {
            screen.set_fg((x + 1) as u16, 0, mark.color());
        }
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

/// The screen columns one tab's label occupies: padding, the change mark,
/// the shown name, and the dirty and conflict marks. Click mapping and
/// overflow both measure through here, so a label that gains a mark stays
/// clickable where it is drawn.
fn label_width(tab: &Tab) -> usize {
    2 + usize::from(tab.mark.is_some())
        + shown_name_cols(&tab.doc.name())
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
    use crate::compare::Compare;
    use crate::diff::{Change, Diff, Hunk};
    use crate::doc::{Caret, Document, EditKind};
    use crate::status::Mark;
    use crate::view::View;

    fn tabs_of(doc: Document, view: View) -> Tabs {
        let mut tabs = Tabs::new(vec![doc]);
        tabs.active_mut().view = view;
        tabs
    }

    fn render_tabs(tabs: &Tabs, width: u16, height: u16) -> (Screen, (u16, u16)) {
        let mut screen = Screen::new(width, height);
        let mut scratch = String::new();
        let cursor = draw(
            &mut screen,
            tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
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

    /// A document standing inside a repository at a fixed set of marks.
    fn marked(text: &str, view: View, hunks: Vec<Hunk>) -> Tabs {
        let mut tabs = tabs_of(Document::from_str(text), view);
        tabs.active_mut().diff = Diff::test_marks(hunks);
        tabs
    }

    /// A gutter mark. The HEAD side is the side-by-side view's business,
    /// not the gutter's, so these carry none.
    fn hunk(start: usize, end: usize, kind: Change) -> Hunk {
        Hunk {
            start,
            end,
            kind,
            head_start: 0,
            head_end: 0,
        }
    }

    #[test]
    fn the_mark_column_appears_only_inside_a_repository() {
        // Outside one the gutter is what it always was, to the column.
        let (plain, plain_cursor) = render("hello\nworld", 14, 5, View::default());
        assert_eq!(row(&plain, 1), "1 hello       ");
        assert_eq!(plain_cursor, (2, 1));

        let (screen, cursor) =
            render_tabs(&marked("hello\nworld", View::default(), Vec::new()), 14, 5);
        assert_eq!(row(&screen, 1), "1  hello      ");
        assert_eq!(row(&screen, 2), "2  world      ");
        assert_eq!(cursor, (3, 1));
    }

    #[test]
    fn each_change_marks_its_own_rows() {
        let tabs = marked(
            "a\nb\nc\nd",
            View::default(),
            vec![hunk(0, 1, Change::Added), hunk(2, 3, Change::Changed)],
        );
        let (screen, _) = render_tabs(&tabs, 8, 7);
        assert_eq!(row(&screen, 1), "1 ▍a    ");
        assert_eq!(row(&screen, 2), "2  b    ");
        assert_eq!(row(&screen, 3), "3 ▍c    ");
        assert_eq!(row(&screen, 4), "4  d    ");
        // Green added, yellow changed, and the numbers stay plain.
        assert_eq!(fg_row(&screen, 1), "  3     ");
        assert_eq!(fg_row(&screen, 2), "        ");
        assert_eq!(fg_row(&screen, 3), "  4     ");
    }

    #[test]
    fn removal_marks_sit_on_the_edge_the_lines_left() {
        let tabs = marked(
            "a\nb\nc\nd",
            View::default(),
            vec![
                hunk(1, 2, Change::RemovedAbove),
                hunk(3, 4, Change::RemovedBelow),
            ],
        );
        let (screen, _) = render_tabs(&tabs, 8, 7);
        assert_eq!(row(&screen, 2), "2 ▔b    ");
        assert_eq!(row(&screen, 4), "4 ▁d    ");
        assert_eq!(fg_row(&screen, 2), "  2     ");
        assert_eq!(fg_row(&screen, 4), "  2     ");
    }

    #[test]
    fn a_multi_line_hunk_marks_every_row_it_covers() {
        let tabs = marked(
            "a\nb\nc\nd",
            View::default(),
            vec![hunk(1, 3, Change::Added)],
        );
        let (screen, _) = render_tabs(&tabs, 8, 7);
        assert_eq!(row(&screen, 1), "1  a    ");
        assert_eq!(row(&screen, 2), "2 ▍b    ");
        assert_eq!(row(&screen, 3), "3 ▍c    ");
        assert_eq!(row(&screen, 4), "4  d    ");
    }

    #[test]
    fn marks_never_reach_past_the_last_line() {
        // A hunk set the buffer has already outgrown; the rows below the
        // document stay blank rather than sprouting marks.
        let tabs = marked("a\nb", View::default(), vec![hunk(0, 9, Change::Added)]);
        let (screen, _) = render_tabs(&tabs, 8, 7);
        assert_eq!(row(&screen, 2), "2 ▍b    ");
        assert_eq!(row(&screen, 3), "        ");
        assert_eq!(row(&screen, 4), "        ");
    }

    #[test]
    fn the_marks_hold_their_column_under_a_scroll_and_a_sidebar() {
        let tabs = marked(
            "abcdefghij\nx",
            View::test_at(0, 0, 3),
            vec![hunk(0, 1, Change::Added)],
        );
        let (screen, _) = render_tabs(&tabs, 8, 5);
        assert_eq!(row(&screen, 1), "1 ▍defgh");

        let tree = tree_of(TREE_PATHS, true);
        let mut screen = Screen::new(60, 5);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &marked("hello", View::default(), vec![hunk(0, 1, Change::Changed)]),
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: false,
                status: &Status::test_plain(),
            }),
        );
        let body = row(&screen, 1);
        let border = body.chars().position(|c| c == '│').unwrap();
        assert_eq!(
            body.chars().skip(border + 1).take(4).collect::<String>(),
            "1 ▍h"
        );
    }

    #[test]
    fn the_mark_column_widens_every_coordinate_that_crosses_the_gutter() {
        let mut tabs = tabs_of(Document::from_str("hello"), View::default());
        assert_eq!(gutter_width(tabs.active()), 2);
        tabs.active_mut().diff = Diff::test_marks(Vec::new());
        assert_eq!(gutter_width(tabs.active()), 3);
        // The caret sits one column further right, and a click at that
        // column still lands on the character under the pointer because
        // `View::hit` subtracts the same width.
        let gutter_w = gutter_width(tabs.active());
        let (_, cursor) = render_tabs(&tabs, 12, 4);
        assert_eq!(cursor, (3, 1));
        let Tab { doc, view, .. } = tabs.active_mut();
        view.click(doc, gutter_w, 2, 6, 1, false);
        assert_eq!(view.cursor, 3); // the second 'l' of "hello"
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
    fn the_help_hint_right_aligns_and_yields_to_a_cramped_status() {
        let (screen, _) = render("ab", 40, 3, View::default());
        assert_eq!(row(&screen, 2), "[No Name] · 1:1                  F1 help");

        // One column short of the two-space gap: the hint disappears whole.
        let (screen, _) = render("ab", 23, 3, View::default());
        assert_eq!(row(&screen, 2), "[No Name] · 1:1        ");
    }

    #[test]
    fn a_notice_takes_over_the_status_line() {
        let tabs = tabs_of(Document::from_str("ab"), View::default());
        let mut screen = Screen::new(12, 3);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "saved ab",
            None,
            Marks::default(),
            None,
        );
        assert_eq!(row(&screen, 2), "saved ab    ");
    }

    #[test]
    fn prompt_caret_parks_at_the_notice_end_in_the_status_line() {
        let tabs = tabs_of(Document::from_str("ab"), View::default());
        let mut screen = Screen::new(12, 3);
        let mut scratch = String::new();
        let cursor = draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "open: sr",
            Some(8),
            Marks::default(),
            None,
        );
        assert_eq!(cursor, (8, 2));

        // The caret clips at the right edge rather than leaving the screen.
        let cursor = draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "open: src/main.rs",
            Some(17),
            Marks::default(),
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
            Marks::default(),
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

    /// The tabs, each carrying the project mark named for it.
    fn tabs_marked(docs: Vec<Document>, marks: &[Option<Mark>]) -> Tabs {
        let mut tabs = Tabs::new(docs);
        for (index, mark) in marks.iter().enumerate() {
            tabs.get_mut(index).mark = *mark;
        }
        tabs
    }

    #[test]
    fn a_tab_label_carries_its_change_mark_beside_the_dirty_one() {
        let tabs = tabs_marked(
            vec![named("a.rs", ""), dirtied(named("b.rs", ""))],
            &[Some(Mark::New), Some(Mark::Changed)],
        );
        let (screen, _) = render_tabs(&tabs, 20, 3);
        assert_eq!(row(&screen, 0), " ●a.rs  ●b.rs+      ");
        // Green for the new file, yellow for the changed one, and the mark
        // is the only coloured cell in either label.
        assert_eq!(fg_row(&screen, 0), " 3      4           ");
        assert_eq!(sel_row(&screen, 0), "#######             ");
    }

    #[test]
    fn a_tab_label_marks_a_change_a_conflict_and_a_dirty_buffer_at_once() {
        let mut doc = dirtied(named("a.rs", ""));
        doc.set_conflict(true);
        let tabs = tabs_marked(vec![doc], &[Some(Mark::Changed)]);
        let (screen, _) = render_tabs(&tabs, 16, 3);
        assert_eq!(row(&screen, 0), " ●a.rs+!        ");
    }

    #[test]
    fn tab_at_follows_the_labels_the_marks_widened() {
        let tabs = tabs_marked(
            vec![named("a.rs", ""), dirtied(named("b.rs", ""))],
            &[Some(Mark::New), None],
        );
        // Drawn as " ●a.rs  b.rs+  ": the mark widens the first label to
        // 0..7, pushing the second to 7..14.
        assert_eq!(tab_at(&tabs, 16, 6), Some(0));
        assert_eq!(tab_at(&tabs, 16, 7), Some(1));
        assert_eq!(tab_at(&tabs, 16, 13), Some(1));
        assert_eq!(tab_at(&tabs, 16, 14), None);
    }

    #[test]
    fn overflow_keeps_a_marked_active_tab_whole() {
        let mut tabs = tabs_marked(
            vec![named("aa.rs", ""), named("bb.rs", ""), named("cc.rs", "")],
            &[None, Some(Mark::Changed), Some(Mark::New)],
        );
        tabs.activate(2);
        let (screen, _) = render_tabs(&tabs, 15, 3);
        // Both marked labels are 8 columns, so only the active one fits.
        assert_eq!(row(&screen, 0), " ●cc.rs        ");
        assert_eq!(tab_at(&tabs, 15, 6), Some(2));
        assert_eq!(tab_at(&tabs, 15, 8), None);
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
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks {
                search: Some(search),
                syntax: &[],
            },
            None,
        );
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

    /// The fg codes of a row as digits, space for default.
    fn fg_row(screen: &Screen, y: u16) -> String {
        let (width, _) = screen.size();
        (0..width)
            .map(|x| match screen.get(x, y).unwrap().fg() {
                0 => ' ',
                fg => char::from_digit(u32::from(fg), 16).unwrap(),
            })
            .collect()
    }

    fn render_syntax(text: &str, width: u16, height: u16, view: View, syntax: &[Span]) -> Screen {
        let tabs = tabs_of(Document::from_str(text), view);
        let mut screen = Screen::new(width, height);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks {
                search: None,
                syntax,
            },
            None,
        );
        screen
    }

    fn span(start: usize, end: usize, color: u8) -> Span {
        Span { start, end, color }
    }

    #[test]
    fn syntax_spans_colour_their_cells_and_nothing_else() {
        let spans = [span(0, 3, 3), span(4, 5, 5)];
        let screen = render_syntax("let x = 1;", 13, 3, View::default(), &spans);
        assert_eq!(row(&screen, 1), "1 let x = 1; ");
        assert_eq!(fg_row(&screen, 1), "  333 5      ");
    }

    #[test]
    fn syntax_spans_carry_across_lines_and_empty_is_plain() {
        // One span covering "ab\ncd" entirely: both lines colour.
        let spans = [span(0, 5, 2)];
        let screen = render_syntax("ab\ncd", 6, 4, View::default(), &spans);
        assert_eq!(fg_row(&screen, 1), "  22  ");
        assert_eq!(fg_row(&screen, 2), "  22  ");

        let plain = render_syntax("ab\ncd", 6, 4, View::default(), &[]);
        assert_eq!(fg_row(&plain, 1), "      ");
    }

    #[test]
    fn syntax_spans_clip_to_the_viewport_and_span_wide_glyphs() {
        // Columns 0-2 scrolled off: only "de" of the coloured "def" shows.
        let spans = [span(3, 6, 4)];
        let screen = render_syntax("abcdef", 4, 3, View::test_at(0, 0, 3), &spans);
        assert_eq!(fg_row(&screen, 1), "  44");

        // A wide glyph colours both of its columns.
        let spans = [span(1, 2, 6)];
        let screen = render_syntax("a日b", 8, 3, View::default(), &spans);
        assert_eq!(fg_row(&screen, 1), "   66   ");
    }

    #[test]
    fn stale_syntax_spans_past_the_text_colour_nothing() {
        let spans = [span(2, 3, 3), span(90, 500, 5)];
        let screen = render_syntax("abcd", 8, 3, View::default(), &spans);
        assert_eq!(fg_row(&screen, 1), "    3   ");
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

    /// A view of `buffer` against `head`, with its lookup already answered.
    fn comparing(head: &str, buffer: &str, rows: usize) -> (Document, Compare) {
        let doc = named("sample.rs", buffer);
        let head = Rope::from_str(head);
        let hunks = crate::diff::hunks(&head, doc.rope());
        let diff = Diff::test_baseline(Some(head), hunks);
        let compare = Compare::new(&doc, &diff, 0, rows);
        (doc, compare)
    }

    /// The same two panes, standing a dirty buffer against the file the
    /// conflict is about.
    fn resolving(disk: &str, buffer: &str, rows: usize) -> (Document, Compare) {
        let doc = named("sample.rs", buffer);
        let disk = crate::doc::Disk {
            text: Rope::from_str(disk),
            hash: 1,
            lossy: false,
        };
        let compare = Compare::disk(&doc, disk, 0, rows);
        (doc, compare)
    }

    fn render_compare(compare: &Compare, doc: &Document, width: u16, height: u16) -> Screen {
        let mut screen = Screen::new(width, height);
        let mut scratch = String::new();
        // Drawn over a full frame, the way the loop composes it.
        let tabs = tabs_of(Document::from_str("scratch"), View::default());
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        draw_compare(&mut screen, compare, doc, &mut scratch);
        screen
    }

    #[test]
    fn a_conflict_view_names_the_file_rather_than_head() {
        let (doc, compare) = resolving("a\nb\n", "a\nB\n", 6);
        let screen = render_compare(&compare, &doc, 30, 9);
        // The left pane is the file itself; the right is the buffer, and
        // the change between them reads exactly as a HEAD diff does.
        assert_eq!(row(&screen, 1), "disk          │sample.rs      ");
        assert_eq!(row(&screen, 2), "1  a          │1  a           ");
        assert_eq!(row(&screen, 3), "2 ▍b          │2 ▍B           ");
    }

    #[test]
    fn the_panes_stand_the_two_texts_level() {
        // One line rewritten and one inserted: the rewrite sits level, and
        // the insertion pads the side that does not have it.
        let (doc, compare) = comparing("a\nb\nc\n", "a\nB\nnew\nc\n", 6);
        let screen = render_compare(&compare, &doc, 30, 9);
        assert_eq!(row(&screen, 1), "HEAD          │sample.rs      ");
        assert_eq!(row(&screen, 2), "1  a          │1  a           ");
        assert_eq!(row(&screen, 3), "2 ▍b          │2 ▍B           ");
        assert_eq!(row(&screen, 4), "              │3 ▍new         ");
        assert_eq!(row(&screen, 5), "3  c          │4  c           ");
        // The empty line every rope carries past its last terminator, one
        // opposite the other.
        assert_eq!(row(&screen, 6), "4             │5              ");
        // Past the end of the shorter view, nothing at all.
        assert_eq!(row(&screen, 7), "              │               ");
    }

    #[test]
    fn the_left_pane_reads_red_and_the_right_green() {
        // The side carries the before-and-after, so a rewrite is red
        // against green rather than one "changed" colour on both.
        let (doc, compare) = comparing("a\nb\nc\n", "a\nB\nnew\nc\n", 6);
        let screen = render_compare(&compare, &doc, 30, 9);
        // Context rows carry no colour at all.
        assert_eq!(fg_row(&screen, 2), "                              ");
        // The bar and the text take it; the line numbers stay plain, as
        // the gutter's do.
        assert_eq!(fg_row(&screen, 3), "  22             33           ");
        assert_eq!(fg_row(&screen, 4), "                 3333         ");
        assert_eq!(fg_row(&screen, 5), "                              ");
    }

    #[test]
    fn a_deletion_leaves_the_buffer_side_blank() {
        let (doc, compare) = comparing("a\ngone\nb\n", "a\nb\n", 6);
        let screen = render_compare(&compare, &doc, 30, 9);
        assert_eq!(row(&screen, 3), "2 ▍gone       │               ");
        assert_eq!(fg_row(&screen, 3), "  22222                       ");
    }

    #[test]
    fn the_view_covers_the_body_and_spares_the_tab_bar_and_status_line() {
        let (doc, compare) = comparing("a\n", "a\nb\n", 6);
        let mut plain = Screen::new(30, 9);
        let mut scratch = String::new();
        let tabs = tabs_of(Document::from_str("scratch"), View::default());
        draw(
            &mut plain,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let screen = render_compare(&compare, &doc, 30, 9);
        assert_eq!(row(&screen, 0), row(&plain, 0));
        assert_eq!(row(&screen, 8), row(&plain, 8));
        // Nothing of the text area beneath shows through.
        assert!(!all_rows(&screen).contains("scratch"));
    }

    #[test]
    fn a_note_stands_in_for_the_panes_when_there_is_nothing_to_show() {
        let doc = named("sample.rs", "a\n");
        let compare = Compare::new(&doc, &Diff::new(None), 0, 6);
        let screen = render_compare(&compare, &doc, 40, 9);
        assert!(
            all_rows(&screen).contains(crate::compare::NOTHING),
            "{}",
            all_rows(&screen)
        );
        // No pane content behind the note.
        assert!(!all_rows(&screen).contains('▍'));
    }

    #[test]
    fn a_line_running_past_a_pane_reports_the_clip() {
        let wide = "x".repeat(200);
        let (doc, compare) = comparing("a\n", &format!("{wide}\n"), 6);
        let mut screen = Screen::new(30, 9);
        let mut scratch = String::new();
        assert!(draw_compare(&mut screen, &compare, &doc, &mut scratch));
        // Short lines on both sides have nothing past the edge.
        let (doc, compare) = comparing("a\n", "b\n", 6);
        let mut screen = Screen::new(30, 9);
        assert!(!draw_compare(&mut screen, &compare, &doc, &mut scratch));
    }

    #[test]
    fn the_panes_hold_their_columns_on_a_terminal_too_narrow_for_them() {
        // Gutters wider than their pane clip rather than crash, and the
        // divider keeps its column.
        let (doc, compare) = comparing("a\nb\n", "a\nB\n", 4);
        for width in [3, 6, 12] {
            let screen = render_compare(&compare, &doc, width, 8);
            let divider = usize::from((width - 1) / 2);
            for y in 1..6 {
                assert_eq!(
                    row(&screen, y).chars().nth(divider),
                    Some('│'),
                    "width {width} row {y}"
                );
            }
        }
    }

    fn all_rows(screen: &Screen) -> String {
        let (_, height) = screen.size();
        (0..height).map(|y| row(screen, y) + "\n").collect()
    }

    #[test]
    fn help_overlay_lists_every_binding_and_description() {
        let tabs = tabs_of(Document::from_str("text"), View::default());
        let mut screen = Screen::new(100, 60);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        draw_help(&mut screen, &mut scratch);
        let all = all_rows(&screen);
        assert!(all.contains(HELP_FOOTER));
        for section in keymap::KEYMAP {
            assert!(
                all.contains(section.title),
                "missing section {}",
                section.title
            );
            for binding in section.bindings {
                let mut label = String::new();
                binding.write_label(&mut label);
                assert!(all.contains(&label), "missing label {label}");
                assert!(
                    all.contains(binding.what),
                    "missing description {}",
                    binding.what
                );
            }
        }
    }

    #[test]
    fn help_overlay_boxes_the_text_area_and_spares_the_chrome() {
        // A selection reaching under the box must not bleed through it.
        // Lines long enough to run past the box's left edge, on a screen
        // wide enough to leave one — the box's own width is the keymap's
        // business and must not decide whether this test can see a bleed.
        let text = "some text that runs on and on\nmore text that runs on and on";
        let view = View::test_at(text.chars().count(), 0, 0).with_anchor(0);
        let tabs = tabs_of(Document::from_str(text), view);
        let mut scratch = String::new();
        let mut plain = Screen::new(100, 24);
        draw(
            &mut plain,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let mut screen = Screen::new(100, 24);
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        draw_help(&mut screen, &mut scratch);

        assert_eq!(row(&screen, 0), row(&plain, 0));
        assert_eq!(row(&screen, 23), row(&plain, 23));
        let all = all_rows(&screen);
        assert!(all.contains('┌') && all.contains('┘'));
        // Row 2 crosses the box interior: reversed cells left of the border
        // survive, everything under the box is cleared.
        let border = row(&screen, 1).chars().position(|c| c == '┌').unwrap();
        let sel = sel_row(&screen, 2);
        assert!(sel_row(&plain, 2)[border..].contains('#'));
        assert!(sel[..border].contains('#'));
        assert!(
            !sel[border..].contains('#'),
            "selection bleeds into the box: {sel}"
        );
    }

    fn picker_of(paths: &[&str], done: bool) -> Picker {
        let mut picker = Picker::new(PathBuf::from("/r"), 1, Default::default());
        picker.absorb(crate::project::FileBatch {
            generation: 1,
            paths: paths.iter().map(|s| s.to_string()).collect(),
            done,
        });
        picker
    }

    fn press(picker: &mut Picker, code: crossterm::event::KeyCode) {
        picker.key(&crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ));
    }

    const PICK_PATHS: &[&str] = &["src/main.rs", "src/draw.rs", "README.md"];

    #[test]
    fn picker_overlay_shows_query_count_and_ranked_results() {
        let tabs = tabs_of(Document::from_str("text"), View::default());
        let mut screen = Screen::new(40, 14);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let mut picker = picker_of(PICK_PATHS, false);
        press(&mut picker, crossterm::event::KeyCode::Char('d'));
        let cursor = draw_picker(&mut screen, &picker, &mut scratch);

        // Walk order first: 40 wide leaves no margin, so the box spans it.
        let query = row(&screen, 2);
        assert!(query.contains("> d"), "query missing: {query}");
        assert!(query.contains("2/3…"), "count missing: {query}");
        assert_eq!(cursor, (5, 2));
        let all = all_rows(&screen);
        assert!(all.contains("src/draw.rs"));
        assert!(all.contains("README.md"));
        assert!(!all.contains("src/main.rs"));
    }

    #[test]
    fn picker_overlay_reverses_the_selected_row_only() {
        let tabs = tabs_of(Document::from_str("text"), View::default());
        let mut screen = Screen::new(40, 14);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let mut picker = picker_of(PICK_PATHS, true);
        press(&mut picker, crossterm::event::KeyCode::Down);
        draw_picker(&mut screen, &picker, &mut scratch);

        assert!(row(&screen, 4).contains("src/draw.rs"));
        assert!(!sel_row(&screen, 3).contains('#'));
        assert!(sel_row(&screen, 4).contains('#'));
        assert!(!sel_row(&screen, 5).contains('#'));
    }

    #[test]
    fn picker_overlay_spares_the_chrome_and_clears_beneath() {
        // 100 wide: the box takes its 80 columns centered, leaving editor
        // content beside it. A selection reaching under it must not bleed.
        let view = View::test_at(19, 0, 0).with_anchor(0);
        let tabs = tabs_of(Document::from_str("some text\nmore text"), view);
        let mut scratch = String::new();
        let mut plain = Screen::new(100, 24);
        draw(
            &mut plain,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let mut screen = Screen::new(100, 24);
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let picker = picker_of(PICK_PATHS, true);
        draw_picker(&mut screen, &picker, &mut scratch);

        assert_eq!(row(&screen, 0), row(&plain, 0));
        assert_eq!(row(&screen, 23), row(&plain, 23));
        // Row 2 crosses the box's top border: reversed cells left of it
        // survive, everything under the box is cleared.
        let border = row(&screen, 2).chars().position(|c| c == '┌').unwrap();
        let sel = sel_row(&screen, 2);
        assert!(sel_row(&plain, 2)[border..].contains('#'));
        assert!(sel[..border].contains('#'));
        assert!(!sel[border..].contains('#'), "selection bleeds: {sel}");
    }

    #[test]
    fn picker_overlay_truncates_long_paths_to_their_tail() {
        let tabs = tabs_of(Document::from_str(""), View::default());
        let mut screen = Screen::new(20, 10);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let picker = picker_of(&["a/very/deep/nested/path/name.rs"], true);
        draw_picker(&mut screen, &picker, &mut scratch);

        let listing = row(&screen, 3);
        assert!(listing.contains('…'), "no ellipsis: {listing}");
        assert!(listing.contains("name.rs"), "tail lost: {listing}");
        assert!(!listing.contains("a/very"), "head kept: {listing}");
    }

    fn grep_of(files: &[(&str, &[(u32, &str)])], query: &str, done: bool) -> Grep {
        let mut grep = Grep::new(PathBuf::from("/r"));
        for ch in query.chars() {
            grep.key(
                &crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Char(ch),
                    crossterm::event::KeyModifiers::NONE,
                ),
                std::time::Instant::now(),
            );
        }
        grep.begin(1, Default::default());
        grep.absorb(crate::grep::HitBatch {
            generation: 1,
            files: files
                .iter()
                .map(|(path, hits)| crate::grep::FileHits {
                    path: path.to_string(),
                    hits: hits
                        .iter()
                        .map(|(line, text)| crate::grep::Hit {
                            line: *line,
                            col: 0,
                            preview: text.to_string(),
                        })
                        .collect(),
                })
                .collect(),
            done,
            truncated: false,
        });
        grep
    }

    const GREP_FILES: &[(&str, &[(u32, &str)])] = &[
        ("src/a.rs", &[(3, "let se = 1;"), (9, "se again")]),
        ("b.rs", &[(1, "sea")]),
    ];

    #[test]
    fn grep_overlay_shows_query_count_headers_and_hits() {
        let tabs = tabs_of(Document::from_str("text"), View::default());
        let mut screen = Screen::new(40, 14);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let grep = grep_of(GREP_FILES, "se", false);
        let cursor = draw_grep(&mut screen, &grep, &mut scratch);

        let query = row(&screen, 2);
        assert!(query.contains("> se"), "query missing: {query}");
        assert!(query.contains("3 hits…"), "count missing: {query}");
        assert_eq!(cursor, (6, 2));
        assert!(row(&screen, 3).contains("src/a.rs"));
        assert!(ul_row(&screen, 3).contains('_'), "header not underlined");
        assert!(row(&screen, 4).contains("3: let se = 1;"));
        assert!(row(&screen, 5).contains("9: se again"));
        assert!(row(&screen, 6).contains("b.rs"));
        assert!(row(&screen, 7).contains("1: sea"));
        assert!(!ul_row(&screen, 4).contains('_'), "hit row underlined");
    }

    #[test]
    fn grep_overlay_marks_a_capped_finished_search() {
        let tabs = tabs_of(Document::from_str("text"), View::default());
        let mut screen = Screen::new(40, 14);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let mut grep = grep_of(GREP_FILES, "se", false);
        grep.absorb(crate::grep::HitBatch {
            generation: 1,
            files: Vec::new(),
            done: true,
            truncated: true,
        });
        draw_grep(&mut screen, &grep, &mut scratch);
        let query = row(&screen, 2);
        assert!(query.contains("3+ hits"), "cap missing: {query}");
        assert!(!query.contains('…'), "finished search still streaming");
    }

    #[test]
    fn grep_overlay_reverses_only_the_selected_hit_row() {
        let tabs = tabs_of(Document::from_str("text"), View::default());
        let mut screen = Screen::new(40, 14);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let mut grep = grep_of(GREP_FILES, "se", true);
        grep.key(
            &crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Down,
                crossterm::event::KeyModifiers::NONE,
            ),
            std::time::Instant::now(),
        );
        draw_grep(&mut screen, &grep, &mut scratch);

        // The second hit sits on row 5; headers never take the bar.
        assert!(!sel_row(&screen, 3).contains('#'));
        assert!(!sel_row(&screen, 4).contains('#'));
        assert!(sel_row(&screen, 5).contains('#'));
        assert!(!sel_row(&screen, 6).contains('#'));
    }

    #[test]
    fn grep_overlay_truncates_paths_to_their_tail_and_previews_to_their_head() {
        let tabs = tabs_of(Document::from_str(""), View::default());
        let mut screen = Screen::new(20, 10);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let files: &[(&str, &[(u32, &str)])] = &[(
            "a/very/deep/nested/path/name.rs",
            &[(12, "a preview far too long to fit")],
        )];
        let grep = grep_of(files, "q", true);
        draw_grep(&mut screen, &grep, &mut scratch);

        let header = row(&screen, 3);
        assert!(header.contains('…'), "no ellipsis: {header}");
        assert!(header.contains("name.rs"), "tail lost: {header}");
        let hit = row(&screen, 4);
        assert!(hit.contains("12: a preview"), "head lost: {hit}");
        assert!(hit.contains('…'), "no ellipsis: {hit}");
        assert!(!hit.contains("fit"), "tail kept: {hit}");
    }

    fn tree_of(paths: &[&str], done: bool) -> Tree {
        let mut tree = Tree::new(PathBuf::from("/r"), 1, Default::default());
        tree.absorb(
            crate::project::FileBatch {
                generation: 1,
                paths: paths.iter().map(|s| s.to_string()).collect(),
                done,
            },
            10,
        );
        tree
    }

    const TREE_PATHS: &[&str] = &["src/main.rs", "README.md"];

    #[test]
    fn tree_sidebar_lists_entries_and_shifts_the_editor_right() {
        let tabs = tabs_of(Document::from_str("hello"), View::default());
        let mut screen = Screen::new(60, 5);
        let mut scratch = String::new();
        let tree = tree_of(TREE_PATHS, true);
        let cursor = draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: false,
                status: &Status::test_plain(),
            }),
        );

        let body = row(&screen, 1);
        assert!(body.starts_with("▸ src"), "no dir glyph: {body}");
        assert_eq!(body.chars().nth(29), Some('│'), "no border: {body}");
        assert_eq!(body.chars().position(|c| c == '1'), Some(30));
        assert!(body.contains("1 hello"), "text not shifted: {body}");
        assert!(row(&screen, 2).starts_with("  README.md"));
        // Unfocused, the cursor stays in the text area, past the sidebar.
        assert_eq!(cursor, (32, 1));
    }

    #[test]
    fn a_focused_tree_reverses_its_selection_and_the_cursor_stays_put() {
        let tabs = tabs_of(Document::from_str("hello"), View::default());
        let mut scratch = String::new();
        let tree = tree_of(TREE_PATHS, true);

        let mut unfocused = Screen::new(60, 5);
        draw(
            &mut unfocused,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: false,
                status: &Status::test_plain(),
            }),
        );
        assert!(!sel_row(&unfocused, 1).contains('#'));

        let mut focused = Screen::new(60, 5);
        let cursor = draw(
            &mut focused,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: true,
                status: &Status::test_plain(),
            }),
        );
        let sel = sel_row(&focused, 1);
        assert!(sel[..29].contains('#'), "selection not reversed: {sel}");
        assert!(!sel_row(&focused, 2).contains('#'));
        // The selection bar is the focus cue; the returned cursor stays in
        // the text area, and the caller hides it while the tree is focused.
        assert_eq!(cursor, (32, 1));
    }

    #[test]
    fn the_edited_file_is_underlined_in_the_tree() {
        let tabs = tabs_of(Document::from_str("hello"), View::default());
        let mut screen = Screen::new(60, 5);
        let mut scratch = String::new();
        let mut tree = tree_of(TREE_PATHS, true);
        tree.set_active(Some(std::path::Path::new("/r/README.md")));
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: false,
                status: &Status::test_plain(),
            }),
        );
        assert!(!ul_row(&screen, 1).contains('_'));
        assert!(ul_row(&screen, 2).contains('_'));
    }

    /// The sidebar drawn over a project standing at fixed marks.
    fn render_tree(tree: &Tree, status: &Status, width: u16) -> Screen {
        let tabs = tabs_of(Document::from_str("hello"), View::default());
        let mut screen = Screen::new(width, 6);
        let mut scratch = String::new();
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree,
                focused: false,
                status,
            }),
        );
        screen
    }

    #[test]
    fn the_sidebar_marks_changed_and_new_rows_in_their_own_column() {
        let mut tree = tree_of(&["src/main.rs", "src/new.rs", "README.md"], true);
        tree.key(
            &crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Right,
                crossterm::event::KeyModifiers::NONE,
            ),
            10,
        );
        let status = Status::test_marks(vec![
            ("src", Mark::Changed),
            ("src/main.rs", Mark::Changed),
            ("src/new.rs", Mark::New),
        ]);
        let screen = render_tree(&tree, &status, 60);
        // The mark holds the last inner column, clear of the border.
        assert_eq!(
            row(&screen, 1).trim_end(),
            "▾ src                       ●│1 hello"
        );
        assert_eq!(row(&screen, 2).trim_end(), "    main.rs                 ●│");
        assert_eq!(row(&screen, 3).trim_end(), "    new.rs                  ●│");
        assert_eq!(row(&screen, 4).trim_end(), "  README.md                  │");
        assert_eq!(
            fg_row(&screen, 2).trim_end(),
            "                            4"
        );
        assert_eq!(
            fg_row(&screen, 3).trim_end(),
            "                            3"
        );
        // An unmarked row spends nothing in the column it still reserves.
        assert_eq!(fg_row(&screen, 4).trim_end(), "");
    }

    #[test]
    fn a_collapsed_directory_carries_the_change_beneath_it() {
        let tree = tree_of(&["src/deep/new.rs"], true);
        let status = Status::test_marks(vec![
            ("src", Mark::New),
            ("src/deep", Mark::New),
            ("src/deep/new.rs", Mark::New),
        ]);
        let screen = render_tree(&tree, &status, 60);
        // `src` is collapsed: without the mark the change under it would be
        // invisible.
        assert_eq!(
            row(&screen, 1).trim_end(),
            "▸ src                       ●│1 hello"
        );
        assert_eq!(
            fg_row(&screen, 1).trim_end(),
            "                            3"
        );
    }

    #[test]
    fn the_sidebar_mark_column_appears_only_inside_a_repository() {
        let tree = tree_of(TREE_PATHS, true);
        let plain = render_tree(&tree, &Status::test_plain(), 60);
        // Outside a repository the sidebar is what it always was, to the
        // column: the mark column is not reserved at all.
        assert_eq!(
            row(&plain, 1).trim_end(),
            "▸ src                        │1 hello"
        );
        assert_eq!(row(&plain, 2).trim_end(), "  README.md                  │");
        assert_eq!(fg_row(&plain, 1).trim_end(), "");
    }

    #[test]
    fn a_reserved_mark_column_truncates_a_long_name_one_column_earlier() {
        let paths = &["a_name_far_too_long_for_the_sidebar.rs"];
        let plain = render_tree(&tree_of(paths, true), &Status::test_plain(), 60);
        let marked = render_tree(&tree_of(paths, true), &Status::test_marks(Vec::new()), 60);
        assert_eq!(
            row(&plain, 1).trim_end(),
            "  a_name_far_too_long_for_th…│1 hello"
        );
        assert_eq!(
            row(&marked, 1).trim_end(),
            "  a_name_far_too_long_for_t… │1 hello"
        );
    }

    #[test]
    fn tree_sidebar_spares_the_tab_bar_and_status_line() {
        let tabs = tabs_of(named("f.rs", "hello"), View::default());
        let mut scratch = String::new();
        let mut plain = Screen::new(60, 5);
        draw(
            &mut plain,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let mut screen = Screen::new(60, 5);
        let tree = tree_of(TREE_PATHS, true);
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: false,
                status: &Status::test_plain(),
            }),
        );
        assert_eq!(row(&screen, 0), row(&plain, 0));
        assert_eq!(row(&screen, 4), row(&plain, 4));
    }

    #[test]
    fn a_narrow_terminal_hides_the_sidebar() {
        assert_eq!(tree_width(true, 44), 0);
        assert_eq!(tree_width(true, 45), 15);
        assert_eq!(tree_width(true, 80), 30);
        assert_eq!(tree_width(false, 80), 0);

        let tabs = tabs_of(Document::from_str("hello"), View::default());
        let mut scratch = String::new();
        let mut plain = Screen::new(44, 5);
        draw(
            &mut plain,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            None,
        );
        let mut screen = Screen::new(44, 5);
        let tree = tree_of(TREE_PATHS, true);
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: false,
                status: &Status::test_plain(),
            }),
        );
        for y in 0..5 {
            assert_eq!(row(&screen, y), row(&plain, y));
        }
    }

    #[test]
    fn the_tree_shows_an_ellipsis_while_the_walk_streams() {
        let tabs = tabs_of(Document::from_str(""), View::default());
        let mut screen = Screen::new(60, 6);
        let mut scratch = String::new();
        let tree = tree_of(&["a.rs"], false);
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: false,
                status: &Status::test_plain(),
            }),
        );
        assert!(row(&screen, 1).starts_with("  a.rs"));
        assert!(row(&screen, 2).starts_with('…'));
    }

    #[test]
    fn tree_names_truncate_keeping_their_head() {
        let tabs = tabs_of(Document::from_str(""), View::default());
        let mut screen = Screen::new(60, 5);
        let mut scratch = String::new();
        let tree = tree_of(&["a_very_long_file_name_that_overflows.rs"], true);
        draw(
            &mut screen,
            &tabs,
            &mut scratch,
            "",
            None,
            Marks::default(),
            Some(Sidebar {
                tree: &tree,
                focused: false,
                status: &Status::test_plain(),
            }),
        );
        let body = row(&screen, 1);
        assert!(body.contains("a_very_long"), "head lost: {body}");
        assert!(body.contains('…'), "no ellipsis: {body}");
        assert!(!body.contains(".rs"), "tail kept: {body}");
        assert_eq!(body.chars().nth(29), Some('│'), "border overrun: {body}");
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [
            (0, 0),
            (0, 5),
            (5, 0),
            (1, 1),
            (2, 2),
            (3, 1),
            (1, 3),
            (10, 5),
            (24, 8),
            (80, 24),
        ] {
            let (mut screen, _) = render("日本\ntext", w, h, View::default());
            let mut scratch = String::new();
            draw_help(&mut screen, &mut scratch);
            draw_picker(&mut screen, &picker_of(&[], false), &mut scratch);
            let mut full = picker_of(&["日本/長いファイル名.rs", "b.rs"], true);
            press(&mut full, crossterm::event::KeyCode::Down);
            draw_picker(&mut screen, &full, &mut scratch);
            draw_grep(&mut screen, &grep_of(&[], "", false), &mut scratch);
            let wide: &[(&str, &[(u32, &str)])] = &[
                ("日本/長いファイル名.rs", &[(1, "日本語のプレビュー")]),
                ("b.rs", &[(2, "x")]),
            ];
            let mut gfull = grep_of(wide, "日本", true);
            gfull.key(
                &crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Down,
                    crossterm::event::KeyModifiers::NONE,
                ),
                std::time::Instant::now(),
            );
            draw_grep(&mut screen, &gfull, &mut scratch);
            let mut tabs = Tabs::new(vec![
                named("aa.rs", "日本"),
                dirtied(named("bb.rs", "text")),
                named("a_very_long_file_name.rs", ""),
            ]);
            tabs.activate(2);
            render_tabs(&tabs, w, h);

            let mut deep = tree_of(
                &["日本/長いファイル名.rs", "a/b/c/d/e/f/g/h/i/deep.rs"],
                false,
            );
            deep.set_active(Some(std::path::Path::new("/r/a/b/c/d/e/f/g/h/i/deep.rs")));
            deep.reveal_active(10);
            for focused in [false, true] {
                let mut screen = Screen::new(w, h);
                draw(
                    &mut screen,
                    &tabs,
                    &mut scratch,
                    "",
                    None,
                    Marks::default(),
                    Some(Sidebar {
                        tree: &deep,
                        focused,
                        status: &Status::test_plain(),
                    }),
                );
            }

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
