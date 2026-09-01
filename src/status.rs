//! The project against HEAD: which files differ and which are new, marked
//! on tree rows and tab labels. One `git status` on a worker, coalesced
//! behind a refresh window, so a build storming the watcher costs one scan
//! per settle rather than one per event. No `git`, no repository, or a
//! changeset too large to review all mean no marks, never an error.

use std::io::Read as _;
use std::path::{MAIN_SEPARATOR, MAIN_SEPARATOR_STR, Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use crate::diff;
use crate::project;
use crate::tabs::{self, Tabs};
use crate::watch::AppEvent;

/// The dot marking a file, and a directory holding one — `▍` marks a line,
/// `●` marks a file.
pub const DOT: char = '●';

/// Bytes of `git status` output read before the pipe closes on the child.
/// Far past what `MAX_FILES` can spend, so the file cap is what actually
/// bounds a result; this only stops a runaway child.
const MAX_BYTES: usize = diff::MAX_LEN;

/// Files past which a scan reports nothing: a changeset this size is not
/// being reviewed row by row, and marking every row marks nothing.
const MAX_FILES: usize = 10_000;

/// What a file says about HEAD. A directory carries the fold of its
/// descendants'.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mark {
    /// Not in HEAD: untracked, or added to the index.
    New,
    Changed,
}

impl Mark {
    /// A `Cell::fg` code, from `Change`'s palette so a file and its lines
    /// read the same: dark green for new, dark yellow for changed.
    pub fn color(self) -> u8 {
        match self {
            Mark::New => 3,
            Mark::Changed => 4,
        }
    }

    /// A directory's mark from its descendants': wholly new reads new,
    /// anything mixed reads changed.
    fn merge(self, other: Mark) -> Mark {
        if self == other { self } else { Mark::Changed }
    }
}

/// A finished scan on its way back to the main loop.
pub struct ScanDone {
    generation: u64,
    /// Root-relative paths with their marks, sorted; `None` when the scan
    /// found no answer to give — no `git`, or a changeset past the cap.
    entries: Option<Vec<(Box<str>, Mark)>>,
}

/// The project's marked files, and the tab labels that mirror them.
pub struct Status {
    /// Whether the project root is a repository, decided once with no
    /// subprocess. False keeps this inert for the whole session: no `git`
    /// ever runs, and the sidebar is exactly as wide as it was before any
    /// of this existed.
    in_repo: bool,
    /// Canonical, so stripping it from a canonicalized tab path works —
    /// `Tree` resolves its own the same way.
    root: PathBuf,
    /// Marked files plus their ancestor directories, root-relative with
    /// native separators, sorted — so a lookup is a binary search and a
    /// collapsed directory costs one, not a scan of its subtree.
    entries: Vec<(Box<str>, Mark)>,
    /// Bumped only when `entries` actually changes, so a re-scan finding
    /// the same set costs neither a repaint nor a tab resync.
    set_gen: u64,
    /// Tags background work so a superseded result is dropped on arrival.
    generation: u64,
    inflight: bool,
    /// The set has never been read, or may have moved since it was.
    stale: bool,
    /// What the last tab sync saw — the syscall-free change detector.
    synced_gen: u64,
    synced: Vec<Option<PathBuf>>,
}

