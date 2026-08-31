mod clip;
mod diff;
mod doc;
mod draw;
mod grapheme;
mod grep;
mod journal;
mod keymap;
mod picker;
mod project;
mod prompt;
mod screen;
mod search;
mod syntax;
mod tabs;
mod term;
mod tree;
mod view;
mod watch;

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};

use doc::{Caret, DiskCheck, Document};
use grep::Grep;
use journal::{Journal, Recovered};
use keymap::Action;
use picker::Picker;
use prompt::{LinePrompt, Outcome, PathPrompt};
use screen::Screen;
use search::SearchPrompt;
use tabs::{Tab, Tabs};
use term::Terminal;
use tree::Tree;
use watch::{AppEvent, Debounce, DirWatcher, Refresh, TreeWatcher};

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let _ = args.next();
    // Open before touching the terminal so errors print on the normal screen.
    let mut docs = Vec::new();
    for arg in args {
        let path = PathBuf::from(arg);
        match Document::open(path.clone()) {
            Ok(doc) => docs.push(doc),
            Err(e) => {
                eprintln!("connor: {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        }
    }
    let (mut journal, recovered, startup_notice) = Journal::start();
    let notice = merge_recovered(&mut docs, recovered, startup_notice);
    if docs.is_empty() {
        docs.push(Document::empty());
    }
    let mut tabs = Tabs::new(docs);
    let result = run(&mut tabs, &mut journal, notice);
    // Only a user quit deletes the session dir; a terminal error or a
    // termination signal keeps it (after a last snapshot) so unsaved work
    // survives to the next start.
    let clean = matches!(result, Ok(Exit::Quit));
    if !clean {
        journal.flush(&tabs);
    }
    journal.finish(clean);
    match result {
        Ok(Exit::Quit) => ExitCode::SUCCESS,
        #[cfg(unix)]
        Ok(Exit::Signal(sig)) => {
            // Re-raise with default disposition: the shell sees signal
            // death (143 for TERM), not a made-up exit code.
            let _ = signal_hook::low_level::emulate_default_handler(sig);
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("connor: {e}");
            ExitCode::FAILURE
        }
    }
}

/// How the session ended: a user quit, or a termination signal that `main`
/// re-raises after cleanup so the process dies with signal status.
enum Exit {
    Quit,
    #[cfg(unix)]
    Signal(i32),
}

/// Folds crash-recovered buffers into the docs the args opened: an entry
/// for an already-open path splices into that document, any other becomes
/// its own tab. Returns the notice the first frame shows.
fn merge_recovered(
    docs: &mut Vec<Document>,
    recovered: Vec<Recovered>,
    startup_notice: Option<String>,
) -> String {
    let mut count = 0;
    for entry in recovered {
        match entry.path {
            Some(path) => {
                let target = tabs::canonical(&path);
                let existing = docs
                    .iter_mut()
                    .find(|doc| doc.path().is_some_and(|p| tabs::canonical(p) == target));
                match existing {
                    Some(doc) => count += usize::from(doc.restore_journal(&entry.text)),
                    None => {
                        // An unopenable path (permissions, say) must not
                        // cost the content: it lives on as a pathless
                        // buffer instead.
                        let mut doc = Document::open(path).unwrap_or_else(|_| Document::empty());
                        if doc.restore_journal(&entry.text) {
                            docs.push(doc);
                            count += 1;
                        }
                    }
                }
            }
            None => {
                let mut doc = Document::empty();
                if doc.restore_journal(&entry.text) {
                    docs.push(doc);
                    count += 1;
                }
            }
        }
    }
    match (count, startup_notice) {
        (0, Some(notice)) => notice,
        (0, None) => String::new(),
        (n, _) => format!(
            "recovered {n} unsaved buffer{}",
            if n == 1 { "" } else { "s" }
        ),
    }
}

/// Two presses on the same cell within this window select the word.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// Lines the viewport moves per wheel notch.
const WHEEL_LINES: usize = 3;

/// What a prompt-gated save was clearing the way for.
#[derive(Clone, Copy)]
enum AfterSave {
    Stay,
    Quit,
    Close,
}

/// What a path typed into the prompt is for.
enum PathAction {
    Open,
    SaveAs { then: AfterSave },
}

/// A mini-prompt owning the next keypress; its question sits in the notice.
enum Prompt {
    /// Ctrl+Q with unsaved changes in any tab: save all / discard / cancel.
    Quit,
    /// Ctrl+W on a dirty tab: save / discard / cancel.
    Close,
    /// The file loaded lossily, so writing it back mangles the bytes the
    /// U+FFFD marks stand for — overwriting must be deliberate.
    LossySave { then: AfterSave },
    /// A path being typed, for opening a file or naming a buffer.
    Path {
        edit: PathPrompt,
        action: PathAction,
    },
    /// Ctrl+G: a line number being typed.
    GoTo(LinePrompt),
    /// Ctrl+F: incremental search.
    Search(SearchPrompt),
    /// Ctrl+/ or F1: the keymap overlay.
    Help,
    /// Ctrl+P: the fuzzy file picker over the project.
    Pick(Picker),
    /// Alt+F: project-wide search.
    Grep(Grep),
}

fn open_prompt(prompt: Prompt, doc: &Document, notice: &mut String) -> Option<Prompt> {
    notice.clear();
    match &prompt {
        Prompt::Quit => {
            notice.push_str("unsaved changes — save before quitting? (y)es · (n)o · (esc) cancel");
        }
        Prompt::Close => {
            notice.push_str("unsaved changes — save before closing? (y)es · (n)o · (esc) cancel");
        }
        Prompt::LossySave { .. } => {
            let _ = write!(
                notice,
                "invalid UTF-8 was replaced on load — overwrite {}? (y)es · (esc) cancel",
                doc.name()
            );
        }
        Prompt::Path { edit, .. } => edit.render(notice),
        Prompt::GoTo(edit) => edit.render(notice),
        Prompt::Search(edit) => edit.render(notice),
        Prompt::Help => notice.push_str("(esc) close"),
        Prompt::Pick(_) => notice.push_str("↑↓ select · enter open · esc"),
        Prompt::Grep(_) => notice.push_str("↑↓ select · enter jump · esc"),
    }
    Some(prompt)
}

fn save_as_prompt(then: AfterSave) -> Prompt {
    Prompt::Path {
        edit: PathPrompt::new("save as: "),
        action: PathAction::SaveAs { then },
    }
}

fn try_save(doc: &mut Document, notice: &mut String) -> bool {
    notice.clear();
    match doc.save() {
        Ok(()) => {
            let _ = write!(notice, "saved {}", doc.name());
            true
        }
        Err(e) => {
            let _ = write!(notice, "save failed: {e}");
            false
        }
    }
}

/// Pastes into a pending prompt's field, flattened to one line. The
/// confirmation prompts ignore it: bracketed paste guarantees pasted text
/// arrives as a paste rather than keystrokes, so a pasted "y" can never
/// confirm a destructive prompt.
fn prompt_paste(prompt: &mut Prompt, text: &str, tabs: &mut Tabs, notice: &mut String) {
    match prompt {
        Prompt::Search(edit) => {
            let Tab { doc, view, .. } = tabs.active_mut();
            edit.paste(text, doc, view);
            edit.render(notice);
        }
        Prompt::GoTo(edit) => {
            edit.paste(text);
            edit.render(notice);
        }
        Prompt::Path { edit, .. } => {
            edit.paste(text);
            edit.render(notice);
        }
        // The query lives in the overlay, so the notice stays untouched.
        Prompt::Pick(edit) => edit.paste(text),
        Prompt::Grep(edit) => edit.paste(text, Instant::now()),
        Prompt::Quit | Prompt::Close | Prompt::LossySave { .. } | Prompt::Help => {}
    }
}

/// Feeds one keypress to a pending prompt. Returns the prompt still pending
/// (unrecognized keys leave it up) and whether the editor should quit.
fn prompt_key(
    prompt: Prompt,
    key: &KeyEvent,
    tabs: &mut Tabs,
    register: &str,
    notice: &mut String,
) -> (Option<Prompt>, bool) {
    // The help overlay owns only its closing keys: Esc or a repeat of an
    // opening chord dismisses it, anything else leaves it up.
    if let Prompt::Help = prompt {
        return if key.code == KeyCode::Esc || keymap::lookup(key) == Some(Action::Help) {
            notice.clear();
            (None, false)
        } else {
            (Some(Prompt::Help), false)
        };
    }
    // Ctrl+V pastes the register into the prompt's field; the prompts' own
    // key handlers never see modified chords.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('v') {
        let mut prompt = prompt;
        if !register.is_empty() {
            prompt_paste(&mut prompt, register, tabs, notice);
        }
        return (Some(prompt), false);
    }
    // The picker consumes every key itself; a repeat of its opening chord
    // dismisses it, like the help overlay.
    if let Prompt::Pick(mut edit) = prompt {
        if keymap::lookup(key) == Some(Action::PickFile) {
            edit.dismiss();
            notice.clear();
            return (None, false);
        }
        return match edit.key(key) {
            picker::Outcome::Pending => (Some(Prompt::Pick(edit)), false),
            picker::Outcome::Cancel => {
                notice.clear();
                (None, false)
            }
            picker::Outcome::Open(path) => {
                notice.clear();
                open_path(tabs, path, notice);
                (None, false)
            }
        };
    }
    // The project-search overlay likewise consumes every key; a repeat of
    // its opening chord dismisses it.
    if let Prompt::Grep(mut edit) = prompt {
        if keymap::lookup(key) == Some(Action::FindProject) {
            edit.dismiss();
            notice.clear();
            return (None, false);
        }
        return match edit.key(key, Instant::now()) {
            grep::Outcome::Pending => (Some(Prompt::Grep(edit)), false),
            grep::Outcome::Cancel => {
                notice.clear();
                (None, false)
            }
            grep::Outcome::Open { path, line, col } => {
                notice.clear();
                if open_path(tabs, path, notice) {
                    jump_to_hit(tabs, line, col);
                }
                (None, false)
            }
        };
    }
    // A search prompt consumes every key itself, and may move the cursor
    // and edit the document.
    if let Prompt::Search(mut edit) = prompt {
        let Tab { doc, view, .. } = tabs.active_mut();
        return match edit.key(key, doc, view) {
            search::Outcome::Pending => {
                edit.render(notice);
                (Some(Prompt::Search(edit)), false)
            }
            search::Outcome::Accept | search::Outcome::Cancel => {
                notice.clear();
                (None, false)
            }
            search::Outcome::ReplacedAll(n) => {
                notice.clear();
                let _ = write!(notice, "replaced {n}");
                (None, false)
            }
        };
    }
    // A go-to-line prompt consumes every key itself.
    if let Prompt::GoTo(mut edit) = prompt {
        return match edit.key(key) {
            Outcome::Pending => {
                edit.render(notice);
                (Some(Prompt::GoTo(edit)), false)
            }
            Outcome::Cancel => {
                notice.clear();
                (None, false)
            }
            Outcome::Submit => {
                notice.clear();
                if let Some(line) = edit.line() {
                    let Tab { doc, view, .. } = tabs.active_mut();
                    doc.break_undo_group();
                    view.begin_or_clear_selection(false);
                    view.move_to_line(doc, line);
                }
                (None, false)
            }
        };
    }
    // A path prompt consumes every key itself.
    if let Prompt::Path { mut edit, action } = prompt {
        return match edit.key(key) {
            Outcome::Pending => {
                edit.render(notice);
                (Some(Prompt::Path { edit, action }), false)
            }
            Outcome::Cancel => {
                notice.clear();
                (None, false)
            }
            Outcome::Submit => {
                notice.clear();
                let path = edit.into_path();
                if path.as_os_str().is_empty() {
                    return (None, false);
                }
                match action {
                    PathAction::Open => {
                        open_path(tabs, path, notice);
                        (None, false)
                    }
                    PathAction::SaveAs { then } => save_as(tabs, path, then, notice),
                }
            }
        };
    }
    match (prompt, key.code) {
        (Prompt::Quit, KeyCode::Char('y' | 'Y')) => quit_saving(tabs, notice),
        (Prompt::Quit, KeyCode::Char('n' | 'N')) => (None, true),
        (Prompt::Close, KeyCode::Char('y' | 'Y')) => {
            let doc = &mut tabs.active_mut().doc;
            if doc.path().is_none() {
                (
                    open_prompt(save_as_prompt(AfterSave::Close), doc, notice),
                    false,
                )
            } else if doc.lossy() {
                (
                    open_prompt(
                        Prompt::LossySave {
                            then: AfterSave::Close,
                        },
                        doc,
                        notice,
                    ),
                    false,
                )
            } else if try_save(doc, notice) {
                (None, close_active_or_quit(tabs))
            } else {
                // A failed save cancels the close: the error stays visible
                // and the changes stay alive.
                (None, false)
            }
        }
        (Prompt::Close, KeyCode::Char('n' | 'N')) => (None, close_active_or_quit(tabs)),
        (Prompt::LossySave { then }, KeyCode::Char('y' | 'Y')) => {
            if !try_save(&mut tabs.active_mut().doc, notice) {
                return (None, false);
            }
            match then {
                AfterSave::Stay => (None, false),
                AfterSave::Quit => quit_saving(tabs, notice),
                AfterSave::Close => (None, close_active_or_quit(tabs)),
            }
        }
        (_, KeyCode::Esc) => {
            notice.clear();
            (None, false)
        }
        (prompt, _) => (Some(prompt), false),
    }
}

/// Saves every dirty tab on the way out. A lossy one is activated and gets
/// its own confirmation, which re-enters here afterwards; a failed save
/// activates the failing tab and cancels the quit — the error stays
/// visible and the changes stay alive.
fn quit_saving(tabs: &mut Tabs, notice: &mut String) -> (Option<Prompt>, bool) {
    for i in 0..tabs.count() {
        let doc = &mut tabs.get_mut(i).doc;
        if !doc.dirty() {
            continue;
        }
        if doc.path().is_none() {
            tabs.activate(i);
            return (
                open_prompt(
                    save_as_prompt(AfterSave::Quit),
                    &tabs.active_mut().doc,
                    notice,
                ),
                false,
            );
        }
        if doc.lossy() {
            tabs.activate(i);
            return (
                open_prompt(
                    Prompt::LossySave {
                        then: AfterSave::Quit,
                    },
                    &tabs.active_mut().doc,
                    notice,
                ),
                false,
            );
        }
        if !try_save(doc, notice) {
            tabs.activate(i);
            return (None, false);
        }
    }
    (None, true)
}

/// Opens `path` in a new tab — or, if a tab already holds it, activates
/// that tab instead of opening the file twice. False means the active tab
/// never changed: the file failed to open.
fn open_path(tabs: &mut Tabs, path: PathBuf, notice: &mut String) -> bool {
    if let Some(index) = tabs.find_by_path(&path) {
        tabs.activate(index);
        return true;
    }
    match Document::open(path) {
        Ok(doc) => {
            tabs.active_mut().doc.break_undo_group();
            tabs.push(doc);
            true
        }
        Err(e) => {
            let _ = write!(notice, "open failed: {e}");
            false
        }
    }
}

/// Lands the caret on a project-search hit: 1-based `line`, char `col`,
/// both clamped — the file may have changed since the search read it.
fn jump_to_hit(tabs: &mut Tabs, line: usize, col: usize) {
    let Tab { doc, view, .. } = tabs.active_mut();
    doc.break_undo_group();
    view.begin_or_clear_selection(false);
    let line = line.clamp(1, doc.line_count()) - 1;
    let pos = (doc.line_start(line) + col).min(doc.line_end(line));
    view.set_caret(Caret {
        cursor: grapheme::snap_to_boundary(doc.rope().slice(..), pos),
        anchor: None,
    });
}

/// Names the buffer and saves it there, then carries on with whatever the
/// save was clearing the way for.
fn save_as(
    tabs: &mut Tabs,
    path: PathBuf,
    then: AfterSave,
    notice: &mut String,
) -> (Option<Prompt>, bool) {
    let tab = tabs.active_mut();
    tab.doc.set_path(path);
    // The new name can gain or lose a grammar and a repository; rebuild
    // the highlighter and the comparison against HEAD.
    tab.syntax = syntax::Syntax::new(&mut tab.doc);
    tab.diff = diff::Diff::new(tab.doc.path());
    let doc = &mut tab.doc;
    if !try_save(doc, notice) {
        // The name sticks, so a plain Ctrl+S can retry the same target.
        return (None, false);
    }
    match then {
        AfterSave::Stay => (None, false),
        AfterSave::Quit => quit_saving(tabs, notice),
        AfterSave::Close => (None, close_active_or_quit(tabs)),
    }
}

/// Forwards a fresh copy to the system clipboard via OSC 52 and says what
/// happened; an oversized copy stays register-only.
fn share_copy(
    terminal: &mut Terminal,
    register: &str,
    in_tmux: bool,
    notice: &mut String,
) -> io::Result<()> {
    match clip::osc52(register, in_tmux) {
        Some(seq) => {
            terminal.write_raw(&seq)?;
            notice.push_str("copied");
        }
        None => notice.push_str("copied (too large for the system clipboard)"),
    }
    Ok(())
}

/// Routes a walker batch to whichever consumer's walk produced it — the
/// picker and the tree can both have one in flight, told apart by
/// generation. Returns whether anything on screen changed; a chunk from a
/// dead walk changes nothing.
fn absorb_files(
    prompt: &mut Option<Prompt>,
    tree: &mut Option<Tree>,
    batch: project::FileBatch,
    text_h: usize,
) -> bool {
    match prompt {
        Some(Prompt::Pick(edit)) if edit.generation() == batch.generation => edit.absorb(batch),
        _ => tree.as_mut().is_some_and(|t| t.absorb(batch, text_h)),
    }
}

/// Routes a hit batch to the project-search overlay. Returns whether
/// anything on screen changed; a chunk from a dead search changes nothing.
fn absorb_hits(prompt: &mut Option<Prompt>, batch: grep::HitBatch) -> bool {
    match prompt {
        Some(Prompt::Grep(edit)) => edit.absorb(batch),
        _ => false,
    }
}

/// Routes a finished background parse to the tab still waiting on it.
/// Returns whether the screen changed — only when the parse is current and
/// its tab is the active one; a closed tab's or superseded result is
/// dropped.
fn absorb_parse(tabs: &mut Tabs, done: syntax::ParseDone, tx: &mpsc::Sender<AppEvent>) -> bool {
    let active = tabs.active_index();
    for index in 0..tabs.count() {
        let Tab { doc, syntax, .. } = tabs.get_mut(index);
        if doc.id() == done.doc_id {
            let changed = syntax.as_mut().is_some_and(|s| s.absorb(done, doc, tx));
            return changed && index == active;
        }
    }
    false
}

/// Routes a finished HEAD lookup or background diff to the tab waiting on
/// it. Returns whether the screen changed — only when the result is current
/// and its tab is the active one; a closed tab's or a superseded result is
/// dropped.
fn absorb_diff(tabs: &mut Tabs, done: diff::DiffDone, tx: &mpsc::Sender<AppEvent>) -> bool {
    let active = tabs.active_index();
    for index in 0..tabs.count() {
        let Tab { doc, diff, .. } = tabs.get_mut(index);
        if doc.id() == done.doc_id {
            let changed = diff.absorb(done, doc, tx);
            return changed && index == active;
        }
    }
    false
}

/// Closing the last tab is quitting; returns whether to quit.
fn close_active_or_quit(tabs: &mut Tabs) -> bool {
    if tabs.count() == 1 {
        return true;
    }
    tabs.close_active();
    false
}

fn run(tabs: &mut Tabs, journal: &mut Journal, notice: String) -> io::Result<Exit> {
    term::init_panic_hook();
    let mut terminal = Terminal::new()?;
    let (width, height) = terminal.size();
    let mut back = Screen::new(width, height);
    let mut scratch = String::new();
    // Startup already has something to say (a recovery, a disabled
    // journal); the watcher complaint below yields to it.
    let mut notice = notice;
    let mut prompt: Option<Prompt> = None;
    let (tx, rx) = mpsc::channel();
    watch::spawn_input_thread(tx.clone());
    // Losing signal handling costs graceful suspend and terminate, not the
    // editor.
    #[cfg(unix)]
    if let Err(e) = watch::spawn_signal_thread(tx.clone())
        && notice.is_empty()
    {
        let _ = write!(notice, "signal handling unavailable: {e}");
    }
    // Losing the watcher (inotify limits, say) costs reloads, not the
    // editor.
    let mut watcher = match DirWatcher::new(tx.clone()) {
        Ok(watcher) => Some(watcher),
        Err(e) => {
            if notice.is_empty() {
                let _ = write!(notice, "file watching unavailable: {e}");
            }
            None
        }
    };
    let mut debounce = Debounce::default();
    let mut last_click: Option<(Instant, u16, u16)> = None;
    // The internal clipboard: keeps the full text even when a copy is too
    // large for OSC 52, and works in terminals that ignore OSC 52 entirely.
    let mut register = String::new();
    let in_tmux = std::env::var_os("TMUX").is_some();
    let root = project::root();
    // Ties walk batches to the picker or tree they were started for; a
    // reopened consumer must never absorb a dead walk's leftovers.
    let mut walk_gen: u64 = 0;
    let mut tree: Option<Tree> = None;
    let mut tree_focus = false;
    let mut tree_watch: Option<TreeWatcher> = None;
    let mut refresh = Refresh::default();
    let mut head_refresh = Refresh::default();
    let mut redraw = true;

    loop {
        let mut follow_cursor = true;
        if redraw {
            // The highlighter catches the tree up with the document and the
            // final viewport before the scene reads its spans; a quiet
            // document with an unmoved viewport is a cache hit.
            {
                let text_h = draw::text_height(back.size().1);
                let Tab {
                    doc,
                    view,
                    syntax,
                    diff,
                } = tabs.active_mut();
                if let Some(syntax) = syntax {
                    syntax.pump(doc, &tx);
                    syntax.refresh(doc, view.scroll_line, text_h);
                }
                diff.pump(doc, &tx);
            }
            back.clear();
            let status_caret = match &prompt {
                Some(Prompt::Path { .. } | Prompt::GoTo(_) | Prompt::Help) => {
                    Some(notice.chars().count())
                }
                Some(Prompt::Search(edit)) => Some(edit.caret_chars()),
                _ => None,
            };
            let search = match &prompt {
                Some(Prompt::Search(edit)) => Some(edit.highlights()),
                _ => None,
            };
            let mut cursor = draw::draw(
                &mut back,
                tabs,
                &mut scratch,
                &notice,
                status_caret,
                draw::Marks {
                    search,
                    syntax: tabs
                        .active()
                        .syntax
                        .as_ref()
                        .map_or(&[][..], syntax::Syntax::spans),
                },
                tree.as_ref().map(|t| (t, tree_focus)),
            );
            if matches!(prompt, Some(Prompt::Help)) {
                draw::draw_help(&mut back, &mut scratch);
            }
            if let Some(Prompt::Pick(edit)) = &prompt {
                cursor = draw::draw_picker(&mut back, edit, &mut scratch);
            }
            if let Some(Prompt::Grep(edit)) = &prompt {
                cursor = draw::draw_grep(&mut back, edit, &mut scratch);
            }
            // A focused sidebar shows focus as its selection bar; a blinking
            // caret there would promise typing the tree can't accept.
            let caret = (!(tree_focus && draw::tree_width(tree.is_some(), back.size().0) > 0))
                .then_some(cursor);
            terminal.present(&back, caret)?;
        }
        redraw = true;

        // Page movement wants the text area as it was when the key arrived.
        let text_h = draw::text_height(back.size().1);

        // A pending debounce or journal snapshot turns the indefinite block
        // into a timed one; their expiries are the only wakes that aren't
        // channel messages.
        let grep_restart = match &prompt {
            Some(Prompt::Grep(edit)) => edit.deadline(),
            _ => None,
        };
        let wake = [
            debounce.deadline(),
            journal.deadline(),
            refresh.deadline(),
            head_refresh.deadline(),
            grep_restart,
        ]
        .into_iter()
        .flatten()
        .min();
        let received = match wake {
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
        };
        match received {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let now = Instant::now();
                let fs_due = debounce.deadline().is_some_and(|t| t <= now);
                // A journal snapshot or a spawned refresh walk changes
                // nothing on screen; drawing here would turn the timers
                // into periodic repaints.
                redraw = fs_due;
                if journal.deadline().is_some_and(|t| t <= now) {
                    journal.flush(tabs);
                }
                if refresh.take(now)
                    && let Some(t) = &mut tree
                {
                    if t.busy() {
                        t.rerun = true;
                    } else {
                        walk_gen += 1;
                        let cancel = project::spawn_walk(root.clone(), walk_gen, tx.clone());
                        t.begin_refresh(walk_gen, cancel);
                    }
                }
                if head_refresh.take(now) {
                    // HEAD may have moved under every buffer; a background
                    // tab looks again when it next draws, and the active one
                    // right now — spawning here rather than in the frame is
                    // what keeps a busy repository from repainting the
                    // screen for changes it may not even have.
                    for index in 0..tabs.count() {
                        tabs.get_mut(index).diff.mark_stale();
                    }
                    let Tab { doc, diff, .. } = tabs.active_mut();
                    diff.pump(doc, &tx);
                }
                if fs_due {
                    reload_changed(tabs, &mut debounce);
                    // A reload may have shifted or removed matches; a stale
                    // set must never reach navigation or a replace.
                    if let Some(Prompt::Search(edit)) = &mut prompt {
                        let Tab { doc, view, .. } = tabs.active_mut();
                        edit.refresh(doc, view);
                        edit.render(&mut notice);
                    }
                }
                // A query's restart pause ran out; its search starts, and
                // the overlay's searching indicator appears.
                if let Some(Prompt::Grep(edit)) = &mut prompt
                    && let Some(query) = edit.take_restart(now)
                {
                    walk_gen += 1;
                    let cancel = grep::spawn_search(root.clone(), query, walk_gen, tx.clone());
                    edit.begin(walk_gen, cancel);
                    redraw = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("event channel closed"));
            }
            Ok(AppEvent::InputFailed(e)) => return Err(e),
            Ok(AppEvent::Fs(path)) => {
                let now = Instant::now();
                if tree.is_some() && path.starts_with(&root) {
                    refresh.note(now);
                }
                // A commit or a branch switch touches no working-tree file,
                // so the git directory is the only place it shows up.
                if tabs
                    .all()
                    .iter()
                    .any(|tab| tab.diff.git_dir().is_some_and(|dir| path.starts_with(dir)))
                {
                    head_refresh.note(now);
                }
                debounce.note(path, now);
                // Noting changes nothing on screen; the expiries draw.
                redraw = false;
            }
            Ok(AppEvent::Files(batch)) => {
                redraw = absorb_files(&mut prompt, &mut tree, batch, text_h);
                // A finished walk settled the directory set; mirror it into
                // the watcher.
                if let Some(t) = &tree
                    && let Some(w) = &mut tree_watch
                    && !t.busy()
                {
                    w.sync(
                        std::iter::once(root.clone())
                            .chain(t.dir_paths().map(|d| root.join(d)))
                            .collect(),
                    );
                }
                // A change arrived mid-refresh; follow up now the tree idles.
                if let Some(t) = &mut tree
                    && t.rerun
                    && !t.busy()
                {
                    walk_gen += 1;
                    let cancel = project::spawn_walk(root.clone(), walk_gen, tx.clone());
                    t.begin_refresh(walk_gen, cancel);
                }
            }
            Ok(AppEvent::Hits(batch)) => {
                redraw = absorb_hits(&mut prompt, batch);
            }
            Ok(AppEvent::Parsed(done)) => {
                redraw = absorb_parse(tabs, done, &tx);
            }
            Ok(AppEvent::Diffed(done)) => {
                redraw = absorb_diff(tabs, done, &tx);
            }
            Ok(AppEvent::Input(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                // A pending prompt owns the key; the loop tail still runs,
                // because prompts are allowed to move the cursor.
                if let Some(pending) = prompt.take() {
                    let (next, quit) = prompt_key(pending, &key, tabs, &register, &mut notice);
                    prompt = next;
                    if quit {
                        break;
                    }
                } else if let Some(t) = &mut tree
                    && tree_focus
                    // A terminal too narrow to show the sidebar must not
                    // let it swallow keys invisibly.
                    && draw::tree_width(true, back.size().0) > 0
                    // Chords stay global: Ctrl+S, Ctrl+Q, tab switching and
                    // the toggle itself all work from the tree.
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                {
                    notice.clear();
                    match t.key(&key, text_h) {
                        tree::Outcome::Pending => {}
                        tree::Outcome::FocusEditor => tree_focus = false,
                        tree::Outcome::Open(path) => {
                            open_path(tabs, path, &mut notice);
                            tree_focus = false;
                        }
                    }
                } else {
                    notice.clear();
                    if let Some(action) = keymap::lookup(&key) {
                        let Tab {
                            doc, view, diff, ..
                        } = tabs.active_mut();
                        if action.is_movement() {
                            // Shift extends a selection through any movement key;
                            // Char keys never map to movement, so shifted typing
                            // is safe.
                            view.begin_or_clear_selection(
                                key.modifiers.contains(KeyModifiers::SHIFT),
                            );
                            doc.break_undo_group();
                        }
                        match action {
                            Action::PrevTab | Action::NextTab => {
                                doc.break_undo_group();
                                if action == Action::PrevTab {
                                    tabs.prev();
                                } else {
                                    tabs.next();
                                }
                                // No continue: the loop tail re-clamps the incoming
                                // view, which may have missed resizes while hidden.
                            }
                            Action::Quit => {
                                if tabs.any_dirty() {
                                    prompt = open_prompt(
                                        Prompt::Quit,
                                        &tabs.active_mut().doc,
                                        &mut notice,
                                    );
                                } else {
                                    break;
                                }
                            }
                            Action::NewTab => {
                                doc.break_undo_group();
                                tabs.push(Document::empty());
                            }
                            Action::Open => {
                                prompt = open_prompt(
                                    Prompt::Path {
                                        edit: PathPrompt::new("open: "),
                                        action: PathAction::Open,
                                    },
                                    doc,
                                    &mut notice,
                                );
                            }
                            Action::Find => {
                                doc.break_undo_group();
                                let edit = SearchPrompt::new(view);
                                // The current match renders in reverse video, which
                                // must not fight a reverse-video selection; the
                                // origin keeps the anchor for Esc to restore.
                                view.anchor = None;
                                prompt = open_prompt(Prompt::Search(edit), doc, &mut notice);
                            }
                            Action::FindProject => {
                                // The search itself spawns once a query is
                                // typed and its restart pause runs out.
                                prompt = open_prompt(
                                    Prompt::Grep(Grep::new(root.clone())),
                                    doc,
                                    &mut notice,
                                );
                            }
                            Action::GoToLine => {
                                prompt =
                                    open_prompt(Prompt::GoTo(LinePrompt::new()), doc, &mut notice);
                            }
                            Action::PrevChange | Action::NextChange => {
                                // Not classified as movement: a hunk jump has
                                // no shift-extends-selection reading.
                                doc.break_undo_group();
                                view.begin_or_clear_selection(false);
                                let from = view.line(doc);
                                let to = if action == Action::NextChange {
                                    diff.next_change(from)
                                } else {
                                    diff.prev_change(from)
                                };
                                // The loop tail scrolls the landing into view.
                                if let Some(line) = to {
                                    view.move_to_line(doc, line + 1);
                                }
                            }
                            Action::CloseTab => {
                                if doc.dirty() {
                                    prompt = open_prompt(
                                        Prompt::Close,
                                        &tabs.active_mut().doc,
                                        &mut notice,
                                    );
                                } else if close_active_or_quit(tabs) {
                                    break;
                                }
                            }
                            Action::Help => {
                                prompt = open_prompt(Prompt::Help, doc, &mut notice);
                            }
                            Action::ToggleTree => match tree.take() {
                                Some(t) => {
                                    t.dismiss();
                                    tree_watch = None;
                                    tree_focus = false;
                                    refresh = Refresh::default();
                                }
                                None => {
                                    walk_gen += 1;
                                    let cancel =
                                        project::spawn_walk(root.clone(), walk_gen, tx.clone());
                                    let mut t = Tree::new(root.clone(), walk_gen, cancel);
                                    t.set_active(tabs.active().doc.path());
                                    tree = Some(t);
                                    tree_focus = true;
                                    // Losing the watcher (inotify limits,
                                    // say) costs auto-refresh, not the tree.
                                    match TreeWatcher::new(tx.clone()) {
                                        Ok(mut w) => {
                                            w.sync(std::iter::once(root.clone()).collect());
                                            tree_watch = Some(w);
                                        }
                                        Err(e) => {
                                            let _ = write!(
                                                notice,
                                                "tree auto-refresh unavailable: {e}"
                                            );
                                        }
                                    }
                                }
                            },
                            Action::PickFile => {
                                walk_gen += 1;
                                let cancel =
                                    project::spawn_walk(root.clone(), walk_gen, tx.clone());
                                prompt = open_prompt(
                                    Prompt::Pick(Picker::new(root.clone(), walk_gen, cancel)),
                                    doc,
                                    &mut notice,
                                );
                            }
                            Action::Save => {
                                if doc.path().is_none() {
                                    prompt = open_prompt(
                                        save_as_prompt(AfterSave::Stay),
                                        doc,
                                        &mut notice,
                                    );
                                } else if doc.lossy() {
                                    prompt = open_prompt(
                                        Prompt::LossySave {
                                            then: AfterSave::Stay,
                                        },
                                        doc,
                                        &mut notice,
                                    );
                                } else {
                                    try_save(doc, &mut notice);
                                }
                            }
                            Action::Copy => {
                                if let Some(text) = view.selected_text(doc) {
                                    register = text;
                                    share_copy(&mut terminal, &register, in_tmux, &mut notice)?;
                                }
                            }
                            Action::Cut => {
                                if let Some(text) = view.cut(doc) {
                                    register = text;
                                    share_copy(&mut terminal, &register, in_tmux, &mut notice)?;
                                }
                            }
                            Action::Paste => {
                                if !register.is_empty() {
                                    view.paste(doc, &register);
                                }
                            }
                            Action::Undo => {
                                if let Some(caret) = doc.undo() {
                                    view.set_caret(caret);
                                }
                            }
                            Action::Redo => {
                                if let Some(caret) = doc.redo() {
                                    view.set_caret(caret);
                                }
                            }
                            Action::WordLeft => view.move_word_left(doc),
                            Action::WordRight => view.move_word_right(doc),
                            Action::DocStart => view.move_doc_start(),
                            Action::DocEnd => view.move_doc_end(doc),
                            Action::Left => view.move_left(doc),
                            Action::Right => view.move_right(doc),
                            Action::Up => view.move_up(doc),
                            Action::Down => view.move_down(doc),
                            Action::Home => view.move_home(doc),
                            Action::End => view.move_end(doc),
                            Action::PageUp => view.page_up(doc, text_h),
                            Action::PageDown => view.page_down(doc, text_h),
                            Action::Newline => view.insert_newline(doc),
                            Action::InsertTab => view.insert_tab(doc),
                            Action::Backspace => view.backspace(doc),
                            Action::Delete => view.delete(doc),
                        }
                        // A prompt owns the keys now; the tree yields.
                        if prompt.is_some() {
                            tree_focus = false;
                        }
                    } else if let KeyCode::Char(ch) = key.code
                        // Alt-modified letters are terminal escape chords, not
                        // text to insert.
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        let Tab { doc, view, .. } = tabs.active_mut();
                        view.insert_char(doc, ch);
                    }
                }
            }
            Ok(AppEvent::Input(Event::Resize(width, height))) => {
                terminal.resize(width, height);
                back.resize(width, height);
            }
            #[cfg(unix)]
            Ok(AppEvent::Signal(sig)) => match sig {
                signal_hook::consts::SIGTSTP => {
                    let (width, height) = terminal.suspend()?;
                    back.resize(width, height);
                }
                // Also fires after an uncatchable external SIGSTOP; after
                // our own suspend it is a redundant repaint.
                signal_hook::consts::SIGCONT => {
                    let (width, height) = terminal.resync_size()?;
                    back.resize(width, height);
                }
                _ => return Ok(Exit::Signal(sig)),
            },
            Ok(AppEvent::Input(Event::Mouse(m))) => {
                let tree_w = draw::tree_width(tree.is_some(), back.size().0);
                match m.kind {
                    // The wheel works during prompts too: it only moves a
                    // viewport, which the prompts don't own. Over the
                    // sidebar it moves the tree's.
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        let delta = if m.kind == MouseEventKind::ScrollUp {
                            -(WHEEL_LINES as isize)
                        } else {
                            WHEEL_LINES as isize
                        };
                        if usize::from(m.column) < tree_w {
                            if let Some(t) = &mut tree {
                                t.scroll_by(delta, text_h);
                            }
                        } else {
                            let Tab { doc, view, .. } = tabs.active_mut();
                            view.scroll_wheel(doc, delta);
                        }
                        follow_cursor = false;
                    }
                    // A click in the sidebar focuses it and lands on its
                    // row: a file opens, a directory toggles. Like a text
                    // click it accepts an open search and nothing else.
                    MouseEventKind::Down(MouseButton::Left)
                        if usize::from(m.column) < tree_w
                            && (1..=text_h).contains(&usize::from(m.row))
                            && matches!(prompt, None | Some(Prompt::Search(_))) =>
                    {
                        prompt = None;
                        notice.clear();
                        tree_focus = true;
                        if let Some(t) = &mut tree
                            && let tree::Outcome::Open(path) =
                                t.click(usize::from(m.row) - 1, text_h)
                        {
                            open_path(tabs, path, &mut notice);
                            tree_focus = false;
                        }
                    }
                    // A click while searching accepts the search — the
                    // viewport and cursor stay — then lands like any click.
                    // The other prompts keep the screen: a stray click must
                    // not dismiss a save confirmation.
                    MouseEventKind::Down(MouseButton::Left)
                        if (1..=text_h).contains(&usize::from(m.row))
                            && matches!(prompt, None | Some(Prompt::Search(_))) =>
                    {
                        prompt = None;
                        notice.clear();
                        tree_focus = false;
                        let shift = m.modifiers.contains(KeyModifiers::SHIFT);
                        let double = !shift
                            && last_click.is_some_and(|(t, x, y)| {
                                t.elapsed() <= DOUBLE_CLICK && (x, y) == (m.column, m.row)
                            });
                        last_click = Some((Instant::now(), m.column, m.row));
                        let gutter_w = draw::gutter_width(tabs.active());
                        let Tab { doc, view, .. } = tabs.active_mut();
                        doc.break_undo_group();
                        view.click(doc, tree_w + gutter_w, text_h, m.column, m.row, shift);
                        if double {
                            view.select_word(doc);
                        }
                    }
                    // A click on a tab label activates it, accepting any
                    // open search on the way out.
                    MouseEventKind::Down(MouseButton::Left)
                        if m.row == 0 && matches!(prompt, None | Some(Prompt::Search(_))) =>
                    {
                        if let Some(i) = draw::tab_at(tabs, usize::from(back.size().0), m.column) {
                            prompt = None;
                            notice.clear();
                            tree_focus = false;
                            tabs.active_mut().doc.break_undo_group();
                            tabs.activate(i);
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) if prompt.is_none() && !tree_focus => {
                        let gutter_w = draw::gutter_width(tabs.active());
                        let Tab { doc, view, .. } = tabs.active_mut();
                        view.drag(doc, tree_w + gutter_w, text_h, m.column, m.row);
                    }
                    _ => {}
                }
            }
            Ok(AppEvent::Input(Event::Paste(text))) => {
                if let Some(pending) = &mut prompt {
                    prompt_paste(pending, &text, tabs, &mut notice);
                } else {
                    notice.clear();
                    let Tab { doc, view, .. } = tabs.active_mut();
                    view.paste(doc, &text);
                }
            }
            Ok(AppEvent::Input(_)) => {}
        }

        if let Some(watcher) = &mut watcher {
            watcher.sync(tabs);
        }
        // One call site covers every way the edited file changes: open,
        // close, switch, save-as.
        if let Some(t) = &mut tree {
            redraw |= t.set_active(tabs.active().doc.path());
        }
        // One call site covers every way an entry goes stale: a save, an
        // undo back to clean, a discarded close. A failure notice appearing
        // here must reach the screen even on a wake that skips the draw.
        let quiet = notice.is_empty();
        journal.sync(tabs, Instant::now(), &mut notice);
        redraw |= quiet && !notice.is_empty();

        // Re-fetch the size: a resize may have changed it. A wheel scroll
        // opts out of the snap, or it would scroll straight back.
        if follow_cursor {
            let (width, height) = back.size();
            let tree_w = draw::tree_width(tree.is_some(), width);
            let gutter_w = draw::gutter_width(tabs.active());
            let Tab { doc, view, .. } = tabs.active_mut();
            let text_w = usize::from(width).saturating_sub(tree_w + gutter_w);
            view.scroll_to_cursor(doc, text_w, draw::text_height(height));
        }
    }
    Ok(Exit::Quit)
}

