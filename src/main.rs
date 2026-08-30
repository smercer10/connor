mod clip;
mod doc;
mod draw;
mod grapheme;
mod journal;
mod prompt;
mod screen;
mod search;
mod tabs;
mod term;
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
use journal::{Journal, Recovered};
use prompt::{LinePrompt, Outcome, PathPrompt};
use screen::Screen;
use search::SearchPrompt;
use tabs::{Tab, Tabs};
use term::Terminal;
use watch::{AppEvent, Debounce, DirWatcher};

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
    if result.is_err() {
        // The terminal died out from under a possibly dirty buffer; a last
        // snapshot before abandoning the session dir keeps it current.
        journal.flush(&tabs);
    }
    journal.finish(result.is_ok());
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("connor: {e}");
            ExitCode::FAILURE
        }
    }
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
            let Tab { doc, view } = tabs.active_mut();
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
        Prompt::Quit | Prompt::Close | Prompt::LossySave { .. } => {}
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
    // Ctrl+V pastes the register into the prompt's field; the prompts' own
    // key handlers never see modified chords.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('v') {
        let mut prompt = prompt;
        if !register.is_empty() {
            prompt_paste(&mut prompt, register, tabs, notice);
        }
        return (Some(prompt), false);
    }
    // A search prompt consumes every key itself, and may move the cursor
    // and edit the document.
    if let Prompt::Search(mut edit) = prompt {
        let Tab { doc, view } = tabs.active_mut();
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
                    let Tab { doc, view } = tabs.active_mut();
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
/// that tab instead of opening the file twice.
fn open_path(tabs: &mut Tabs, path: PathBuf, notice: &mut String) {
    if let Some(index) = tabs.find_by_path(&path) {
        tabs.activate(index);
        return;
    }
    match Document::open(path) {
        Ok(doc) => {
            tabs.active_mut().doc.break_undo_group();
            tabs.push(doc);
        }
        Err(e) => {
            let _ = write!(notice, "open failed: {e}");
        }
    }
}

/// Names the buffer and saves it there, then carries on with whatever the
/// save was clearing the way for.
fn save_as(
    tabs: &mut Tabs,
    path: PathBuf,
    then: AfterSave,
    notice: &mut String,
) -> (Option<Prompt>, bool) {
    let doc = &mut tabs.active_mut().doc;
    doc.set_path(path);
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

/// Closing the last tab is quitting; returns whether to quit.
fn close_active_or_quit(tabs: &mut Tabs) -> bool {
    if tabs.count() == 1 {
        return true;
    }
    tabs.close_active();
    false
}

fn run(tabs: &mut Tabs, journal: &mut Journal, notice: String) -> io::Result<()> {
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
    // Losing the watcher (inotify limits, say) costs reloads, not the
    // editor.
    let mut watcher = match DirWatcher::new(tx) {
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
    let mut redraw = true;

    loop {
        let mut follow_cursor = true;
        if redraw {
            back.clear();
            let status_caret = match &prompt {
                Some(Prompt::Path { .. } | Prompt::GoTo(_)) => Some(notice.chars().count()),
                Some(Prompt::Search(edit)) => Some(edit.caret_chars()),
                _ => None,
            };
            let search = match &prompt {
                Some(Prompt::Search(edit)) => Some(edit.highlights()),
                _ => None,
            };
            let cursor = draw::draw(&mut back, tabs, &mut scratch, &notice, status_caret, search);
            terminal.present(&back, cursor)?;
        }
        redraw = true;

        // Page movement wants the text area as it was when the key arrived.
        let text_h = draw::text_height(back.size().1);

        // A pending debounce or journal snapshot turns the indefinite block
        // into a timed one; their expiries are the only wakes that aren't
        // channel messages.
        let wake = [debounce.deadline(), journal.deadline()]
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
                if journal.deadline().is_some_and(|t| t <= now) {
                    journal.flush(tabs);
                    // A snapshot changes nothing on screen; drawing here
                    // would turn the timer into a periodic repaint.
                    redraw = fs_due;
                }
                if fs_due {
                    reload_changed(tabs, &mut debounce);
                    // A reload may have shifted or removed matches; a stale
                    // set must never reach navigation or a replace.
                    if let Some(Prompt::Search(edit)) = &mut prompt {
                        let Tab { doc, view } = tabs.active_mut();
                        edit.refresh(doc, view);
                        edit.render(&mut notice);
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("event channel closed"));
            }
            Ok(AppEvent::InputFailed(e)) => return Err(e),
            Ok(AppEvent::Fs(path)) => debounce.note(path, Instant::now()),
            Ok(AppEvent::Input(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                // A pending prompt owns the key; the loop tail still runs,
                // because prompts are allowed to move the cursor.
                if let Some(pending) = prompt.take() {
                    let (next, quit) = prompt_key(pending, &key, tabs, &register, &mut notice);
                    prompt = next;
                    if quit {
                        break;
                    }
                } else {
                    notice.clear();
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    let alt = key.modifiers.contains(KeyModifiers::ALT);
                    // Tab chords are claimed before the buffer keys: their base
                    // keys are movement keys, and the movement path below would
                    // clear the selection and move the wrong tab's cursor.
                    let prev_tab =
                        (ctrl && key.code == KeyCode::PageUp) || (alt && key.code == KeyCode::Left);
                    let next_tab = (ctrl && key.code == KeyCode::PageDown)
                        || (alt && key.code == KeyCode::Right);
                    if prev_tab || next_tab {
                        tabs.active_mut().doc.break_undo_group();
                        if prev_tab {
                            tabs.prev();
                        } else {
                            tabs.next();
                        }
                        // No continue: the loop tail re-clamps the incoming
                        // view, which may have missed resizes while hidden.
                    } else if ctrl && key.code == KeyCode::Char('q') {
                        if tabs.any_dirty() {
                            prompt = open_prompt(Prompt::Quit, &tabs.active_mut().doc, &mut notice);
                        } else {
                            break;
                        }
                    } else if ctrl && key.code == KeyCode::Char('n') {
                        tabs.active_mut().doc.break_undo_group();
                        tabs.push(Document::empty());
                    } else if ctrl && key.code == KeyCode::Char('o') {
                        prompt = open_prompt(
                            Prompt::Path {
                                edit: PathPrompt::new("open: "),
                                action: PathAction::Open,
                            },
                            &tabs.active_mut().doc,
                            &mut notice,
                        );
                    } else if ctrl && key.code == KeyCode::Char('f') {
                        let Tab { doc, view } = tabs.active_mut();
                        doc.break_undo_group();
                        let edit = SearchPrompt::new(view);
                        // The current match renders in reverse video, which
                        // must not fight a reverse-video selection; the
                        // origin keeps the anchor for Esc to restore.
                        view.anchor = None;
                        prompt = open_prompt(Prompt::Search(edit), doc, &mut notice);
                    } else if ctrl && key.code == KeyCode::Char('g') {
                        prompt = open_prompt(
                            Prompt::GoTo(LinePrompt::new()),
                            &tabs.active_mut().doc,
                            &mut notice,
                        );
                    } else if ctrl && key.code == KeyCode::Char('w') {
                        if tabs.active_mut().doc.dirty() {
                            prompt =
                                open_prompt(Prompt::Close, &tabs.active_mut().doc, &mut notice);
                        } else if close_active_or_quit(tabs) {
                            break;
                        }
                    } else {
                        let Tab { doc, view } = tabs.active_mut();
                        let movement = matches!(
                            key.code,
                            KeyCode::Left
                                | KeyCode::Right
                                | KeyCode::Up
                                | KeyCode::Down
                                | KeyCode::Home
                                | KeyCode::End
                                | KeyCode::PageUp
                                | KeyCode::PageDown
                        );
                        if movement {
                            // Shift extends a selection through any movement key;
                            // Char keys never land here, so shifted typing is safe.
                            view.begin_or_clear_selection(
                                key.modifiers.contains(KeyModifiers::SHIFT),
                            );
                            doc.break_undo_group();
                        }
                        match key.code {
                            KeyCode::Char('s') if ctrl => {
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
                            KeyCode::Char('c') if ctrl => {
                                if let Some(text) = view.selected_text(doc) {
                                    register = text;
                                    share_copy(&mut terminal, &register, in_tmux, &mut notice)?;
                                }
                            }
                            KeyCode::Char('x') if ctrl => {
                                if let Some(text) = view.cut(doc) {
                                    register = text;
                                    share_copy(&mut terminal, &register, in_tmux, &mut notice)?;
                                }
                            }
                            KeyCode::Char('v') if ctrl => {
                                if !register.is_empty() {
                                    view.paste(doc, &register);
                                }
                            }
                            KeyCode::Char('z') if ctrl => {
                                if let Some(caret) = doc.undo() {
                                    view.set_caret(caret);
                                }
                            }
                            KeyCode::Char('y') if ctrl => {
                                if let Some(caret) = doc.redo() {
                                    view.set_caret(caret);
                                }
                            }
                            #[cfg(debug_assertions)]
                            KeyCode::Char('p') if ctrl => panic!("deliberate panic (Ctrl+P)"),
                            KeyCode::Left if ctrl => view.move_word_left(doc),
                            KeyCode::Right if ctrl => view.move_word_right(doc),
                            KeyCode::Home if ctrl => view.move_doc_start(),
                            KeyCode::End if ctrl => view.move_doc_end(doc),
                            KeyCode::Left => view.move_left(doc),
                            KeyCode::Right => view.move_right(doc),
                            KeyCode::Up => view.move_up(doc),
                            KeyCode::Down => view.move_down(doc),
                            KeyCode::Home => view.move_home(doc),
                            KeyCode::End => view.move_end(doc),
                            KeyCode::PageUp => view.page_up(doc, text_h),
                            KeyCode::PageDown => view.page_down(doc, text_h),
                            // Alt-modified letters are terminal escape chords, not
                            // text to insert.
                            KeyCode::Char(ch)
                                if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) =>
                            {
                                view.insert_char(doc, ch)
                            }
                            KeyCode::Enter => view.insert_newline(doc),
                            KeyCode::Tab => view.insert_tab(doc),
                            KeyCode::Backspace => view.backspace(doc),
                            KeyCode::Delete => view.delete(doc),
                            _ => {}
                        }
                    }
                }
            }
            Ok(AppEvent::Input(Event::Resize(width, height))) => {
                terminal.resize(width, height);
                back.resize(width, height);
            }
            Ok(AppEvent::Input(Event::Mouse(m))) => match m.kind {
                // The wheel works during prompts too: it only moves the
                // viewport, which the prompts don't own.
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                    let delta = if m.kind == MouseEventKind::ScrollUp {
                        -(WHEEL_LINES as isize)
                    } else {
                        WHEEL_LINES as isize
                    };
                    let Tab { doc, view } = tabs.active_mut();
                    view.scroll_wheel(doc, delta);
                    follow_cursor = false;
                }
                // A click while searching accepts the search — the viewport
                // and cursor stay — then lands like any click. The other
                // prompts keep the screen: a stray click must not dismiss a
                // save confirmation.
                MouseEventKind::Down(MouseButton::Left)
                    if (1..=text_h).contains(&usize::from(m.row))
                        && matches!(prompt, None | Some(Prompt::Search(_))) =>
                {
                    prompt = None;
                    notice.clear();
                    let shift = m.modifiers.contains(KeyModifiers::SHIFT);
                    let double = !shift
                        && last_click.is_some_and(|(t, x, y)| {
                            t.elapsed() <= DOUBLE_CLICK && (x, y) == (m.column, m.row)
                        });
                    last_click = Some((Instant::now(), m.column, m.row));
                    let Tab { doc, view } = tabs.active_mut();
                    doc.break_undo_group();
                    let gutter_w = draw::gutter_width(doc);
                    view.click(doc, gutter_w, text_h, m.column, m.row, shift);
                    if double {
                        view.select_word(doc);
                    }
                }
                // A click on a tab label activates it, accepting any open
                // search on the way out.
                MouseEventKind::Down(MouseButton::Left)
                    if m.row == 0 && matches!(prompt, None | Some(Prompt::Search(_))) =>
                {
                    if let Some(i) = draw::tab_at(tabs, usize::from(back.size().0), m.column) {
                        prompt = None;
                        notice.clear();
                        tabs.active_mut().doc.break_undo_group();
                        tabs.activate(i);
                    }
                }
                MouseEventKind::Drag(MouseButton::Left) if prompt.is_none() => {
                    let Tab { doc, view } = tabs.active_mut();
                    let gutter_w = draw::gutter_width(doc);
                    view.drag(doc, gutter_w, text_h, m.column, m.row);
                }
                _ => {}
            },
            Ok(AppEvent::Input(Event::Paste(text))) => {
                if let Some(pending) = &mut prompt {
                    prompt_paste(pending, &text, tabs, &mut notice);
                } else {
                    notice.clear();
                    let Tab { doc, view } = tabs.active_mut();
                    view.paste(doc, &text);
                }
            }
            Ok(AppEvent::Input(_)) => {}
        }

        if let Some(watcher) = &mut watcher {
            watcher.sync(tabs);
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
            let Tab { doc, view } = tabs.active_mut();
            let text_w = usize::from(width).saturating_sub(draw::gutter_width(doc));
            view.scroll_to_cursor(doc, text_w, draw::text_height(height));
        }
    }
    Ok(())
}

/// The debounce expired: reconcile every touched tab with its file. Paths
/// that match no tab — our own temp files, unrelated neighbours in a
/// watched directory — fall out here.
fn reload_changed(tabs: &mut Tabs, debounce: &mut Debounce) {
    for path in debounce.take() {
        let Some(index) = tabs.find_by_path(&path) else {
            continue;
        };
        let Tab { doc, view } = tabs.get_mut(index);
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