impl Status {
    pub fn new(root: &Path) -> Status {
        Status {
            in_repo: project::is_repo(root),
            root: std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf()),
            entries: Vec::new(),
            set_gen: 0,
            generation: 0,
            inflight: false,
            stale: true,
            synced_gen: 0,
            synced: Vec::new(),
        }
    }

    /// Whether the sidebar carries a mark column. True for the whole
    /// repository rather than only for rows that have a mark, so a name
    /// never shifts a column sideways as marks come and go.
    pub fn in_repo(&self) -> bool {
        self.in_repo
    }

    /// The mark for a root-relative path, file or directory. One binary
    /// search and no allocation: the tree calls this per drawn row.
    pub fn mark(&self, rel: &str) -> Option<Mark> {
        self.entries
            .binary_search_by(|(p, _)| (**p).cmp(rel))
            .ok()
            .map(|i| self.entries[i].1)
    }

    /// The set may have moved; the next pump looks again. Clearing nothing
    /// keeps the current marks on screen until the new ones land.
    pub fn mark_stale(&mut self) {
        self.stale = true;
    }

    /// Starts a scan when one is owed. Called once at startup and at each
    /// refresh expiry — never from a frame, so a busy repository cannot
    /// turn into a repaint loop. One job at a time, so an event burst
    /// cannot stack threads.
    pub fn pump(&mut self, tx: &Sender<AppEvent>) {
        if !self.in_repo || self.inflight || !self.stale {
            return;
        }
        // Cleared at the spawn, not on arrival, so a save that lands
        // mid-scan is not swallowed by the reply to the question before it.
        self.stale = false;
        self.generation += 1;
        spawn_scan(self.root.clone(), self.generation, tx.clone());
        self.inflight = true;
    }

    /// Installs a finished scan if it is still the awaited one, then pumps
    /// again to cover whatever happened while it ran. Returns whether the
    /// marks changed — a re-scan finding the same set must not cost a
    /// repaint.
    pub fn absorb(&mut self, done: ScanDone, tx: &Sender<AppEvent>) -> bool {
        if !self.inflight || done.generation != self.generation {
            return false;
        }
        self.inflight = false;
        let entries = done.entries.unwrap_or_default();
        let changed = entries != self.entries;
        if changed {
            self.entries = entries;
            self.set_gen += 1;
        }
        self.pump(tx);
        changed
    }

    /// Catches every tab's label up with the set. Called each loop
    /// iteration so one call site covers open, close and save-as; almost
    /// always a no-op guarded by the set generation and a lexical compare
    /// of tab paths against the last sync. Returns whether a mark moved.
    /// Outside a repository it costs the one comparison below: there is no
    /// mark to give and no path worth resolving.
    pub fn sync(&mut self, tabs: &mut Tabs) -> bool {
        if !self.in_repo {
            return false;
        }
        if self.synced_gen == self.set_gen
            && self.synced.len() == tabs.count()
            && tabs
                .all()
                .iter()
                .zip(&self.synced)
                .all(|(tab, p)| tab.doc.path() == p.as_deref())
        {
            return false;
        }
        self.synced_gen = self.set_gen;
        self.synced = tabs
            .all()
            .iter()
            .map(|tab| tab.doc.path().map(Path::to_path_buf))
            .collect();
        let mut moved = false;
        for index in 0..tabs.count() {
            let mark = self.synced[index]
                .as_deref()
                .and_then(|path| self.relative(path))
                .and_then(|rel| self.mark(&rel));
            let tab = tabs.get_mut(index);
            moved |= tab.mark != mark;
            tab.mark = mark;
        }
        moved
    }

    /// A tab's path as the scan spells it; `None` for a file outside the
    /// project root, which no row and no scan entry can name.
    fn relative(&self, path: &Path) -> Option<String> {
        tabs::canonical(path)
            .strip_prefix(&self.root)
            .ok()
            .and_then(Path::to_str)
            .map(str::to_string)
    }

    /// A project with no repository behind it: every mark inert, the
    /// sidebar exactly as wide as it was before any of this existed.
    #[cfg(test)]
    pub fn test_plain() -> Status {
        Status {
            in_repo: false,
            ..Status::new(Path::new("."))
        }
    }

    /// A set standing at fixed marks, so a test can draw or sync them
    /// without a repository behind it.
    #[cfg(test)]
    pub fn test_marks(entries: Vec<(&str, Mark)>) -> Status {
        Status {
            in_repo: true,
            entries: entries
                .into_iter()
                .map(|(p, m)| (p.into(), m))
                .collect::<Vec<_>>(),
            ..Status::new(Path::new("."))
        }
    }
}

/// Runs `git status` and parses it on a worker: the process, the read and
/// the parse all stay off the event path. No cancel flag — like the HEAD
/// lookup there is no progress to poll, the work is bounded by the caps
/// above, and a superseded result is dropped by generation on arrival. The
/// thread owns no editor state and exits when the receiver is gone.
fn spawn_scan(root: PathBuf, generation: u64, tx: Sender<AppEvent>) {
    thread::spawn(move || {
        let _ = tx.send(AppEvent::Scanned(ScanDone {
            generation,
            entries: scan(&root),
        }));
    });
}

