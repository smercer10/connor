// Wired up over the next few commits.
#[allow(dead_code)]
mod doc;
#[allow(dead_code)]
mod grapheme;
mod screen;
mod term;
#[allow(dead_code)]
mod view;

use std::fmt::Write as _;
use std::io;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use screen::Screen;
use term::Terminal;

fn main() -> io::Result<()> {
    term::init_panic_hook();
    let mut terminal = Terminal::new()?;
    let (width, height) = terminal.size();
    let mut back = Screen::new(width, height);
    let mut size_label = String::new();
    let mut last_key = String::new();
    write_size_label(&mut size_label, width, height);

    loop {
        back.clear();
        draw_scene(&mut back, &size_label, &last_key);
        terminal.present(&back, (0, 0))?;

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
                        write_size_label(&mut size_label, width, height);
                    }
                    #[cfg(debug_assertions)]
                    KeyCode::Char('p') if ctrl => panic!("deliberate panic (Ctrl+P)"),
                    _ => describe_key(&mut last_key, &key),
                }
            }
            Event::Resize(width, height) => {
                terminal.resize(width, height);
                back.resize(width, height);
                write_size_label(&mut size_label, width, height);
            }
            _ => {}
        }
    }
    Ok(())
}

fn write_size_label(label: &mut String, width: u16, height: u16) {
    label.clear();
    let _ = write!(label, "{width}x{height}");
}

fn describe_key(label: &mut String, key: &KeyEvent) {
    label.clear();
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        label.push_str("Ctrl+");
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        label.push_str("Alt+");
    }
    if key.modifiers.contains(KeyModifiers::SHIFT) {
        label.push_str("Shift+");
    }
    let _ = write!(label, "{}", key.code);
}

/// Draws the placeholder frame: a border, the current size, and the last key
/// pressed (a small localized change that exercises the diff). Clips rather
/// than panics at degenerate sizes.
fn draw_scene(buf: &mut Screen, size_label: &str, last_key: &str) {
    let (width, height) = buf.size();
    if width == 0 || height == 0 {
        return;
    }
    let right = width - 1;
    let bottom = height - 1;
    for x in 0..width {
        buf.set(x, 0, '─');
        buf.set(x, bottom, '─');
    }
    for y in 0..height {
        buf.set(0, y, '│');
        buf.set(right, y, '│');
    }
    buf.set(0, 0, '┌');
    buf.set(right, 0, '┐');
    buf.set(0, bottom, '└');
    buf.set(right, bottom, '┘');

    let mid = height / 2;
    set_centered(buf, mid.saturating_sub(2), "connor");
    set_centered(buf, mid, size_label);
    if !last_key.is_empty() {
        let prefix = "last key: ";
        let len = prefix.chars().count() + last_key.chars().count();
        let x = centered_x(width, len);
        buf.set_text(x, mid + 2, prefix);
        buf.set_text(
            x.saturating_add(prefix.chars().count() as u16),
            mid + 2,
            last_key,
        );
    }
    set_centered(
        buf,
        bottom.saturating_sub(1),
        "Ctrl+Q quit · Ctrl+Z suspend",
    );
}

fn set_centered(buf: &mut Screen, y: u16, text: &str) {
    let (width, _) = buf.size();
    buf.set_text(centered_x(width, text.chars().count()), y, text);
}

fn centered_x(width: u16, len: usize) -> u16 {
    let space = usize::from(width).saturating_sub(len) / 2;
    u16::try_from(space).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(buf: &Screen, x: u16, y: u16) -> char {
        buf.get(x, y).unwrap().str().chars().next().unwrap()
    }

    fn row(buf: &Screen, y: u16) -> String {
        let (width, _) = buf.size();
        (0..width).map(|x| cell(buf, x, y)).collect()
    }

    #[test]
    fn border_corners_and_edges() {
        let mut buf = Screen::new(20, 10);
        draw_scene(&mut buf, "20x10", "");
        assert_eq!(cell(&buf, 0, 0), '┌');
        assert_eq!(cell(&buf, 19, 0), '┐');
        assert_eq!(cell(&buf, 0, 9), '└');
        assert_eq!(cell(&buf, 19, 9), '┘');
        assert_eq!(cell(&buf, 10, 0), '─');
        assert_eq!(cell(&buf, 10, 9), '─');
        assert_eq!(cell(&buf, 0, 5), '│');
        assert_eq!(cell(&buf, 19, 5), '│');
    }

    #[test]
    fn title_size_and_hint_are_placed() {
        let mut buf = Screen::new(40, 12);
        draw_scene(&mut buf, "40x12", "");
        assert!(row(&buf, 4).contains("connor"));
        assert!(row(&buf, 6).contains("40x12"));
        assert!(row(&buf, 10).contains("Ctrl+Q quit"));
    }

    #[test]
    fn last_key_readout_appears() {
        let mut buf = Screen::new(40, 12);
        draw_scene(&mut buf, "40x12", "Ctrl+X");
        assert!(row(&buf, 8).contains("last key: Ctrl+X"));
    }

    #[test]
    fn degenerate_sizes_do_not_panic() {
        for (w, h) in [(0, 0), (0, 5), (5, 0), (1, 1), (2, 2), (3, 1), (1, 3)] {
            let mut buf = Screen::new(w, h);
            draw_scene(&mut buf, "1x1", "Ctrl+X");
        }
    }

    // KeyCode's Display names vary by platform (Enter is "Return" on macOS),
    // so only the modifier prefixing — the part we own — is pinned here.
    #[test]
    fn describe_key_formats_modifiers_and_code() {
        let mut label = String::new();
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        describe_key(&mut label, &key);
        assert_eq!(label, "Ctrl+x");
        let key = KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE);
        describe_key(&mut label, &key);
        assert_eq!(label, "x");
    }
}
