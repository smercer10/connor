use std::io::{self, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::{Command as _, cursor, execute, terminal};

use crate::screen::Screen;

static ACTIVE: AtomicBool = AtomicBool::new(false);

// Button presses (?1000), drags while held (?1002) and SGR encoding (?1006),
// but not crossterm's EnableMouseCapture: that also turns on any-motion
// tracking (?1003), which would wake the loop and redraw on every pointer
// move. crossterm parses SGR reports regardless of how tracking was enabled.
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

/// Puts the terminal back the way the shell expects it. Idempotent, so the
/// panic hook and `Drop` can both call it without coordination, and it must
/// never panic — hence `let _ =` on every write.
fn restore() {
    if ACTIVE.swap(false, Ordering::SeqCst) {
        // End any pending synchronized update first: a panic between begin and
        // end must not leave the terminal holding back a buffered frame.
        let _ = execute!(
            io::stdout(),
            terminal::EndSynchronizedUpdate,
            DisableBracketedPaste,
            Print(DISABLE_MOUSE),
            terminal::LeaveAlternateScreen,
            cursor::Show
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// Chains the default hook so the panic message and backtrace print on the
/// normal screen after the terminal is restored. Call before `Terminal::new`.
pub fn init_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

/// Owns the terminal: raw mode and the alternate screen are entered on
/// construction and restored on drop.
pub struct Terminal {
    front: Screen,
    scratch: String,
    needs_clear: bool,
}

impl Terminal {
    pub fn new() -> io::Result<Self> {
        let (width, height) = terminal::size()?;
        let mut term = Self {
            front: Screen::new(width, height),
            scratch: String::new(),
            needs_clear: true,
        };
        term.reserve_scratch();
        term.enter()?;
        Ok(term)
    }

    pub fn size(&self) -> (u16, u16) {
        self.front.size()
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.front.resize(width, height);
        self.reserve_scratch();
        self.needs_clear = true;
    }

    /// Diffs `back` against what is on screen and writes only the changed
    /// runs, wrapped in a synchronized update and flushed as a single write,
    /// leaving the terminal cursor visible at `cursor`. Allocation-free:
    /// everything is formatted into the reusable scratch buffer, whose
    /// capacity is provisioned at construction and resize.
    pub fn present(&mut self, back: &Screen, cursor: (u16, u16)) -> io::Result<()> {
        let Self {
            front,
            scratch,
            needs_clear,
        } = self;
        scratch.clear();
        let _ = terminal::BeginSynchronizedUpdate.write_ansi(scratch);
        let _ = cursor::Hide.write_ansi(scratch);
        if *needs_clear {
            let _ = terminal::Clear(terminal::ClearType::All).write_ansi(scratch);
            *needs_clear = false;
        }
        // Styles are toggled only when they change and always switched off
        // by frame end, so each frame starts from a known plain state.
        let mut reversed = false;
        let mut underlined = false;
        back.for_each_changed_run(front, |x, y, run| {
            // A wide glyph changing always changes its leader, so a run can
            // never begin on a continuation cell.
            debug_assert!(!run[0].is_continuation());
            let _ = cursor::MoveTo(x, y).write_ansi(scratch);
            for cell in run {
                // The terminal advanced two columns at the leader; emitting
                // anything for the continuation would shift the row.
                if !cell.is_continuation() {
                    if cell.reversed() != reversed {
                        reversed = cell.reversed();
                        let attr = if reversed {
                            Attribute::Reverse
                        } else {
                            Attribute::NoReverse
                        };
                        let _ = SetAttribute(attr).write_ansi(scratch);
                    }
                    if cell.underlined() != underlined {
                        underlined = cell.underlined();
                        let attr = if underlined {
                            Attribute::Underlined
                        } else {
                            Attribute::NoUnderline
                        };
                        let _ = SetAttribute(attr).write_ansi(scratch);
                    }
                    scratch.push_str(cell.str());
                }
            }
        });
        if reversed {
            let _ = SetAttribute(Attribute::NoReverse).write_ansi(scratch);
        }
        if underlined {
            let _ = SetAttribute(Attribute::NoUnderline).write_ansi(scratch);
        }
        let _ = cursor::MoveTo(cursor.0, cursor.1).write_ansi(scratch);
        let _ = cursor::Show.write_ansi(scratch);
        let _ = terminal::EndSynchronizedUpdate.write_ansi(scratch);
        let mut out = io::stdout().lock();
        out.write_all(scratch.as_bytes())?;
        out.flush()?;
        front.copy_from(back);
        Ok(())
    }

    /// Writes a standalone escape sequence (e.g. an OSC 52 clipboard set)
    /// straight to the terminal, outside the frame diff. It touches no
    /// cells, so the front buffer stays valid.
    pub fn write_raw(&mut self, seq: &str) -> io::Result<()> {
        let mut out = io::stdout().lock();
        out.write_all(seq.as_bytes())?;
        out.flush()
    }

    fn enter(&mut self) -> io::Result<()> {
        // Mark active before touching the terminal so a partial entry (raw
        // mode on, alternate screen failed) is still restored by Drop.
        ACTIVE.store(true, Ordering::SeqCst);
        terminal::enable_raw_mode()?;
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            EnableBracketedPaste,
            Print(ENABLE_MOUSE),
            cursor::Hide
        )?;
        self.front.clear();
        self.needs_clear = true;
        Ok(())
    }

    fn reserve_scratch(&mut self) {
        let (width, height) = self.front.size();
        // 32 bytes per cell: the glyph plus cursor moves and the reverse and
        // underline toggles that can appear inside runs.
        let target = usize::from(width) * usize::from(height) * 32 + 1024;
        self.scratch
            .reserve(target.saturating_sub(self.scratch.len()));
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore();
    }
}
