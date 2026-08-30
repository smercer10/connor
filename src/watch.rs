use std::io;
use std::sync::mpsc::Sender;
use std::thread;

use crossterm::event::{self, Event};

/// Everything that can wake the main loop, carried on one channel so the
/// loop blocks on a single `recv` — never polling — while other threads
/// feed it.
pub enum AppEvent {
    Input(Event),
    /// The input thread hit a read error and exited; the terminal is gone.
    InputFailed(io::Error),
}

/// Forwards terminal input into `tx` from a dedicated thread that owns no
/// editor state. It exits when reading fails or the receiver drops; at quit
/// it may still be parked in `read`, and process exit reaps it.
pub fn spawn_input_thread(tx: Sender<AppEvent>) {
    thread::spawn(move || {
        loop {
            let msg = match event::read() {
                Ok(ev) => AppEvent::Input(ev),
                Err(e) => {
                    let _ = tx.send(AppEvent::InputFailed(e));
                    return;
                }
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
}
