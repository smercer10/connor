//! The crash journal: dirty buffers snapshotted to the state directory so
//! a hard kill costs seconds of typing, not the work. Each running editor
//! owns one session directory whose `lock` file it holds an exclusive
//! flock on; the kernel drops the lock with the process, so at startup a
//! lock that can be taken marks a crashed session whose entries are
//! recovered.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ropey::Rope;

use crate::tabs::Tabs;

/// A changed dirty buffer reaches disk this long after its first
/// unjournaled change. Fixed from that first change, so continuous typing
/// cannot push the snapshot out.
const INTERVAL: Duration = Duration::from_secs(3);

/// One buffer pulled out of a crashed session's journal.
pub struct Recovered {
    pub path: Option<PathBuf>,
    pub text: String,
}

enum Cmd {
    Write {
        id: u64,
        path: Option<PathBuf>,
        rope: Rope,
    },
    Remove {
        id: u64,
    },
    /// Clean exit: remove the whole session directory.
    Shutdown,
}

/// The command planner: which buffer revisions the session directory holds
/// and when the next write falls due. Pure bookkeeping — all IO happens on
/// the writer thread it feeds.
#[derive(Default)]
struct Ledger {
    /// Last journaled revision per document id.
    entries: HashMap<u64, u64>,
    deadline: Option<Instant>,
}

impl Ledger {
    /// Reconciles with the open tabs: entries for clean or closed
    /// documents come back as removals at once, and the deadline arms
    /// while any dirty document has moved past its journaled revision.
    fn sync(&mut self, tabs: &Tabs, now: Instant) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        self.entries.retain(|&id, _| {
            let keep = tabs
                .all()
                .iter()
                .any(|tab| tab.doc.id() == id && tab.doc.dirty());
            if !keep {
                cmds.push(Cmd::Remove { id });
            }
            keep
        });
        if self.pending(tabs) {
            self.deadline.get_or_insert(now + INTERVAL);
        } else {
            self.deadline = None;
        }
        cmds
    }

    /// Snapshots every dirty document that moved since its last write.
    fn flush(&mut self, tabs: &Tabs) -> Vec<Cmd> {
        let mut cmds = Vec::new();
        for tab in tabs.all() {
            let doc = &tab.doc;
            if doc.dirty() && self.entries.get(&doc.id()) != Some(&doc.revision()) {
                self.entries.insert(doc.id(), doc.revision());
                cmds.push(Cmd::Write {
                    id: doc.id(),
                    path: doc.path().map(Path::to_path_buf),
                    rope: doc.rope().clone(),
                });
            }
        }
        self.deadline = None;
        cmds
    }

    fn pending(&self, tabs: &Tabs) -> bool {
        tabs.all().iter().any(|tab| {
            tab.doc.dirty() && self.entries.get(&tab.doc.id()) != Some(&tab.doc.revision())
        })
    }
}

/// The main loop's handle on journaling. Disabled (every method a no-op)
/// when the state directory or session lock could not be set up — the
/// editor works, it just is not protected.
pub struct Journal {
    inner: Option<Active>,
}

struct Active {
    tx: Sender<Cmd>,
    handle: JoinHandle<()>,
    ledger: Ledger,
    /// Raised by the writer thread on its first failed write.
    failed: Arc<AtomicBool>,
    warned: bool,
}

impl Journal {
    /// Recovers any crashed sessions' journals and opens this session's
    /// own. The notice, when present, says why journaling is off.
    pub fn start() -> (Journal, Vec<Recovered>, Option<String>) {
        start_in(resolve_state_dir(
            std::env::var_os("XDG_STATE_HOME"),
            std::env::var_os("HOME"),
        ))
    }

    /// When the next snapshot falls due; `None` while nothing is pending.
    pub fn deadline(&self) -> Option<Instant> {
        self.inner
            .as_ref()
            .and_then(|active| active.ledger.deadline)
    }

    /// Loop-tail reconcile: stale entries are removed the same iteration
    /// their document became clean or closed, and a failed write surfaces
    /// once into an empty notice.
    pub fn sync(&mut self, tabs: &Tabs, now: Instant, notice: &mut String) {
        let Some(active) = &mut self.inner else {
            return;
        };
        for cmd in active.ledger.sync(tabs, now) {
            if active.tx.send(cmd).is_err() {
                active.failed.store(true, Ordering::Relaxed);
            }
        }
        if !active.warned && notice.is_empty() && active.failed.load(Ordering::Relaxed) {
            notice.push_str("crash journal failed — unsaved work is not protected");
            active.warned = true;
        }
    }