/// The debounce expired: reconcile every touched tab with its file. Paths
/// that match no tab — our own temp files, unrelated neighbours in a
/// watched directory — fall out here.
fn reload_changed(tabs: &mut Tabs, debounce: &mut Debounce) {
    for path in debounce.take() {
        let Some(index) = tabs.find_by_path(&path) else {
            continue;
        };
        let Tab { doc, view, .. } = tabs.get_mut(index);
        let caret = Caret {
            cursor: view.cursor,
            anchor: view.anchor,
        };
        if let DiskCheck::Reloaded { old, span } = doc.check_disk(caret) {
            view.remap_after_reload(&old, doc.rope(), &span);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh scratch directory per test: tests run in parallel, so each
    /// needs its own.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("connor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(path: Option<PathBuf>, text: &str) -> Recovered {
        Recovered {
            path,
            text: text.to_owned(),
        }
    }

    #[test]
    fn recovery_splices_into_the_doc_already_open_for_that_path() {
        let dir = scratch_dir("merge-dedupe");
        let path = dir.join("f.txt");
        std::fs::write(&path, "disk\n").unwrap();
        let mut docs = vec![Document::open(path.clone()).unwrap()];
        let notice = merge_recovered(&mut docs, vec![entry(Some(path), "edited\n")], None);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].rope().to_string(), "edited\n");
        assert!(docs[0].dirty());
        assert!(docs[0].recovered());
        assert_eq!(notice, "recovered 1 unsaved buffer");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recovery_opens_its_own_tab_for_a_path_not_on_the_command_line() {
        let dir = scratch_dir("merge-new-tab");
        let path = dir.join("f.txt");
        std::fs::write(&path, "disk\n").unwrap();
        let mut docs = Vec::new();
        merge_recovered(&mut docs, vec![entry(Some(path.clone()), "edited\n")], None);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path(), Some(path.as_path()));
        assert_eq!(docs[0].rope().to_string(), "edited\n");
        assert!(docs[0].dirty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recovery_for_a_deleted_file_still_carries_its_path() {
        let dir = scratch_dir("merge-deleted");
        let path = dir.join("gone.txt");
        let mut docs = Vec::new();
        let notice = merge_recovered(&mut docs, vec![entry(Some(path.clone()), "lost\n")], None);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path(), Some(path.as_path()));
        assert!(docs[0].dirty());
        assert_eq!(notice, "recovered 1 unsaved buffer");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_pathless_entry_becomes_a_dirty_no_name_buffer() {
        let mut docs = Vec::new();
        let notice = merge_recovered(&mut docs, vec![entry(None, "scratch\n")], None);
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].path(), None);
        assert!(docs[0].dirty());
        assert!(docs[0].recovered());
        assert_eq!(notice, "recovered 1 unsaved buffer");
    }

    #[test]
    fn an_entry_matching_the_disk_is_dropped_without_a_tab() {
        let dir = scratch_dir("merge-clean");
        let path = dir.join("f.txt");
        std::fs::write(&path, "same\n").unwrap();
        let mut docs = Vec::new();
        let notice = merge_recovered(&mut docs, vec![entry(Some(path), "same\n")], None);
        assert!(docs.is_empty());
        assert_eq!(notice, "");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn help_prompt_closes_on_esc_and_its_own_chords_only() {
        let mut tabs = Tabs::new(vec![Document::empty()]);
        let mut notice = String::new();
        for (code, mods) in [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::F(1), KeyModifiers::NONE),
            (KeyCode::Char('/'), KeyModifiers::CONTROL),
            (KeyCode::Char('7'), KeyModifiers::CONTROL),
        ] {
            notice.push_str("(esc) close");
            let key = KeyEvent::new(code, mods);
            let (next, quit) = prompt_key(Prompt::Help, &key, &mut tabs, "", &mut notice);
            assert!(next.is_none(), "{code:?} left help open");
            assert!(!quit);
            assert!(notice.is_empty());
        }
        for (code, mods) in [
            (KeyCode::Char('a'), KeyModifiers::NONE),
            (KeyCode::Char('q'), KeyModifiers::CONTROL),
            (KeyCode::Enter, KeyModifiers::NONE),
        ] {
            let key = KeyEvent::new(code, mods);
            let (next, quit) = prompt_key(Prompt::Help, &key, &mut tabs, "", &mut notice);
            assert!(matches!(next, Some(Prompt::Help)), "{code:?} closed help");
            assert!(!quit);
        }
    }

    fn pick_prompt(root: PathBuf, paths: &[&str]) -> Prompt {
        let mut picker = Picker::new(root, 1, Default::default());
        picker.absorb(project::FileBatch {
            generation: 1,
            paths: paths.iter().map(|s| s.to_string()).collect(),
            done: true,
        });
        Prompt::Pick(picker)
    }

    #[test]
    fn pick_prompt_types_closes_on_its_chord_and_opens_on_enter() {
        let dir = scratch_dir("pick-open");
        std::fs::write(dir.join("f.txt"), "text\n").unwrap();
        let mut tabs = Tabs::new(vec![Document::empty()]);
        let mut notice = String::new();

        // A typed character leaves the picker up.
        let prompt = pick_prompt(dir.clone(), &["f.txt"]);
        let key = KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE);
        let (next, quit) = prompt_key(prompt, &key, &mut tabs, "", &mut notice);
        assert!(matches!(next, Some(Prompt::Pick(_))));
        assert!(!quit);

        // Esc and a repeat Ctrl+P both dismiss without opening.
        for (code, mods) in [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('p'), KeyModifiers::CONTROL),
        ] {
            notice.push_str("↑↓ select · enter open · esc");
            let prompt = pick_prompt(dir.clone(), &["f.txt"]);
            let key = KeyEvent::new(code, mods);
            let (next, quit) = prompt_key(prompt, &key, &mut tabs, "", &mut notice);
            assert!(next.is_none(), "{code:?} left the picker open");
            assert!(!quit);
            assert!(notice.is_empty());
            assert_eq!(tabs.count(), 1);
        }

        // Enter opens the selection in a new tab; a second Enter on the
        // same path activates that tab instead of duplicating it.
        for _ in 0..2 {
            let prompt = pick_prompt(dir.clone(), &["f.txt"]);
            let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
            let (next, quit) = prompt_key(prompt, &key, &mut tabs, "", &mut notice);
            assert!(next.is_none());
            assert!(!quit);
            assert_eq!(tabs.count(), 2);
            assert_eq!(
                tabs.active_mut().doc.path(),
                Some(dir.join("f.txt").as_path())
            );
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn grep_prompt(root: PathBuf, path: &str, hits: Vec<grep::Hit>) -> Prompt {
        let mut edit = Grep::new(root);
        edit.begin(1, Default::default());
        edit.absorb(grep::HitBatch {
            generation: 1,
            files: vec![grep::FileHits {
                path: path.to_string(),
                hits,
            }],
            done: true,
            truncated: false,
        });
        Prompt::Grep(edit)
    }

    fn hit(line: u32, col: u32) -> grep::Hit {
        grep::Hit {
            line,
            col,
            preview: String::new(),
        }
    }

    #[test]
    fn grep_prompt_types_closes_on_its_chord_and_jumps_on_enter() {
        let dir = scratch_dir("grep-jump");
        std::fs::write(dir.join("f.txt"), "one\nneedle here\n").unwrap();
        let mut tabs = Tabs::new(vec![Document::empty()]);
        let mut notice = String::new();

        // A typed character leaves the overlay up.
        let prompt = grep_prompt(dir.clone(), "f.txt", vec![hit(2, 7)]);
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        let (next, quit) = prompt_key(prompt, &key, &mut tabs, "", &mut notice);
        assert!(matches!(next, Some(Prompt::Grep(_))));
        assert!(!quit);

        // Esc and a repeat Alt+F both dismiss without opening.
        for (code, mods) in [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('f'), KeyModifiers::ALT),
        ] {
            notice.push_str("↑↓ select · enter jump · esc");
            let prompt = grep_prompt(dir.clone(), "f.txt", vec![hit(2, 7)]);
            let key = KeyEvent::new(code, mods);
            let (next, quit) = prompt_key(prompt, &key, &mut tabs, "", &mut notice);
            assert!(next.is_none(), "{code:?} left the overlay open");
            assert!(!quit);
            assert!(notice.is_empty());
            assert_eq!(tabs.count(), 1);
        }

        // Enter opens the hit's file and lands the caret on its line and
        // column.
        let prompt = grep_prompt(dir.clone(), "f.txt", vec![hit(2, 7)]);
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let (next, quit) = prompt_key(prompt, &key, &mut tabs, "", &mut notice);
        assert!(next.is_none());
        assert!(!quit);
        assert_eq!(tabs.count(), 2);
        let Tab { doc, view, .. } = tabs.active_mut();
        assert_eq!(doc.path(), Some(dir.join("f.txt").as_path()));
        assert_eq!(view.cursor, doc.line_start(1) + 7);
        assert_eq!(view.anchor, None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_hit_outrunning_the_changed_file_clamps_instead_of_panicking() {
        let dir = scratch_dir("grep-clamp");
        std::fs::write(dir.join("f.txt"), "short\n").unwrap();
        let mut tabs = Tabs::new(vec![Document::empty()]);
        let mut notice = String::new();
        let prompt = grep_prompt(dir.clone(), "f.txt", vec![hit(99, 50)]);
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        prompt_key(prompt, &key, &mut tabs, "", &mut notice);
        let Tab { doc, view, .. } = tabs.active_mut();
        assert!(view.cursor <= doc.rope().len_chars());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unopenable_hit_file_reports_and_moves_no_caret() {
        // A missing file opens as an empty buffer by design; a directory is
        // what genuinely fails to open.
        let dir = scratch_dir("grep-unopenable");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let mut tabs = Tabs::new(vec![Document::from_str("stay\n")]);
        tabs.active_mut().view.cursor = 3;
        let mut notice = String::new();
        let prompt = grep_prompt(dir.clone(), "sub", vec![hit(1, 0)]);
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        let (next, _) = prompt_key(prompt, &key, &mut tabs, "", &mut notice);
        assert!(next.is_none());
        assert!(notice.contains("open failed"), "notice: {notice}");
        assert_eq!(tabs.count(), 1);
        assert_eq!(tabs.active_mut().view.cursor, 3);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hit_batches_route_to_the_grep_overlay_by_generation() {
        let batch = |generation| grep::HitBatch {
            generation,
            files: vec![grep::FileHits {
                path: "a.rs".to_string(),
                hits: vec![hit(1, 0)],
            }],
            done: false,
            truncated: false,
        };
        let mut prompt = match grep_prompt(PathBuf::from("/r"), "seed.rs", vec![hit(1, 0)]) {
            Prompt::Grep(mut edit) => {
                edit.begin(2, Default::default());
                Some(Prompt::Grep(edit))
            }
            _ => unreachable!(),
        };
        assert!(absorb_hits(&mut prompt, batch(2)));
        // A dead search's chunk changes nothing, and no overlay eats it.
        assert!(!absorb_hits(&mut prompt, batch(9)));
        let Some(Prompt::Grep(edit)) = &prompt else {
            unreachable!()
        };
        assert_eq!(edit.hit_count(), 2);
        assert!(!absorb_hits(&mut None, batch(2)));
        let mut help = Some(Prompt::Help);
        assert!(!absorb_hits(&mut help, batch(2)));
    }

    #[test]
    fn walker_batches_route_to_the_picker_or_the_tree_by_generation() {
        let mut picker = Picker::new(PathBuf::from("/r"), 1, Default::default());
        picker.absorb(project::FileBatch {
            generation: 1,
            paths: Vec::new(),
            done: false,
        });
        let mut prompt = Some(Prompt::Pick(picker));
        let mut tree = Some(Tree::new(PathBuf::from("/r"), 2, Default::default()));

        let batch = |generation, path: &str| project::FileBatch {
            generation,
            paths: vec![path.to_string()],
            done: true,
        };
        assert!(absorb_files(
            &mut prompt,
            &mut tree,
            batch(1, "picked.rs"),
            10
        ));
        assert!(absorb_files(
            &mut prompt,
            &mut tree,
            batch(2, "treed.rs"),
            10
        ));
        // A dead walk's chunk reaches neither.
        assert!(!absorb_files(
            &mut prompt,
            &mut tree,
            batch(9, "ghost.rs"),
            10
        ));

        let Some(Prompt::Pick(picker)) = &prompt else {
            unreachable!()
        };
        assert_eq!(picker.total(), 1);
        assert_eq!(picker.shown(0), "picked.rs");
        let tree = tree.unwrap();
        assert_eq!(tree.visible_len(), 1);
        assert_eq!(tree.row(0).name, "treed.rs");
    }

    #[test]
    fn the_startup_notice_yields_to_a_recovery_and_stands_otherwise() {
        let mut docs = Vec::new();
        let disabled = Some("crash journal disabled: x".to_owned());
        let notice = merge_recovered(&mut docs, Vec::new(), disabled.clone());
        assert_eq!(notice, "crash journal disabled: x");
        let entries = vec![entry(None, "a\n"), entry(None, "b\n")];
        let notice = merge_recovered(&mut docs, entries, disabled);
        assert_eq!(notice, "recovered 2 unsaved buffers");
    }
}
