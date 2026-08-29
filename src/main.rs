mod doc;
mod draw;
mod grapheme;
mod screen;
mod term;
mod view;

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};

use doc::Document;
use screen::Screen;
use term::Terminal;
use view::View;

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let _ = args.next();
    let path = args.next().map(PathBuf::from);
    if args.next().is_some() {
        eprintln!("usage: connor [FILE]");
        return ExitCode::FAILURE;
    }
    // Open before touching the terminal so errors print on the normal screen.
    let doc = match path {
        Some(path) => match Document::open(path.clone()) {
            Ok(doc) => doc,
            Err(e) => {
                eprintln!("connor: {}: {e}", path.display());
                return ExitCode::FAILURE;
            }
        },
        None => Document::empty(),
    };
    match run(&doc) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("connor: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(doc: &Document) -> io::Result<()> {
    term::init_panic_hook();
    let mut terminal = Terminal::new()?;
    let (width, height) = terminal.size();
    let mut back = Screen::new(width, height);
    let mut view = View::default();
    let mut scratch = String::new();

    loop {
        back.clear();
        let cursor = draw::draw(&mut back, doc, &view, &mut scratch);
        terminal.present(&back, cursor)?;

        // Page movement wants the text area as it was when the key arrived.
        let text_h = draw::text_height(back.size().1);

        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                match key.code {
                    KeyCode::Char('q') if ctrl => break,
                    #[cfg(unix)]
                    KeyCode::Char('z') if ctrl => {
                        terminal.suspend()?;
                        let (width, height) = terminal.size();
                        back.resize(width, height);
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
                    _ => {}
                }
            }
            Event::Resize(width, height) => {
                terminal.resize(width, height);
                back.resize(width, height);
            }
            _ => {}
        }

        // Re-fetch the size: resize and suspend may have changed it.
        let (width, height) = back.size();
        let text_w = usize::from(width).saturating_sub(draw::gutter_width(doc));
        view.scroll_to_cursor(doc, text_w, draw::text_height(height));
    }
    Ok(())
}