    /// Hands every changed dirty buffer to the writer thread.
    pub fn flush(&mut self, tabs: &Tabs) {
        let Some(active) = &mut self.inner else {
            return;
        };
        for cmd in active.ledger.flush(tabs) {
            if active.tx.send(cmd).is_err() {
                active.failed.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Ends the session: with `remove` (a clean exit) the writer deletes
    /// the session directory; without it the directory survives for the
    /// next start to recover. Joins so queued writes land before the
    /// process exits.
    pub fn finish(self, remove: bool) {
        let Some(active) = self.inner else { return };
        if remove {
            let _ = active.tx.send(Cmd::Shutdown);
        }
        drop(active.tx);
        let _ = active.handle.join();
    }
}

/// XDG state resolution: `$XDG_STATE_HOME/connor`, else
/// `$HOME/.local/state/connor`, else nothing to journal into.
fn resolve_state_dir(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(xdg).join("connor"));
    }
    let home = home.filter(|v| !v.is_empty())?;
    Some(PathBuf::from(home).join(".local/state/connor"))
}

fn start_in(state_dir: Option<PathBuf>) -> (Journal, Vec<Recovered>, Option<String>) {
    let disabled = |why: String| (Journal { inner: None }, Vec::new(), Some(why));
    let Some(state_dir) = state_dir else {
        return disabled("crash journal disabled: no home directory".to_owned());
    };
    let journal_dir = state_dir.join("journal");
    if let Err(e) = fs::create_dir_all(&journal_dir) {
        return disabled(format!("crash journal disabled: {e}"));
    }
    // Journals hold file contents; keep them to their owner. Best-effort.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o700));
    }
    let recovered = harvest(&journal_dir);
    match create_session(&journal_dir) {
        Ok((dir, lock)) => {
            let failed = Arc::new(AtomicBool::new(false));
            let (tx, handle) = spawn_writer(dir, lock, failed.clone());
            let journal = Journal {
                inner: Some(Active {
                    tx,
                    handle,
                    ledger: Ledger::default(),
                    failed,
                    warned: false,
                }),
            };
            (journal, recovered, None)
        }
        // The harvest already happened and its content must not be
        // dropped: recovery still reaches the user as dirty buffers, only
        // re-journaling is off.
        Err(e) => (
            Journal { inner: None },
            recovered,
            Some(format!("crash journal disabled: {e}")),
        ),
    }
}

/// Scans the journal root for crashed sessions — those whose `lock` this
/// process can take. Every entry that parses is collected and deleted; one
/// that does not is left in place (with its directory) for the next start,
/// so a recovery hiccup never destroys the only copy.
fn harvest(journal_dir: &Path) -> Vec<Recovered> {
    let mut recovered = Vec::new();
    let Ok(sessions) = fs::read_dir(journal_dir) else {
        return recovered;
    };
    for session in sessions.flatten() {
        let dir = session.path();
        if !dir.is_dir() {
            continue;
        }
        // No lock file yet: an instance is mid-startup, microseconds from
        // locking. A held lock: a live instance. Both are skipped.
        let Ok(lock) = File::open(dir.join("lock")) else {
            continue;
        };
        if lock.try_lock().is_err() {
            continue;
        }
        let mut leftovers = false;
        if let Ok(items) = fs::read_dir(&dir) {
            for item in items.flatten() {
                let path = item.path();
                let name = item.file_name();
                let name = name.to_string_lossy();
                if name == "lock" {
                    continue;
                }
                if !name.starts_with("entry-") {
                    // A temp file the crash interrupted; the rename never
                    // happened, so the previous full entry still stands.
                    let _ = fs::remove_file(&path);
                    continue;
                }
                match fs::read(&path).ok().as_deref().and_then(parse_entry) {
                    Some(entry) => {
                        recovered.push(entry);
                        let _ = fs::remove_file(&path);
                    }
                    None => leftovers = true,
                }
            }
        }
        drop(lock);
        if !leftovers {
            let _ = fs::remove_dir_all(&dir);
        }
    }
    recovered
}