/// The project's marked files, or `None` when there is no answer: no `git`
/// on the machine, a failed run, or a changeset past the cap.
///
/// `--no-optional-locks` keeps `git` from refreshing and rewriting
/// `.git/index`, which the watch on the git directory would otherwise see —
/// a scan must not wake the editor that asked for it. `-uall` lists the
/// files inside an untracked directory rather than the directory alone, so
/// every new file gets its own row; `--no-renames` keeps each `-z` record a
/// single NUL-terminated field.
fn scan(root: &Path) -> Option<Vec<(Box<str>, Mark)>> {
    let mut child = diff::git(root)
        .arg("--no-optional-locks")
        .args(["status", "--porcelain", "-z", "-uall", "--no-renames"])
        .spawn()
        .ok()?;
    let mut out = Vec::new();
    // Capped rather than read whole, and the pipe closes before the wait:
    // an oversized listing leaves `git` writing into a closed pipe, which
    // ends it and fails the status below, instead of deadlocking us.
    let read = child
        .stdout
        .take()
        .map(|stdout| stdout.take(MAX_BYTES as u64 + 1).read_to_end(&mut out));
    let ok = child.wait().is_ok_and(|status| status.success());
    if !ok || !matches!(read, Some(Ok(_))) || out.len() > MAX_BYTES {
        return None;
    }
    parse(&out)
}

