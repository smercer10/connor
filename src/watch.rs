use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::tabs::Tabs;

/// Everything that can wake the main loop, carried on one channel so the
/// loop blocks on a single `recv` — never polling — while other threads
/// feed it.
pub enum AppEvent {
    Input(Event),
    /// The input thread hit a read error and exited; the terminal is gone.
    InputFailed(io::Error),
    /// A path inside a watched directory was touched; raw and un-debounced.
    Fs(PathBuf),
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

/// One save fires a burst of events within milliseconds; this coalesces
/// them. The window is fixed from the first event — a continuously writing
/// agent cannot push the flush out — and the main loop waits it out inside
/// `recv_timeout`, a timed block rather than a poll.
const WINDOW: Duration = Duration::from_millis(50);

#[derive(Default)]
pub struct Debounce {
    pending: Vec<PathBuf>,
    deadline: Option<Instant>,
}

impl Debounce {
    pub fn note(&mut self, path: PathBuf, now: Instant) {
        if !self.pending.contains(&path) {
            self.pending.push(path);
        }
        self.deadline.get_or_insert(now + WINDOW);
    }

    /// When the pending paths fall due; `None` when nothing is pending.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn take(&mut self) -> Vec<PathBuf> {
        self.deadline = None;
        std::mem::take(&mut self.pending)
    }
}

/// Watches the parent directories of open files, forwarding raw events
/// into the main loop's channel. Directories rather than the files
/// themselves: saves are temp-file-plus-rename, which replaces the inode a
/// file watch would stay bound to.
pub struct DirWatcher {
    inner: RecommendedWatcher,
    dirs: HashSet<PathBuf>,
    /// Tab paths as of the last sync — the syscall-free change detector.
    paths: Vec<Option<PathBuf>>,
}

impl DirWatcher {
    pub fn new(tx: Sender<AppEvent>) -> notify::Result<DirWatcher> {
        let inner = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(ev) = res else { return };
            if matches!(ev.kind, EventKind::Access(_)) {
                return;
            }
            for path in ev.paths {
                if tx.send(AppEvent::Fs(path)).is_err() {
                    return;
                }
            }
        })?;
        Ok(DirWatcher {
            inner,
            dirs: HashSet::new(),
            paths: Vec::new(),
        })
    }

    /// Reconciles the watched directories with the open tabs. Called every
    /// loop iteration so one call site covers every way a path can appear
    /// or vanish (open, close, save-as); almost always a no-op guarded by a
    /// lexical compare of tab paths against the last sync. Watch and
    /// unwatch failures are ignored — a directory that can't be watched
    /// just gets no reloads.
    pub fn sync(&mut self, tabs: &Tabs) {
        if self.paths.len() == tabs.count()
            && tabs
                .all()
                .iter()
                .zip(&self.paths)
                .all(|(tab, p)| tab.doc.path() == p.as_deref())
        {
            return;
        }
        self.paths = tabs
            .all()
            .iter()
            .map(|tab| tab.doc.path().map(Path::to_path_buf))
            .collect();
        let desired: HashSet<PathBuf> = self
            .paths
            .iter()
            .flatten()
            .filter_map(|path| watch_dir(path))
            .collect();
        for dir in self.dirs.difference(&desired) {
            let _ = self.inner.unwatch(dir);
        }
        for dir in desired.difference(&self.dirs) {
            let _ = self.inner.watch(dir, RecursiveMode::NonRecursive);
        }
        self.dirs = desired;
    }
}

/// The canonical directory a file's saves land in, symlinks resolved the
/// way saving resolves them, so the watch sits where writes actually
/// arrive. A missing file resolves within its parent.
fn watch_dir(path: &Path) -> Option<PathBuf> {
    match fs::canonicalize(path) {
        Ok(target) => target.parent().map(Path::to_path_buf),
        Err(_) => {
            let parent = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            fs::canonicalize(parent).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_note_arms_the_deadline_and_later_notes_do_not_extend_it() {
        let mut debounce = Debounce::default();
        let start = Instant::now();
        assert_eq!(debounce.deadline(), None);
        debounce.note(PathBuf::from("a"), start);
        assert_eq!(debounce.deadline(), Some(start + WINDOW));
        debounce.note(PathBuf::from("b"), start + WINDOW / 2);
        assert_eq!(debounce.deadline(), Some(start + WINDOW));
    }

    #[test]
    fn notes_dedupe_and_take_drains_and_disarms() {
        let mut debounce = Debounce::default();
        let now = Instant::now();
        debounce.note(PathBuf::from("a"), now);
        debounce.note(PathBuf::from("b"), now);
        debounce.note(PathBuf::from("a"), now);
        assert_eq!(
            debounce.take(),
            vec![PathBuf::from("a"), PathBuf::from("b")]
        );
        assert_eq!(debounce.deadline(), None);
        assert!(debounce.take().is_empty());
    }
}