/// Creates and locks this session's directory, named by pid with a suffix
/// to step over a crashed predecessor's leftovers under the same pid.
fn create_session(journal_dir: &Path) -> io::Result<(PathBuf, File)> {
    let pid = std::process::id();
    let mut attempt = 0;
    loop {
        let name = if attempt == 0 {
            pid.to_string()
        } else {
            format!("{pid}-{attempt}")
        };
        let dir = journal_dir.join(name);
        match fs::create_dir(&dir) {
            Ok(()) => {
                let lock = File::create(dir.join("lock"))?;
                lock.try_lock().map_err(io::Error::other)?;
                return Ok((dir, lock));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attempt < 100 => attempt += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Owns all journal IO on its own thread, so an fsync never stalls a
/// frame. A dropped sender (panic, error exit) ends the thread with the
/// directory intact — that is the crash the journal exists for. The lock
/// file rides along so the flock outlives the main thread's teardown.
fn spawn_writer(
    dir: PathBuf,
    lock: File,
    failed: Arc<AtomicBool>,
) -> (Sender<Cmd>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        while let Ok(cmd) = rx.recv() {
            match cmd {
                Cmd::Write { id, path, rope } => {
                    if write_entry(&dir, id, path.as_deref(), &rope).is_err() {
                        failed.store(true, Ordering::Relaxed);
                    }
                }
                Cmd::Remove { id } => {
                    let _ = fs::remove_file(dir.join(format!("entry-{id}")));
                }
                Cmd::Shutdown => {
                    let _ = fs::remove_dir_all(&dir);
                    break;
                }
            }
        }
        drop(lock);
    });
    (tx, handle)
}

/// One entry file: the buffer's path on the first line (empty when it has
/// none), the exact buffer content after it. Temp-plus-rename in the
/// session directory, so a kill mid-write leaves the previous snapshot.
fn write_entry(dir: &Path, id: u64, path: Option<&Path>, rope: &Rope) -> io::Result<()> {
    let temp = dir.join(format!(".entry-{id}.tmp"));
    let file = File::create(&temp)?;
    let mut writer = BufWriter::new(file);
    // A path containing a newline cannot survive a line-based header; the
    // content still does, recovered as a pathless buffer.
    let header = path.map(header_bytes).filter(|b| !b.contains(&b'\n'));
    writer.write_all(header.as_deref().unwrap_or_default())?;
    writer.write_all(b"\n")?;
    rope.write_to(&mut writer)?;
    let file = writer
        .into_inner()
        .map_err(io::IntoInnerError::into_error)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temp, dir.join(format!("entry-{id}")))
}

fn parse_entry(bytes: &[u8]) -> Option<Recovered> {
    let split = bytes.iter().position(|&b| b == b'\n')?;
    let text = String::from_utf8(bytes[split + 1..].to_vec()).ok()?;
    let header = &bytes[..split];
    let path = (!header.is_empty()).then(|| header_path(header));
    Some(Recovered { path, text })
}

#[cfg(unix)]
fn header_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(unix)]
fn header_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes.to_vec()))
}