/// Porcelain v1 records — `XY <path>\0` — as sorted marks, ancestors
/// included. `None` past the file cap.
fn parse(out: &[u8]) -> Option<Vec<(Box<str>, Mark)>> {
    let mut entries: Vec<(Box<str>, Mark)> = Vec::new();
    let mut files = 0;
    for record in out.split(|&b| b == 0) {
        // "XY " plus at least one path byte.
        if record.len() < 4 {
            continue;
        }
        let (x, y) = (record[0], record[1]);
        // A file gone from the working tree has no row to mark and leaves
        // no ghost entry: staged deletion, unstaged deletion, added then
        // deleted, deleted by both.
        if y == b'D' || (x == b'D' && y == b' ') {
            continue;
        }
        let mark = if x == b'?' || x == b'A' {
            Mark::New
        } else {
            Mark::Changed
        };
        // Non-UTF-8 names are dropped, as the project walk drops them: no
        // tree row can spell one either.
        let Ok(path) = str::from_utf8(&record[3..]) else {
            continue;
        };
        files += 1;
        if files > MAX_FILES {
            return None;
        }
        // `git` always spells paths with `/`; the tree and the walk use the
        // platform's separator.
        let path = path.replace('/', MAIN_SEPARATOR_STR);
        for (i, b) in path.bytes().enumerate() {
            if b == MAIN_SEPARATOR as u8 {
                entries.push((path[..i].into(), mark));
            }
        }
        entries.push((path.into(), mark));
    }
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    // A directory is reached once per marked descendant; fold them.
    entries.dedup_by(|b, a| {
        if a.0 == b.0 {
            a.1 = a.1.merge(b.1);
            return true;
        }
        false
    });
    Some(entries)
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    use super::*;
    use crate::doc::Document;

    /// The parse of one `\0`-joined listing, as `(path, mark)` pairs.
    fn marks(records: &[&str]) -> Vec<(String, Mark)> {
        let joined = records.join("\0");
        parse(joined.as_bytes())
            .unwrap()
            .into_iter()
            .map(|(p, m)| (p.into_string(), m))
            .collect()
    }

    fn pair(path: &str, mark: Mark) -> (String, Mark) {
        (path.to_string(), mark)
    }

    #[test]
    fn an_untracked_file_is_new_and_a_modified_one_changed() {
        assert_eq!(
            marks(&["?? new.rs", " M mod.rs", "M  staged.rs", "MM both.rs"]),
            [
                pair("both.rs", Mark::Changed),
                pair("mod.rs", Mark::Changed),
                pair("new.rs", Mark::New),
                pair("staged.rs", Mark::Changed),
            ]
        );
    }

    #[test]
    fn a_file_added_to_the_index_is_new_not_changed() {
        // It exists now and HEAD has never seen it, whatever the index
        // says: staging is not a state this shows.
        assert_eq!(
            marks(&["A  added.rs", "AM touched.rs", "AA conflict.rs"]),
            [
                pair("added.rs", Mark::New),
                pair("conflict.rs", Mark::New),
                pair("touched.rs", Mark::New),
            ]
        );
    }

    #[test]
    fn a_file_gone_from_the_working_tree_leaves_no_ghost_entry() {
        assert_eq!(
            marks(&[
                " D unstaged.rs",
                "D  staged.rs",
                "AD added-then-gone.rs",
                "DD both.rs",
                "UD theirs.rs",
                " M kept.rs",
            ]),
            [pair("kept.rs", Mark::Changed)]
        );
    }

    #[test]
    fn an_unmerged_file_that_still_exists_is_changed() {
        assert_eq!(
            marks(&["UU ours.rs", "DU recreated.rs"]),
            [
                pair("ours.rs", Mark::Changed),
                pair("recreated.rs", Mark::Changed)
            ]
        );
    }

    #[test]
    fn a_directory_carries_the_fold_of_its_descendants() {
        // Wholly new reads new; a directory holding both reads changed, so
        // no change under a collapsed row is invisible.
        assert_eq!(
            marks(&["?? a/new/one.rs", "?? a/new/two.rs", " M a/old.rs"]),
            [
                pair("a", Mark::Changed),
                pair("a/new", Mark::New),
                pair("a/new/one.rs", Mark::New),
                pair("a/new/two.rs", Mark::New),
                pair("a/old.rs", Mark::Changed),
            ]
        );
    }

    #[test]
    fn a_stray_or_short_record_is_ignored() {
        assert_eq!(
            marks(&["", "??", "?? ", " M a.rs"]),
            [pair("a.rs", Mark::Changed)]
        );
    }

    #[test]
    fn a_listing_past_the_cap_reports_nothing() {
        let records: Vec<String> = (0..=MAX_FILES).map(|i| format!("?? f{i}.rs")).collect();
        let joined = records.join("\0");
        assert_eq!(parse(joined.as_bytes()), None);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_name_is_skipped_without_error() {
        let mut out = Vec::new();
        out.extend_from_slice(b"?? bad-\xff-name\0 M kept.rs");
        let found = parse(&out).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(&*found[0].0, "kept.rs");
    }

    #[test]
    fn a_lookup_answers_files_directories_and_misses() {
        let status = Status::test_marks(vec![
            ("a", Mark::Changed),
            ("a/b.rs", Mark::Changed),
            ("c.rs", Mark::New),
        ]);
        assert_eq!(status.mark("a"), Some(Mark::Changed));
        assert_eq!(status.mark("a/b.rs"), Some(Mark::Changed));
        assert_eq!(status.mark("c.rs"), Some(Mark::New));
        assert_eq!(status.mark("a/z.rs"), None);
        assert_eq!(status.mark(""), None);
    }

    fn done(generation: u64, entries: Option<Vec<(&str, Mark)>>) -> ScanDone {
        ScanDone {
            generation,
            entries: entries.map(|e| e.into_iter().map(|(p, m)| (p.into(), m)).collect()),
        }
    }

    #[test]
    fn a_root_outside_a_repository_never_runs_git() {
        let (tx, rx) = mpsc::channel();
        let mut status = Status::new(&std::env::temp_dir());
        assert!(!status.in_repo());
        status.pump(&tx);
        assert!(!status.inflight);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_superseded_result_is_dropped() {
        let (tx, _rx) = mpsc::channel();
        let mut status = Status::test_marks(vec![("a.rs", Mark::New)]);
        status.inflight = true;
        status.generation = 2;
        assert!(!status.absorb(done(1, Some(Vec::new())), &tx));
        assert_eq!(status.mark("a.rs"), Some(Mark::New));
        assert!(status.inflight, "the awaited scan is still out");
    }

    #[test]
    fn a_scan_matching_the_marks_on_screen_costs_no_repaint() {
        let (tx, _rx) = mpsc::channel();
        let mut status = Status::test_marks(vec![("a.rs", Mark::New)]);
        let before = status.set_gen;
        status.inflight = true;
        status.generation = 1;
        assert!(!status.absorb(done(1, Some(vec![("a.rs", Mark::New)])), &tx));
        assert_eq!(status.set_gen, before);

        status.inflight = true;
        status.generation = 2;
        assert!(status.absorb(done(2, Some(vec![("a.rs", Mark::Changed)])), &tx));
        assert_eq!(status.mark("a.rs"), Some(Mark::Changed));
        assert_eq!(status.set_gen, before + 1);
    }

    #[test]
    fn an_answerless_scan_clears_the_marks() {
        let (tx, _rx) = mpsc::channel();
        let mut status = Status::test_marks(vec![("a.rs", Mark::New)]);
        status.inflight = true;
        status.generation = 1;
        assert!(status.absorb(done(1, None), &tx));
        assert_eq!(status.mark("a.rs"), None);
    }

    #[test]
    fn a_change_during_a_scan_is_not_answered_by_that_scan() {
        let (tx, rx) = mpsc::channel();
        let mut status = Status::test_marks(Vec::new());
        status.stale = false; // the spawn cleared it
        status.inflight = true;
        status.generation = 1;
        status.mark_stale(); // a save landed while the scan was out
        status.absorb(done(1, Some(Vec::new())), &tx);
        // The re-pump inside `absorb` starts the follow-up scan itself.
        assert!(status.inflight);
        assert!(rx.try_recv().is_err(), "the scan is still running");
    }

    #[test]
    fn the_tab_sync_marks_by_path_and_is_a_guarded_no_op() {
        let dir = scratch_dir("status-sync");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "a").unwrap();
        std::fs::write(dir.join("b.rs"), "b").unwrap();
        let mut status = Status::test_marks(vec![("src/a.rs", Mark::Changed)]);
        status.root = std::fs::canonicalize(&dir).unwrap();

        let mut tabs = Tabs::new(vec![
            Document::open(dir.join("src/a.rs")).unwrap(),
            Document::open(dir.join("b.rs")).unwrap(),
            Document::empty(),
        ]);
        assert!(status.sync(&mut tabs));
        assert_eq!(tabs.all()[0].mark, Some(Mark::Changed));
        assert_eq!(tabs.all()[1].mark, None);
        assert_eq!(tabs.all()[2].mark, None);
        // Nothing moved: the guard answers without touching the filesystem.
        assert!(!status.sync(&mut tabs));

        tabs.activate(0);
        tabs.close_active();
        assert!(
            !status.sync(&mut tabs),
            "the surviving tabs keep their marks"
        );
        assert_eq!(tabs.all()[0].mark, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outside_a_repository_nothing_is_marked_or_resolved() {
        let mut status = Status::test_plain();
        let mut tabs = Tabs::new(vec![Document::from_str("a")]);
        assert!(!status.in_repo());
        assert!(!status.sync(&mut tabs));
        assert_eq!(tabs.all()[0].mark, None);
        assert_eq!(status.mark("a.rs"), None);
    }

    #[test]
    fn a_new_set_remarks_every_tab() {
        let dir = scratch_dir("status-resync");
        std::fs::write(dir.join("a.rs"), "a").unwrap();
        let mut status = Status::test_marks(Vec::new());
        status.root = std::fs::canonicalize(&dir).unwrap();
        let mut tabs = Tabs::new(vec![Document::open(dir.join("a.rs")).unwrap()]);
        assert!(!status.sync(&mut tabs));
        assert_eq!(tabs.all()[0].mark, None);

        let (tx, _rx) = mpsc::channel();
        status.inflight = true;
        status.generation = 1;
        assert!(status.absorb(done(1, Some(vec![("a.rs", Mark::New)])), &tx));
        assert!(status.sync(&mut tabs));
        assert_eq!(tabs.all()[0].mark, Some(Mark::New));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("connor-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A throwaway repository with one commit behind it, or `None` when
    /// this machine has no usable `git` — the same condition the feature
    /// degrades under, so the test degrades with it.
    fn scratch_repo(name: &str) -> Option<PathBuf> {
        let dir = scratch_dir(name);
        let run = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.email=t@example.com",
                    "-c",
                    "user.name=t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|s| s.success())
        };
        std::fs::create_dir_all(dir.join("src")).ok()?;
        std::fs::write(dir.join("src/mod.rs"), "a\n").ok()?;
        std::fs::write(dir.join("src/kept.rs"), "a\n").ok()?;
        std::fs::write(dir.join("gone.rs"), "a\n").ok()?;
        std::fs::write(dir.join(".gitignore"), "ignored/\n").ok()?;
        (run(&["init"]) && run(&["add", "-A"]) && run(&["commit", "-m", "in"])).then_some(dir)
    }

    #[test]
    fn a_real_repository_reads_back_its_changed_and_new_files() {
        let Some(dir) = scratch_repo("status-repo") else {
            return;
        };
        std::fs::write(dir.join("src/mod.rs"), "b\n").unwrap();
        std::fs::remove_file(dir.join("gone.rs")).unwrap();
        std::fs::create_dir_all(dir.join("new/deep")).unwrap();
        std::fs::write(dir.join("new/deep/fresh.rs"), "n\n").unwrap();
        std::fs::create_dir_all(dir.join("ignored")).unwrap();
        std::fs::write(dir.join("ignored/junk"), "j\n").unwrap();

        let found = scan(&dir).unwrap();
        let found: Vec<(&str, Mark)> = found.iter().map(|(p, m)| (&**p, *m)).collect();
        assert_eq!(
            found,
            [
                ("new", Mark::New),
                ("new/deep", Mark::New),
                ("new/deep/fresh.rs", Mark::New),
                ("src", Mark::Changed),
                ("src/mod.rs", Mark::Changed),
            ],
            "deleted, ignored and untouched files leave no entry"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
