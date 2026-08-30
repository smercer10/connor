mod doc;
mod draw;
mod grapheme;
mod screen;
mod tabs;
mod term;
mod view;

use std::fmt::Write as _;
use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use doc::Document;
use screen::Screen;
use tabs::{Tab, Tabs};
use term::Terminal;

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

/// A mini-prompt owning the next keypress; its question sits in the notice.
enum Prompt {
    /// Ctrl+Q with unsaved changes: save / discard / cancel.
    Quit,
    /// The file loaded lossily, so writing it back mangles the bytes the
    /// U+FFFD marks stand for — overwriting must be deliberate.
    LossySave { then_quit: bool },
}

fn open_prompt(prompt: Prompt, doc: &Document, notice: &mut String) -> Option<Prompt> {
    notice.clear();
    match prompt {
        Prompt::Quit => {
            notice.push_str("unsaved changes — save before quitting? (y)es · (n)o · (esc) cancel");
        }
        Prompt::LossySave { .. } => {
            let _ = write!(
                notice,
                "invalid UTF-8 was replaced on load — overwrite {}? (y)es · (esc) cancel",
                doc.name()
            );
        }
    }
    Some(prompt)
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
    key: KeyCode,
    doc: &mut Document,
    notice: &mut String,
) -> (Option<Prompt>, bool) {
    match (prompt, key) {
        (Prompt::Quit, KeyCode::Char('y' | 'Y')) => {
            if doc.lossy() {
                (
                    open_prompt(Prompt::LossySave { then_quit: true }, doc, notice),
                    false,
                )
            } else {
                // A failed save cancels the quit: the error stays visible
                // and the changes stay alive.
                (None, try_save(doc, notice))
            }
        }
        (Prompt::Quit, KeyCode::Char('n' | 'N')) => (None, true),
        (Prompt::LossySave { then_quit }, KeyCode::Char('y' | 'Y')) => {
            (None, try_save(doc, notice) && then_quit)
        }
        (_, KeyCode::Esc) => {
            notice.clear();
            (None, false)
        }
        (prompt, _) => (Some(prompt), false),
    }
}

fn run(tabs: &mut Tabs) -> io::Result<()> {
    term::init_panic_hook();
    let mut terminal = Terminal::new()?;
    let (width, height) = terminal.size();
    let mut back = Screen::new(width, height);
    let mut scratch = String::new();
    let mut notice = String::new();
    let mut prompt: Option<Prompt> = None;

    loop {
        back.clear();
        let cursor = draw::draw(&mut back, tabs, &mut scratch, &notice);
        terminal.present(&back, cursor)?;

        // Page movement wants the text area as it was when the key arrived.
        let text_h = draw::text_height(back.size().1);

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let Tab { doc, view } = tabs.active_mut();
                if let Some(pending) = prompt.take() {
                    let (next, quit) = prompt_key(pending, key.code, doc, &mut notice);
                    prompt = next;
                    if quit {
                        break;
                    }
                    continue;
                }
                notice.clear();
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
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
                    view.begin_or_clear_selection(key.modifiers.contains(KeyModifiers::SHIFT));
                    doc.break_undo_group();
                }
                match key.code {
                    KeyCode::Char('q') if ctrl => {
                        if doc.dirty() {
                            prompt = open_prompt(Prompt::Quit, doc, &mut notice);
                        } else {
                            break;
                        }
                    }
                    KeyCode::Char('s') if ctrl => {
                        if doc.lossy() {
                            prompt = open_prompt(
                                Prompt::LossySave { then_quit: false },
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
                    KeyCode::Char(ch) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => {
                        view.insert_char(doc, ch)
                    }
                    KeyCode::Enter => view.insert_newline(doc),
                    KeyCode::Tab => view.insert_tab(doc),
                    KeyCode::Backspace => view.backspace(doc),
                    KeyCode::Delete => view.delete(doc),
                    _ => {}
                }
            }
            Event::Resize(width, height) => {
                terminal.resize(width, height);
                back.resize(width, height);
            }
            _ => {}
        }

        // Re-fetch the size: a resize may have changed it.
        let (width, height) = back.size();
        let Tab { doc, view } = tabs.active_mut();
        let text_w = usize::from(width).saturating_sub(draw::gutter_width(doc));
        view.scroll_to_cursor(doc, text_w, draw::text_height(height));
    }
    Ok(())
}
