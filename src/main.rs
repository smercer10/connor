mod doc;
mod draw;
mod grapheme;
mod prompt;
mod screen;
mod tabs;
mod term;
mod view;
mod watch;

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use doc::{Caret, DiskCheck, Document};
use prompt::{Outcome, PathPrompt};
use screen::Screen;
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
    if docs.is_empty() {
        docs.push(Document::empty());
    }
    let mut tabs = Tabs::new(docs);
    match run(&mut tabs) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("connor: {e}");
            ExitCode::FAILURE
        }
    }
}

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

/// Feeds one keypress to a pending prompt. Returns the prompt still pending
/// (unrecognized keys leave it up) and whether the editor should quit.
fn prompt_key(
    prompt: Prompt,
    key: &KeyEvent,
    tabs: &mut Tabs,
    notice: &mut String,
) -> (Option<Prompt>, bool) {
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

/// Closing the last tab is quitting; returns whether to quit.
fn close_active_or_quit(tabs: &mut Tabs) -> bool {
    if tabs.count() == 1 {
        return true;
    }
    tabs.close_active();
    false
}

fn run(tabs: &mut Tabs) -> io::Result<()> {
    term::init_panic_hook();
    let mut terminal = Terminal::new()?;
    let (width, height) = terminal.size();
    let mut back = Screen::new(width, height);
    let mut scratch = String::new();
    let mut notice = String::new();
    let mut prompt: Option<Prompt> = None;
    let (tx, rx) = mpsc::channel();
    watch::spawn_input_thread(tx.clone());
    // Losing the watcher (inotify limits, say) costs reloads, not the
    // editor.
    let mut watcher = match DirWatcher::new(tx) {
        Ok(watcher) => Some(watcher),
        Err(e) => {
            let _ = write!(notice, "file watching unavailable: {e}");
            None
        }
    };
    let mut debounce = Debounce::default();

    loop {
        back.clear();
        let status_caret =
            matches!(prompt, Some(Prompt::Path { .. })).then(|| notice.chars().count());
        let cursor = draw::draw(&mut back, tabs, &mut scratch, &notice, status_caret);
        terminal.present(&back, cursor)?;

        // Page movement wants the text area as it was when the key arrived.
        let text_h = draw::text_height(back.size().1);

        // A pending debounce turns the indefinite block into a timed one;
        // its expiry is the only wake that isn't a channel message.
        let received = match debounce.deadline() {
            None => rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected),
            Some(t) => rx.recv_timeout(t.saturating_duration_since(Instant::now())),
        };
        match received {
            Err(mpsc::RecvTimeoutError::Timeout) => reload_changed(tabs, &mut debounce),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other("event channel closed"));
            }
            Ok(AppEvent::InputFailed(e)) => return Err(e),
            Ok(AppEvent::Fs(path)) => debounce.note(path, Instant::now()),
            Ok(AppEvent::Input(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                // A pending prompt owns the key; the loop tail still runs,
                // because prompts are allowed to move the cursor.
                if let Some(pending) = prompt.take() {
                    let (next, quit) = prompt_key(pending, &key, tabs, &mut notice);
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
            Ok(AppEvent::Input(_)) => {}
        }

        if let Some(watcher) = &mut watcher {
            watcher.sync(tabs);
        }

        // Re-fetch the size: a resize may have changed it.
        let (width, height) = back.size();
        let Tab { doc, view } = tabs.active_mut();
        let text_w = usize::from(width).saturating_sub(draw::gutter_width(doc));
        view.scroll_to_cursor(doc, text_w, draw::text_height(height));
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