#[cfg(not(unix))]
fn header_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(not(unix))]
fn header_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::doc::{Caret, Document, EditKind};

    /// A fresh scratch directory per test: tests run in parallel, so each
    /// needs its own.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("connor-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn edit(doc: &mut Document, text: &str) {
        let caret = Caret {
            cursor: 0,
            anchor: None,
        };
        doc.edit(0..0, text, caret, EditKind::Other);
    }

    #[test]
    fn state_dir_prefers_xdg_and_falls_back_to_home() {
        let xdg = Some(OsString::from("/xdg"));
        let home = Some(OsString::from("/home/u"));
        assert_eq!(
            resolve_state_dir(xdg.clone(), home.clone()),
            Some(PathBuf::from("/xdg/connor"))
        );
        assert_eq!(
            resolve_state_dir(None, home.clone()),
            Some(PathBuf::from("/home/u/.local/state/connor"))
        );
        // An empty variable is as good as unset.
        assert_eq!(
            resolve_state_dir(Some(OsString::new()), home),
            Some(PathBuf::from("/home/u/.local/state/connor"))
        );
        assert_eq!(resolve_state_dir(None, None), None);
    }

    #[test]
    fn entry_roundtrips_path_and_exact_content() {
        let dir = scratch_dir("journal-roundtrip");
        let rope = Rope::from_str("a\r\nb\nno trailing newline");
        write_entry(&dir, 7, Some(Path::new("/tmp/some file.txt")), &rope).unwrap();
        let entry = parse_entry(&fs::read(dir.join("entry-7")).unwrap()).unwrap();
        assert_eq!(entry.path.as_deref(), Some(Path::new("/tmp/some file.txt")));
        assert_eq!(entry.text, "a\r\nb\nno trailing newline");
        // No temp file left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pathless_and_empty_buffers_roundtrip() {
        let dir = scratch_dir("journal-pathless");
        write_entry(&dir, 1, None, &Rope::new()).unwrap();
        let entry = parse_entry(&fs::read(dir.join("entry-1")).unwrap()).unwrap();
        assert_eq!(entry.path, None);
        assert_eq!(entry.text, "");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_containing_a_newline_survives_as_pathless_content() {
        let dir = scratch_dir("journal-nl-path");
        let rope = Rope::from_str("content");
        write_entry(&dir, 1, Some(Path::new("bad\nname")), &rope).unwrap();
        let entry = parse_entry(&fs::read(dir.join("entry-1")).unwrap()).unwrap();
        assert_eq!(entry.path, None);
        assert_eq!(entry.text, "content");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_rejects_a_headerless_or_non_utf8_entry() {
        assert!(parse_entry(b"no newline at all").is_none());
        assert!(parse_entry(b"/tmp/x\nbad \xFF bytes").is_none());
    }

    #[test]
    fn harvest_recovers_an_unlocked_session_and_removes_it() {
        let root = scratch_dir("journal-harvest");
        let session = root.join("12345");
        fs::create_dir(&session).unwrap();
        File::create(session.join("lock")).unwrap();
        fs::write(session.join("entry-1"), b"/tmp/a\none").unwrap();
        fs::write(session.join("entry-2"), b"\ntwo").unwrap();
        fs::write(session.join(".entry-3.tmp"), b"partial").unwrap();
        let mut recovered = harvest(&root);
        recovered.sort_by(|a, b| a.text.cmp(&b.text));
        assert_eq!(recovered.len(), 2);
        assert_eq!(recovered[0].path.as_deref(), Some(Path::new("/tmp/a")));
        assert_eq!(recovered[0].text, "one");
        assert_eq!(recovered[1].path, None);
        assert_eq!(recovered[1].text, "two");
        assert!(!session.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn harvest_skips_a_live_session() {
        let root = scratch_dir("journal-live");
        let (session, lock) = create_session(&root).unwrap();
        fs::write(session.join("entry-1"), b"/tmp/a\nlive work").unwrap();
        // The flock is held per open description, so this process's own
        // lock stands in for another live editor's.
        assert!(harvest(&root).is_empty());
        assert!(session.join("entry-1").exists());
        drop(lock);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn harvest_skips_a_session_without_a_lock_file() {
        let root = scratch_dir("journal-no-lock");
        let session = root.join("999");
        fs::create_dir(&session).unwrap();
        fs::write(session.join("entry-1"), b"/tmp/a\nmid-startup").unwrap();
        assert!(harvest(&root).is_empty());
        assert!(session.join("entry-1").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn harvest_keeps_a_malformed_entry_for_the_next_start() {
        let root = scratch_dir("journal-malformed");
        let session = root.join("777");
        fs::create_dir(&session).unwrap();
        File::create(session.join("lock")).unwrap();
        fs::write(session.join("entry-1"), b"/tmp/a\ngood").unwrap();
        fs::write(session.join("entry-2"), b"headerless").unwrap();
        let recovered = harvest(&root);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].text, "good");
        assert!(!session.join("entry-1").exists());
        assert!(session.join("entry-2").exists());
        assert!(session.join("lock").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sessions_get_distinct_locked_directories() {
        let root = scratch_dir("journal-session");
        let (first, _lock_a) = create_session(&root).unwrap();
        let (second, _lock_b) = create_session(&root).unwrap();
        assert_ne!(first, second);
        assert!(first.join("lock").exists());
        assert!(second.join("lock").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn ledger_arms_once_and_writes_on_flush() {
        let mut doc = Document::empty();
        edit(&mut doc, "a");
        let mut tabs = Tabs::new(vec![doc]);
        let mut ledger = Ledger::default();
        let now = Instant::now();
        assert!(ledger.sync(&tabs, now).is_empty());
        assert_eq!(ledger.deadline, Some(now + INTERVAL));
        // More typing must not push the snapshot out.
        edit(&mut tabs.active_mut().doc, "b");
        ledger.sync(&tabs, now + INTERVAL / 2);
        assert_eq!(ledger.deadline, Some(now + INTERVAL));
        let cmds = ledger.flush(&tabs);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Cmd::Write { .. }));
        assert_eq!(ledger.deadline, None);
        // Nothing changed since: quiet on both paths.
        assert!(ledger.flush(&tabs).is_empty());
        assert!(ledger.sync(&tabs, now).is_empty());
        assert_eq!(ledger.deadline, None);
    }

    #[test]
    fn ledger_removes_an_entry_when_its_document_comes_clean() {
        let mut doc = Document::empty();
        edit(&mut doc, "a");
        let id = doc.id();
        let mut tabs = Tabs::new(vec![doc]);
        let mut ledger = Ledger::default();
        ledger.flush(&tabs);
        tabs.active_mut().doc.undo();
        let cmds = ledger.sync(&tabs, Instant::now());
        assert!(matches!(cmds[..], [Cmd::Remove { id: got }] if got == id));
        // Redo makes it dirty at a new revision: journaled again.
        tabs.active_mut().doc.redo();
        let now = Instant::now();
        assert!(ledger.sync(&tabs, now).is_empty());
        assert_eq!(ledger.deadline, Some(now + INTERVAL));
        assert_eq!(ledger.flush(&tabs).len(), 1);
    }

    #[test]
    fn ledger_removes_the_entry_of_a_closed_tab() {
        let mut kept = Document::empty();
        edit(&mut kept, "kept");
        let mut discarded = Document::empty();
        edit(&mut discarded, "discarded");
        let id = discarded.id();
        let mut tabs = Tabs::new(vec![kept, discarded]);
        let mut ledger = Ledger::default();
        assert_eq!(ledger.flush(&tabs).len(), 2);
        tabs.activate(1);
        tabs.close_active();
        let cmds = ledger.sync(&tabs, Instant::now());
        assert!(matches!(cmds[..], [Cmd::Remove { id: got }] if got == id));
    }

    #[test]
    fn writer_persists_entries_until_a_clean_shutdown_removes_them() {
        let root = scratch_dir("journal-writer");
        let (dir, lock) = create_session(&root).unwrap();
        let failed = Arc::new(AtomicBool::new(false));
        let (tx, handle) = spawn_writer(dir.clone(), lock, failed.clone());
        tx.send(Cmd::Write {
            id: 1,
            path: None,
            rope: Rope::from_str("survives"),
        })
        .unwrap();
        // A dropped sender is a crash: the entry and directory survive.
        drop(tx);
        handle.join().unwrap();
        assert!(dir.join("entry-1").exists());
        assert!(!failed.load(Ordering::Relaxed));

        let lock = File::open(dir.join("lock")).unwrap();
        lock.try_lock().unwrap();
        let (tx, handle) = spawn_writer(dir.clone(), lock, failed.clone());
        tx.send(Cmd::Remove { id: 1 }).unwrap();
        tx.send(Cmd::Shutdown).unwrap();
        handle.join().unwrap();
        assert!(!dir.exists());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_write_raises_the_flag_and_the_writer_carries_on() {
        let root = scratch_dir("journal-fail");
        let missing = root.join("missing");
        let lock = File::create(root.join("lock")).unwrap();
        let failed = Arc::new(AtomicBool::new(false));
        let (tx, handle) = spawn_writer(missing, lock, failed.clone());
        tx.send(Cmd::Write {
            id: 1,
            path: None,
            rope: Rope::from_str("x"),
        })
        .unwrap();
        drop(tx);
        handle.join().unwrap();
        assert!(failed.load(Ordering::Relaxed));
        fs::remove_dir_all(&root).unwrap();
    }
}
